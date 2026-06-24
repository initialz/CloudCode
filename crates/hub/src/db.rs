//! SQLite-backed persistence for accounts, audit, and session tracking.
//!
//! The hub used to keep accounts inline in `hub.toml` and audit events
//! in an append-only JSONL file. Both have moved to a SQLite db so the
//! admin UI can query, filter, and aggregate them. The JSONL audit log
//! is kept as a secondary sink (append-only durability + offline
//! analysis).
//!
//! Single-file db, WAL mode, ~8 connection pool. SQLite is plenty for
//! the kind of write rate a cloudcode hub sees (a few events per session,
//! maybe a few accounts).

use anyhow::{Context, Result};
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions};
use sqlx::{ConnectOptions, Row, SqlitePool};
use std::path::Path;
use std::str::FromStr;

#[derive(Clone)]
pub struct Db {
    pub pool: SqlitePool,
}

#[derive(Debug, Clone)]
pub struct DbAccount {
    pub name: String,
    pub real_name: Option<String>,
    pub token_hash: String,
    pub token_prefix: Option<String>,
    pub created_at: i64,
    pub disabled: bool,
    /// Per-account sandbox mode: "strict" / "permissive" / "off".
    /// Replaces the legacy `sandbox_enabled` bool. Defaults to "strict"
    /// for fresh installs; migration maps the legacy bool to "strict"
    /// (true) or "permissive" (false).
    pub sandbox_mode: String,
}

/// One row in the hub-managed workspace registry. The actual files
/// still live on the agent's disk; this is just the (account, agent,
/// name) binding the menu + OpenSession routing rely on.
#[derive(Debug, Clone)]
pub struct WorkspaceBinding {
    pub account: String,
    pub agent: String,
    pub name: String,
    pub created_at: i64,
}

/// Parse `(input, output, cache_creation, cache_read)` token counts from an
/// assistant message body's `$.message.usage`. Non-assistant rows return all
/// `None` (the token report only sums assistant rows). For an assistant row,
/// missing fields default to `0` so the stored value is non-NULL — which also
/// marks the row as "already extracted" for the backfill's `IS NULL` probe.
fn usage_tokens(kind: &str, body: &str) -> (Option<i64>, Option<i64>, Option<i64>, Option<i64>) {
    if kind != "assistant" {
        return (None, None, None, None);
    }
    let usage = serde_json::from_str::<serde_json::Value>(body)
        .ok()
        .and_then(|v| v.get("message").and_then(|m| m.get("usage")).cloned());
    let get = |k: &str| -> i64 {
        usage
            .as_ref()
            .and_then(|u| u.get(k))
            .and_then(|x| x.as_i64())
            .unwrap_or(0)
    };
    (
        Some(get("input_tokens")),
        Some(get("output_tokens")),
        Some(get("cache_creation_input_tokens")),
        Some(get("cache_read_input_tokens")),
    )
}

impl Db {
    pub async fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() && !parent.exists() {
                std::fs::create_dir_all(parent)
                    .with_context(|| format!("creating db dir {}", parent.display()))?;
            }
        }
        let dsn = format!("sqlite://{}", path.display());
        let opts = SqliteConnectOptions::from_str(&dsn)?
            .create_if_missing(true)
            .journal_mode(SqliteJournalMode::Wal)
            .busy_timeout(std::time::Duration::from_secs(5))
            // TEMP DIAGNOSTIC (v1.31.1): log every statement with its
            // elapsed time at INFO so a production log shows exactly which
            // SQL holds the write lock during a "database is locked" storm.
            // The lock holder shows up as the line with the large `elapsed`.
            // Revert once the culprit is identified — this is verbose.
            .log_statements(log::LevelFilter::Info)
            .log_slow_statements(log::LevelFilter::Warn, std::time::Duration::from_millis(500));
        let pool = SqlitePoolOptions::new()
            .max_connections(8)
            .connect_with(opts)
            .await
            .with_context(|| format!("opening sqlite at {}", path.display()))?;
        let db = Self { pool };
        db.run_migrations().await?;
        // Backfill precomputed token columns for pre-existing assistant rows
        // in the background, so an upgrade doesn't block boot on a full-table
        // json_extract pass. New rows are populated at insert time.
        db.spawn_token_backfill();
        // Refresh the dashboard rollup tables hourly (and once at boot), so
        // the daily charts read precomputed rows instead of scanning the
        // messages table on every load.
        db.spawn_stats_rollup();
        // Auto-delete data older than the configured retention once a day.
        db.spawn_auto_cleanup();
        Ok(db)
    }

    async fn run_migrations(&self) -> Result<()> {
        // Pre-step: drop any stale `workspaces` table left over from
        // the abandoned hub-canonical experiment on the feature
        // branch. Its schema didn't have the `agent` column, so the
        // CREATE INDEX statements below would fail with
        // "no such column: agent" on dbs that touched that branch.
        // The new shape is purely a name→agent binding (no canonical
        // file content), so the old rows wouldn't be meaningful here
        // anyway — agent Hello reseeds the table on connect.
        let cols = sqlx::query("PRAGMA table_info(workspaces)")
            .fetch_all(&self.pool)
            .await
            .ok()
            .unwrap_or_default();
        let table_exists = !cols.is_empty();
        let has_agent_col = cols.iter().any(|r| {
            let name: String = r.try_get("name").unwrap_or_default();
            name == "agent"
        });
        if table_exists && !has_agent_col {
            tracing::info!("dropping legacy workspaces table (schema predates v1.13 binding model)");
            sqlx::query("DROP TABLE workspaces")
                .execute(&self.pool)
                .await
                .context("drop legacy workspaces table")?;
        }

        // No external migration tool — hub owns its schema. Each statement
        // is idempotent (`IF NOT EXISTS`) so an existing db just gets the
        // new objects on upgrade.
        let stmts = [
            "CREATE TABLE IF NOT EXISTS accounts (
                name            TEXT PRIMARY KEY,
                token_hash      TEXT NOT NULL,
                token_prefix    TEXT,
                created_at      INTEGER NOT NULL,
                disabled        INTEGER NOT NULL DEFAULT 0,
                sandbox_enabled INTEGER NOT NULL DEFAULT 1
            )",
            // Idempotent ALTER for deployments that pre-date the
            // sandbox_enabled column. SQLite errors on duplicate
            // column; the next statement swallows that case via
            // the marker check below.
            "ALTER TABLE accounts ADD COLUMN sandbox_enabled INTEGER NOT NULL DEFAULT 1",
            "ALTER TABLE accounts ADD COLUMN real_name TEXT",
            // sandbox_mode replaces sandbox_enabled (kept for back-compat).
            // Migration: legacy true → "strict", false → "permissive".
            "ALTER TABLE accounts ADD COLUMN sandbox_mode TEXT NOT NULL DEFAULT 'strict'",
            "UPDATE accounts SET sandbox_mode = 'permissive' WHERE sandbox_mode = 'strict' AND sandbox_enabled = 0",
            "CREATE TABLE IF NOT EXISTS audit_events (
                id         INTEGER PRIMARY KEY AUTOINCREMENT,
                ts         INTEGER NOT NULL,
                kind       TEXT NOT NULL,
                account    TEXT,
                agent      TEXT,
                session_id TEXT,
                workspace  TEXT,
                detail     TEXT
            )",
            "CREATE INDEX IF NOT EXISTS idx_audit_ts ON audit_events(ts DESC)",
            "CREATE INDEX IF NOT EXISTS idx_audit_account ON audit_events(account)",
            "CREATE INDEX IF NOT EXISTS idx_audit_kind ON audit_events(kind)",
            "CREATE TABLE IF NOT EXISTS sessions (
                session_id  TEXT PRIMARY KEY,
                account     TEXT NOT NULL,
                agent       TEXT NOT NULL,
                workspace   TEXT NOT NULL,
                started_at  INTEGER NOT NULL,
                ended_at    INTEGER,
                ended_reason TEXT
            )",
            "CREATE INDEX IF NOT EXISTS idx_sessions_started ON sessions(started_at DESC)",
            "CREATE INDEX IF NOT EXISTS idx_sessions_active ON sessions(ended_at) WHERE ended_at IS NULL",
            // Conversation messages tailed from claude's per-project
            // jsonl logs. One row per JSONL line; `kind` is the outer
            // `type` field (user / assistant / permission-mode / ...);
            // `body` is the raw line as JSON.
            // Token columns are precomputed from `body.$.message.usage` at
            // insert time (assistant rows only; NULL elsewhere) so the daily
            // token report is a plain integer SUM instead of a json_extract
            // over every body — see `usage_tokens` + the assistant token
            // index below. Existing dbs get these via an ALTER in
            // `migrate_message_tokens`.
            "CREATE TABLE IF NOT EXISTS messages (
                id                    INTEGER PRIMARY KEY AUTOINCREMENT,
                cc_session_id         TEXT NOT NULL,
                claude_session_id     TEXT NOT NULL,
                ts                    INTEGER NOT NULL,
                kind                  TEXT NOT NULL,
                body                  TEXT NOT NULL,
                input_tokens          INTEGER,
                output_tokens         INTEGER,
                cache_creation_tokens INTEGER,
                cache_read_tokens     INTEGER
            )",
            "CREATE INDEX IF NOT EXISTS idx_messages_cc_session ON messages(cc_session_id, id)",
            "CREATE INDEX IF NOT EXISTS idx_messages_claude_session ON messages(claude_session_id, id)",
            "CREATE INDEX IF NOT EXISTS idx_messages_ts ON messages(ts DESC)",
            // Covering index for the daily message-count report
            // (`WHERE ts>=? GROUP BY day, kind`): ts + kind both come from
            // the index, so COUNT/GROUP never touches the table.
            "CREATE INDEX IF NOT EXISTS idx_messages_ts_kind ON messages(ts, kind)",
            // Precomputed token columns for existing dbs (fresh dbs get them
            // in CREATE TABLE above). The migration loop tolerates the
            // "duplicate column name" error these throw once applied.
            "ALTER TABLE messages ADD COLUMN input_tokens INTEGER",
            "ALTER TABLE messages ADD COLUMN output_tokens INTEGER",
            "ALTER TABLE messages ADD COLUMN cache_creation_tokens INTEGER",
            "ALTER TABLE messages ADD COLUMN cache_read_tokens INTEGER",
            // Partial covering index for the daily token report
            // (`SUM(input_tokens)… WHERE ts>=? AND kind='assistant'`): all
            // summed columns live in the index, restricted to assistant
            // rows, so the query is index-only — no body reads, no
            // json_extract. This is the fix for the slow/502 tokens-daily.
            "CREATE INDEX IF NOT EXISTS idx_messages_assistant_tokens
                 ON messages(ts, input_tokens, output_tokens, cache_creation_tokens, cache_read_tokens)
                 WHERE kind = 'assistant'",
            // Precomputed daily rollups for the admin dashboard. A background
            // task (spawn_stats_rollup) refreshes these hourly so the daily
            // charts read a few dozen rows instead of scanning millions of
            // message rows on every load. One row per UTC day; days with no
            // activity are simply absent (the API fills zeros).
            "CREATE TABLE IF NOT EXISTS stats_daily (
                day                   TEXT PRIMARY KEY,
                user_msgs             INTEGER NOT NULL DEFAULT 0,
                assistant_msgs        INTEGER NOT NULL DEFAULT 0,
                other_msgs            INTEGER NOT NULL DEFAULT 0,
                input_tokens          INTEGER NOT NULL DEFAULT 0,
                output_tokens         INTEGER NOT NULL DEFAULT 0,
                cache_creation_tokens INTEGER NOT NULL DEFAULT 0,
                cache_read_tokens     INTEGER NOT NULL DEFAULT 0,
                computed_at           INTEGER NOT NULL
            )",
            // Per-(day, account, agent) message counts feeding the
            // leaderboard's message totals — the previously slow
            // sessions⋈messages JOIN. Refreshed hourly for the recent
            // window only (the leaderboard never looks back past 30 days).
            "CREATE TABLE IF NOT EXISTS stats_msg_by_session_daily (
                day      TEXT NOT NULL,
                account  TEXT NOT NULL,
                agent    TEXT NOT NULL,
                msgs     INTEGER NOT NULL DEFAULT 0,
                PRIMARY KEY (day, account, agent)
            )",
            // Track each jsonl file's byte offset so the agent can resume
            // tailing where it left off after a restart. Keyed on the
            // claude session id (== filename without .jsonl); the cc
            // session that first saw it is recorded for routing.
            "CREATE TABLE IF NOT EXISTS jsonl_progress (
                claude_session_id TEXT PRIMARY KEY,
                cc_session_id     TEXT NOT NULL,
                offset            INTEGER NOT NULL,
                updated_at        INTEGER NOT NULL
            )",
            // Per-account whitelist of agents this account may connect to.
            // Semantics: strict whitelist — a row must exist for the
            // (account, agent) pair, otherwise the account is denied.
            // First-run seed (below) grants each pre-existing account
            // every agent it had historically connected to (derived from
            // sessions), so v0.9 upgrades didn't lock anyone out.
            "CREATE TABLE IF NOT EXISTS account_allowed_agents (
                account TEXT NOT NULL,
                agent   TEXT NOT NULL,
                PRIMARY KEY (account, agent)
            )",
            // Key-value scratchpad for migrations that need to run
            // exactly once across the lifetime of the database. Without
            // this table the ACL seed below would re-run on every hub
            // restart and resurrect rows the admin had explicitly
            // deleted from the UI.
            "CREATE TABLE IF NOT EXISTS db_meta (
                key   TEXT PRIMARY KEY,
                value TEXT
            )",
            // Per-account opaque JSON blob holding webterm UI preferences
            // (default args per tool, future things). The hub does not
            // interpret `data`; webterm owns the schema and migrates the
            // blob in code as it evolves. One row per account.
            "CREATE TABLE IF NOT EXISTS user_preferences (
                account    TEXT PRIMARY KEY,
                data       TEXT NOT NULL,
                updated_at INTEGER NOT NULL
            )",
            // Persistent admin / webterm session stores. Previously
            // both sat in in-process DashMaps and got wiped on every
            // hub restart, forcing operators + webterm users to log
            // back in after each upgrade. Now the only thing
            // restart-sensitive is the cookie's expiration timestamp.
            "CREATE TABLE IF NOT EXISTS admin_sessions (
                sid        TEXT PRIMARY KEY,
                expires_at INTEGER NOT NULL
            )",
            "CREATE INDEX IF NOT EXISTS idx_admin_sessions_expires ON admin_sessions(expires_at)",
            "CREATE TABLE IF NOT EXISTS user_sessions (
                sid        TEXT PRIMARY KEY,
                account    TEXT NOT NULL,
                expires_at INTEGER NOT NULL
            )",
            "CREATE INDEX IF NOT EXISTS idx_user_sessions_expires ON user_sessions(expires_at)",
            // Hub-managed binding of workspace name → owning agent.
            // The actual workspace files still live on the agent's
            // disk (v1.12 model); this table just records "user X's
            // workspace Y belongs to agent Z" so the menu can show
            // one cross-agent list and the open path knows which
            // agent to route to. PK is (account, agent, name) so
            // two agents can each have a workspace called "proj" —
            // the UI disambiguates by showing the agent suffix
            // when the bare name would collide.
            "CREATE TABLE IF NOT EXISTS workspaces (
                account    TEXT NOT NULL,
                agent      TEXT NOT NULL,
                name       TEXT NOT NULL,
                created_at INTEGER NOT NULL,
                PRIMARY KEY (account, agent, name)
            )",
            "CREATE INDEX IF NOT EXISTS idx_workspaces_account ON workspaces(account)",
            "CREATE INDEX IF NOT EXISTS idx_workspaces_agent ON workspaces(agent)",
            // Captured user inputs (prompts + bash escapes) from claude's
            // per-project jsonl logs, shipped by the agent's
            // `audit.rs` pipeline. Separate from `messages` because the
            // semantics differ: `messages` is the conversation feed,
            // `user_interactions` is the audit feed (filtered to human
            // input, exposed via the admin UI with explicit "reveal").
            // The UNIQUE constraint is the agent's restart-resume
            // dedupe: re-tailing the same jsonl file from offset 0
            // would otherwise duplicate every row.
            "CREATE TABLE IF NOT EXISTS user_interactions (
                id                 INTEGER PRIMARY KEY AUTOINCREMENT,
                account            TEXT NOT NULL,
                agent              TEXT NOT NULL,
                workspace          TEXT NOT NULL,
                claude_session_id  TEXT NOT NULL,
                prompt_id          TEXT,
                parent_uuid        TEXT,
                cwd                TEXT,
                git_branch         TEXT,
                ts_ms              INTEGER NOT NULL,
                kind               TEXT NOT NULL,
                content            TEXT NOT NULL,
                UNIQUE(claude_session_id, parent_uuid, ts_ms)
            )",
            "CREATE INDEX IF NOT EXISTS idx_ui_account_ts ON user_interactions(account, ts_ms DESC)",
            "CREATE INDEX IF NOT EXISTS idx_ui_workspace_ts ON user_interactions(workspace, ts_ms DESC)",
            "CREATE INDEX IF NOT EXISTS idx_ui_session ON user_interactions(claude_session_id)",
            // Invite links — admin-issued shareable URLs that mint a
            // brand-new account on accept. `token` is the secret in
            // the public URL (`/invite/<token>`); `id` is the short
            // admin-side handle. `allowed_agents` is a comma-separated
            // string (simple to query, no schema for a join table —
            // expected to be a handful of agents per invite). `used`
            // tracks accept count for max_uses enforcement; `active`
            // is the admin's soft disable.
            "CREATE TABLE IF NOT EXISTS invite_links (
                id             TEXT PRIMARY KEY,
                label          TEXT,
                token          TEXT NOT NULL UNIQUE,
                max_uses       INTEGER NOT NULL DEFAULT 0,
                used           INTEGER NOT NULL DEFAULT 0,
                allowed_agents TEXT NOT NULL DEFAULT '',
                active         INTEGER NOT NULL DEFAULT 1,
                created_at     INTEGER NOT NULL,
                created_by     TEXT NOT NULL
            )",
            "CREATE INDEX IF NOT EXISTS idx_invite_links_token ON invite_links(token)",
            // sandbox_mode applied to accounts created via this invite.
            "ALTER TABLE invite_links ADD COLUMN sandbox_mode TEXT NOT NULL DEFAULT 'strict'",
            // Audit trail of which accounts were minted by which invite.
            // PK is (invite_id, account) so the same account can never
            // be linked to one invite twice (defensive; the accept
            // handler refuses duplicate usernames anyway).
            "CREATE TABLE IF NOT EXISTS invite_acceptances (
                invite_id   TEXT NOT NULL,
                account     TEXT NOT NULL,
                accepted_at INTEGER NOT NULL,
                PRIMARY KEY (invite_id, account)
            )",
            "CREATE INDEX IF NOT EXISTS idx_invite_acceptances_invite ON invite_acceptances(invite_id)",
            // Compat for deployments that already ran the unguarded
            // seed (pre-v1.8.x): if the ACL table is non-empty, assume
            // the seed has logically happened and lock the marker in,
            // so the WHERE NOT EXISTS guard below short-circuits and
            // we don't undelete anything on the next start.
            "INSERT OR IGNORE INTO db_meta (key, value)
                SELECT 'seeded_acl_v0_9', '1'
                 WHERE EXISTS (SELECT 1 FROM account_allowed_agents)",
            // Fresh deployments: actually run the seed. Guarded so it
            // only happens once.
            "INSERT OR IGNORE INTO account_allowed_agents (account, agent)
                SELECT DISTINCT s.account, s.agent
                  FROM sessions s
                  JOIN accounts a ON a.name = s.account
                 WHERE NOT EXISTS (
                     SELECT 1 FROM db_meta WHERE key = 'seeded_acl_v0_9'
                 )",
            // Lock the marker in unconditionally — even for fresh dbs
            // with zero historical sessions, so the seed doesn't try
            // again once the admin starts using the system.
            "INSERT OR IGNORE INTO db_meta (key, value)
                VALUES ('seeded_acl_v0_9', '1')",
        ];
        for sql in stmts {
            let res = sqlx::query(sql).execute(&self.pool).await;
            if let Err(e) = res {
                // Idempotent ALTER TABLE: SQLite returns "duplicate
                // column name" when the column already exists. Treat
                // that as success so re-running migrations on an
                // already-upgraded db is a no-op.
                let msg = e.to_string();
                if msg.contains("duplicate column name") {
                    continue;
                }
                return Err(e).with_context(|| {
                    format!(
                        "migrate: {}",
                        sql.split_whitespace().take(4).collect::<Vec<_>>().join(" ")
                    )
                });
            }
        }
        Ok(())
    }

    /// One-time backfill of the precomputed token columns for assistant rows
    /// that predate the column migration. Runs in the background, newest-first
    /// (so the dashboard's recent window goes accurate first), in small paced
    /// batches so we never hold a long write lock that would stall agents
    /// shipping new messages. `COALESCE(..., 0)` guarantees every processed
    /// row becomes non-NULL — including assistant rows that carry no usage —
    /// so the `input_tokens IS NULL` cursor always makes progress and the loop
    /// terminates.
    fn spawn_token_backfill(&self) {
        let pool = self.pool.clone();
        tokio::spawn(async move {
            let mut total: u64 = 0;
            loop {
                let res = sqlx::query(
                    "UPDATE messages
                        SET input_tokens          = COALESCE(CAST(json_extract(body,'$.message.usage.input_tokens') AS INTEGER), 0),
                            output_tokens         = COALESCE(CAST(json_extract(body,'$.message.usage.output_tokens') AS INTEGER), 0),
                            cache_creation_tokens = COALESCE(CAST(json_extract(body,'$.message.usage.cache_creation_input_tokens') AS INTEGER), 0),
                            cache_read_tokens     = COALESCE(CAST(json_extract(body,'$.message.usage.cache_read_input_tokens') AS INTEGER), 0)
                      WHERE id IN (
                          SELECT id FROM messages
                           WHERE kind = 'assistant' AND input_tokens IS NULL
                           ORDER BY id DESC
                           LIMIT 2000)",
                )
                .execute(&pool)
                .await;
                match res {
                    Ok(r) => {
                        let n = r.rows_affected();
                        total += n;
                        if n == 0 {
                            if total > 0 {
                                tracing::info!(rows = total, "message token backfill complete");
                            }
                            break;
                        }
                        // Yield to writers between batches.
                        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "token backfill batch failed; will retry next boot");
                        break;
                    }
                }
            }
        });
    }

    // ---- key/value meta + settings ------------------------------------

    /// Read a `db_meta` value by key. Returns None if absent.
    pub async fn get_meta(&self, key: &str) -> Result<Option<String>> {
        let row = sqlx::query("SELECT value FROM db_meta WHERE key = ?1")
            .bind(key)
            .fetch_optional(&self.pool)
            .await?;
        Ok(row.map(|r| r.get::<String, _>("value")))
    }

    /// Upsert a `db_meta` value.
    pub async fn set_meta(&self, key: &str, value: &str) -> Result<()> {
        sqlx::query(
            "INSERT INTO db_meta (key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        )
        .bind(key)
        .bind(value)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    // ---- dashboard rollups --------------------------------------------

    /// Recompute the per-day message/token rollup for all days touched by
    /// rows with `ts >= since_secs`. INSERT OR REPLACE keeps it idempotent;
    /// a full pass (since = 0) backfills history. Indexed range scan on ts.
    pub async fn refresh_stats_daily(&self, since_secs: i64, now_secs: i64) -> Result<()> {
        sqlx::query(
            "INSERT OR REPLACE INTO stats_daily
                 (day, user_msgs, assistant_msgs, other_msgs,
                  input_tokens, output_tokens, cache_creation_tokens, cache_read_tokens,
                  computed_at)
             SELECT date(ts,'unixepoch') AS day,
                    SUM(kind = 'user'),
                    SUM(kind = 'assistant'),
                    SUM(kind NOT IN ('user','assistant')),
                    SUM(CASE WHEN kind='assistant' THEN COALESCE(input_tokens,0) ELSE 0 END),
                    SUM(CASE WHEN kind='assistant' THEN COALESCE(output_tokens,0) ELSE 0 END),
                    SUM(CASE WHEN kind='assistant' THEN COALESCE(cache_creation_tokens,0) ELSE 0 END),
                    SUM(CASE WHEN kind='assistant' THEN COALESCE(cache_read_tokens,0) ELSE 0 END),
                    ?2
               FROM messages
              WHERE ts >= ?1
              GROUP BY day",
        )
        .bind(since_secs)
        .bind(now_secs)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Rebuild the leaderboard's per-(day, account, agent) message counts for
    /// the recent window. The leaderboard never looks past 30 days, so we
    /// recompute the last `window_days` days wholesale each run and drop
    /// anything older — keeping the table tiny and always fresh.
    pub async fn refresh_msg_by_session_daily(&self, window_days: i64) -> Result<()> {
        let cutoff = chrono::Utc::now().timestamp() - window_days * 86_400;
        let cutoff_day = chrono::DateTime::<chrono::Utc>::from_timestamp(cutoff, 0)
            .map(|d| d.format("%Y-%m-%d").to_string())
            .unwrap_or_default();
        // Recompute the window, then prune days that fell out of it.
        sqlx::query(
            "INSERT OR REPLACE INTO stats_msg_by_session_daily (day, account, agent, msgs)
             SELECT date(m.ts,'unixepoch') AS day, s.account, s.agent, COUNT(*)
               FROM messages m
               JOIN sessions s ON s.session_id = m.cc_session_id
              WHERE m.ts >= ?1
              GROUP BY day, s.account, s.agent",
        )
        .bind(cutoff)
        .execute(&self.pool)
        .await?;
        sqlx::query("DELETE FROM stats_msg_by_session_daily WHERE day < ?1")
            .bind(&cutoff_day)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Background loop: backfill the daily rollup once (full history), then
    /// refresh the recent window + leaderboard rollup every hour. Cheap —
    /// the hourly pass is a 2-day indexed range + a 31-day join, off the
    /// request path.
    fn spawn_stats_rollup(&self) {
        let db = Self { pool: self.pool.clone() };
        tokio::spawn(async move {
            // One-time full backfill of stats_daily on first boot after the
            // feature ships. Marked in db_meta so later boots skip it.
            if db.get_meta("stats_daily_backfilled").await.ok().flatten().is_none() {
                let now = chrono::Utc::now().timestamp();
                if let Err(e) = db.refresh_stats_daily(0, now).await {
                    tracing::warn!(error = %e, "stats_daily backfill failed; will retry next boot");
                } else {
                    let _ = db.set_meta("stats_daily_backfilled", "1").await;
                    tracing::info!("stats_daily backfill complete");
                }
            }
            loop {
                let now = chrono::Utc::now().timestamp();
                // Recent days may still be accumulating; recompute the last
                // 2 days each pass to absorb late-arriving messages.
                if let Err(e) = db.refresh_stats_daily(now - 2 * 86_400, now).await {
                    tracing::warn!(error = %e, "stats_daily refresh failed");
                }
                if let Err(e) = db.refresh_msg_by_session_daily(31).await {
                    tracing::warn!(error = %e, "leaderboard rollup refresh failed");
                }
                tokio::time::sleep(std::time::Duration::from_secs(3600)).await;
            }
        });
    }

    /// Background loop: once a day, if auto-cleanup is enabled in settings,
    /// delete time-series rows older than the configured retention. DELETE
    /// only (no VACUUM) so it never needs scratch disk or a long exclusive
    /// lock — space is reused by later writes.
    fn spawn_auto_cleanup(&self) {
        let db = Self { pool: self.pool.clone() };
        tokio::spawn(async move {
            loop {
                let enabled = db
                    .get_meta("cleanup_enabled")
                    .await
                    .ok()
                    .flatten()
                    .map(|v| v == "1")
                    .unwrap_or(false);
                let months = db
                    .get_meta("cleanup_retention_months")
                    .await
                    .ok()
                    .flatten()
                    .and_then(|v| v.parse::<u32>().ok())
                    .unwrap_or(1);
                if enabled {
                    if let Some(cutoff) = chrono::Utc::now()
                        .checked_sub_months(chrono::Months::new(months))
                        .map(|d| d.timestamp())
                    {
                        match db.cleanup_delete_batched(cutoff).await {
                            Ok(c) if c.messages + c.sessions + c.user_interactions > 0 => {
                                tracing::info!(
                                    months,
                                    messages = c.messages,
                                    sessions = c.sessions,
                                    user_interactions = c.user_interactions,
                                    "auto-cleanup deleted old data"
                                );
                            }
                            Ok(_) => {}
                            Err(e) => tracing::warn!(error = %e, "auto-cleanup failed"),
                        }
                    }
                }
                tokio::time::sleep(std::time::Duration::from_secs(24 * 3600)).await;
            }
        });
    }

    // ---- accounts ------------------------------------------------------

    pub async fn account_count(&self) -> Result<i64> {
        let row = sqlx::query("SELECT COUNT(*) AS n FROM accounts")
            .fetch_one(&self.pool)
            .await?;
        Ok(row.get::<i64, _>("n"))
    }

    /// Per-account activity rollup: most recent session start (or
    /// None if the account has never opened one) and how many of
    /// its sessions are currently live (ended_at IS NULL). One SQL
    /// round trip; admin UI uses it next to list_accounts() to
    /// render Online / Last used columns without N+1ing.
    pub async fn account_activity_index(
        &self,
    ) -> Result<Vec<(String, Option<i64>, i64)>> {
        let rows = sqlx::query(
            "SELECT account,
                    MAX(started_at) AS last_used,
                    SUM(CASE WHEN ended_at IS NULL THEN 1 ELSE 0 END) AS active_count
               FROM sessions
              GROUP BY account",
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|r| {
                (
                    r.get::<String, _>("account"),
                    r.get::<Option<i64>, _>("last_used"),
                    r.get::<Option<i64>, _>("active_count").unwrap_or(0),
                )
            })
            .collect())
    }

    /// Look up a single account by name. Used by the username+token
    /// login path so we don't pay the O(N) argon2 verify of
    /// `authenticate()`. `Ok(None)` means no such name.
    pub async fn get_account(&self, name: &str) -> Result<Option<DbAccount>> {
        let row = sqlx::query(
            "SELECT name, token_hash, token_prefix, created_at, disabled, sandbox_mode, real_name
             FROM accounts WHERE name = ?1",
        )
        .bind(name)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(|r| DbAccount {
            name: r.get("name"),
            real_name: r.get("real_name"),
            token_hash: r.get("token_hash"),
            token_prefix: r.get("token_prefix"),
            created_at: r.get("created_at"),
            disabled: r.get::<i64, _>("disabled") != 0,
            sandbox_mode: r.get("sandbox_mode"),
        }))
    }

    pub async fn list_accounts(&self) -> Result<Vec<DbAccount>> {
        let rows = sqlx::query(
            "SELECT name, token_hash, token_prefix, created_at, disabled, sandbox_mode, real_name
             FROM accounts ORDER BY name",
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|r| DbAccount {
                name: r.get("name"),
                real_name: r.get("real_name"),
                token_hash: r.get("token_hash"),
                token_prefix: r.get("token_prefix"),
                created_at: r.get("created_at"),
                disabled: r.get::<i64, _>("disabled") != 0,
                sandbox_mode: r.get("sandbox_mode"),
            })
            .collect())
    }

    /// Look up a single account's sandbox mode. Defaults to "strict"
    /// if missing — err on the side of more isolation.
    pub async fn account_sandbox_mode(&self, name: &str) -> Result<String> {
        let row = sqlx::query(
            "SELECT sandbox_mode FROM accounts WHERE name = ?1",
        )
        .bind(name)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row
            .map(|r| r.get::<String, _>("sandbox_mode"))
            .unwrap_or_else(|| "strict".to_string()))
    }

    pub async fn set_account_sandbox_mode(&self, name: &str, mode: &str) -> Result<()> {
        if !matches!(mode, "strict" | "permissive" | "off") {
            anyhow::bail!("invalid sandbox mode '{}'", mode);
        }
        let rows = sqlx::query("UPDATE accounts SET sandbox_mode = ?1 WHERE name = ?2")
            .bind(mode)
            .bind(name)
            .execute(&self.pool)
            .await?
            .rows_affected();
        if rows == 0 {
            anyhow::bail!("account '{}' not found", name);
        }
        Ok(())
    }

    pub async fn insert_account(
        &self,
        name: &str,
        token_hash: &str,
        token_prefix: Option<&str>,
        real_name: Option<&str>,
    ) -> Result<()> {
        sqlx::query(
            "INSERT INTO accounts (name, token_hash, token_prefix, created_at, disabled, real_name)
             VALUES (?1, ?2, ?3, ?4, 0, ?5)",
        )
        .bind(name)
        .bind(token_hash)
        .bind(token_prefix)
        .bind(chrono::Utc::now().timestamp())
        .bind(real_name)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn account_exists(&self, name: &str) -> Result<bool> {
        let row = sqlx::query("SELECT 1 AS one FROM accounts WHERE name = ?1")
            .bind(name)
            .fetch_optional(&self.pool)
            .await?;
        Ok(row.is_some())
    }

    pub async fn update_account_token(
        &self,
        name: &str,
        token_hash: &str,
        token_prefix: &str,
    ) -> Result<()> {
        let rows = sqlx::query(
            "UPDATE accounts SET token_hash = ?1, token_prefix = ?2 WHERE name = ?3",
        )
        .bind(token_hash)
        .bind(token_prefix)
        .bind(name)
        .execute(&self.pool)
        .await?
        .rows_affected();
        if rows == 0 {
            anyhow::bail!("account '{}' not found", name);
        }
        Ok(())
    }

    pub async fn set_account_disabled(&self, name: &str, disabled: bool) -> Result<()> {
        let rows = sqlx::query("UPDATE accounts SET disabled = ?1 WHERE name = ?2")
            .bind(if disabled { 1_i64 } else { 0_i64 })
            .bind(name)
            .execute(&self.pool)
            .await?
            .rows_affected();
        if rows == 0 {
            anyhow::bail!("account '{}' not found", name);
        }
        Ok(())
    }

    pub async fn update_account_real_name(&self, name: &str, real_name: Option<&str>) -> Result<()> {
        let rows = sqlx::query("UPDATE accounts SET real_name = ?1 WHERE name = ?2")
            .bind(real_name)
            .bind(name)
            .execute(&self.pool)
            .await?
            .rows_affected();
        if rows == 0 {
            anyhow::bail!("account '{}' not found", name);
        }
        Ok(())
    }

    pub async fn delete_account(&self, name: &str) -> Result<()> {
        // No SQLite foreign keys here, so we walk the dependent
        // tables ourselves. Drop the ACL rows first so a partial
        // failure still leaves the world in a consistent state
        // (orphan ACL rows are worse than orphan audit rows —
        // sessions history is meant to outlive the account).
        let mut tx = self.pool.begin().await?;
        sqlx::query("DELETE FROM account_allowed_agents WHERE account = ?1")
            .bind(name)
            .execute(&mut *tx)
            .await?;
        let rows = sqlx::query("DELETE FROM accounts WHERE name = ?1")
            .bind(name)
            .execute(&mut *tx)
            .await?
            .rows_affected();
        if rows == 0 {
            anyhow::bail!("account '{}' not found", name);
        }
        tx.commit().await?;
        Ok(())
    }

    // ---- workspaces (hub-managed binding) -----------------------------

    /// Record that `account`'s `name` workspace lives on `agent`.
    /// `INSERT OR IGNORE` semantics: a duplicate `(account, agent,
    /// name)` is a no-op (used by the agent-Hello seeding path).
    /// Returns true if a new row was inserted, false if the binding
    /// already existed.
    pub async fn upsert_workspace_binding(
        &self,
        account: &str,
        agent: &str,
        name: &str,
    ) -> Result<bool> {
        let r = sqlx::query(
            "INSERT OR IGNORE INTO workspaces (account, agent, name, created_at)
             VALUES (?1, ?2, ?3, ?4)",
        )
        .bind(account)
        .bind(agent)
        .bind(name)
        .bind(chrono::Utc::now().timestamp())
        .execute(&self.pool)
        .await?;
        Ok(r.rows_affected() > 0)
    }

    /// Strict insert — fails if the same `(account, agent, name)`
    /// already exists. Use for user-driven create flows so a UI
    /// typo doesn't silently no-op.
    pub async fn insert_workspace_binding(
        &self,
        account: &str,
        agent: &str,
        name: &str,
    ) -> Result<()> {
        sqlx::query(
            "INSERT INTO workspaces (account, agent, name, created_at)
             VALUES (?1, ?2, ?3, ?4)",
        )
        .bind(account)
        .bind(agent)
        .bind(name)
        .bind(chrono::Utc::now().timestamp())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn delete_workspace_binding(
        &self,
        account: &str,
        agent: &str,
        name: &str,
    ) -> Result<u64> {
        let r = sqlx::query(
            "DELETE FROM workspaces WHERE account = ?1 AND agent = ?2 AND name = ?3",
        )
        .bind(account)
        .bind(agent)
        .bind(name)
        .execute(&self.pool)
        .await?;
        Ok(r.rows_affected())
    }

    /// Every workspace this account owns, across all agents. Order
    /// is `(agent, name)` so the picker's grouping stays predictable
    /// across redraws.
    pub async fn list_workspaces_for_account(
        &self,
        account: &str,
    ) -> Result<Vec<WorkspaceBinding>> {
        let rows = sqlx::query(
            "SELECT account, agent, name, created_at
               FROM workspaces
              WHERE account = ?1
              ORDER BY agent, name",
        )
        .bind(account)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|r| WorkspaceBinding {
                account: r.get("account"),
                agent: r.get("agent"),
                name: r.get("name"),
                created_at: r.get("created_at"),
            })
            .collect())
    }

    /// Every workspace binding in the table, across all accounts and
    /// agents. Used by the admin inventory page so it can surface
    /// bindings whose owning agent happens to be offline at request
    /// time (without falling back to reconstructing the set from the
    /// sessions table — which would resurrect rows the admin just
    /// deleted).
    pub async fn list_all_workspace_bindings(&self) -> Result<Vec<WorkspaceBinding>> {
        let rows = sqlx::query(
            "SELECT account, agent, name, created_at
               FROM workspaces
              ORDER BY agent, account, name",
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|r| WorkspaceBinding {
                account: r.get("account"),
                agent: r.get("agent"),
                name: r.get("name"),
                created_at: r.get("created_at"),
            })
            .collect())
    }

    /// Look up the owning agent for a given workspace name. Used by
    /// OpenSession to route to the right agent without the client
    /// having to specify it on every call. Returns `Ok(None)` if no
    /// such binding exists (e.g. the workspace was deleted between
    /// the menu render and the user pressing Enter).
    pub async fn get_workspace_agent(
        &self,
        account: &str,
        agent: &str,
        name: &str,
    ) -> Result<Option<String>> {
        let row = sqlx::query(
            "SELECT agent FROM workspaces
              WHERE account = ?1 AND agent = ?2 AND name = ?3",
        )
        .bind(account)
        .bind(agent)
        .bind(name)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(|r| r.get("agent")))
    }

    /// Drop every workspace binding for `agent`. Called when an
    /// admin deletes the agent so we don't leave dangling rows that
    /// reference a name that will never come back online.
    pub async fn delete_workspace_bindings_for_agent(
        &self,
        agent: &str,
    ) -> Result<u64> {
        let r = sqlx::query("DELETE FROM workspaces WHERE agent = ?1")
            .bind(agent)
            .execute(&self.pool)
            .await?;
        Ok(r.rows_affected())
    }

    // ---- account → agent whitelist ------------------------------------

    pub async fn list_allowed_agents(&self, account: &str) -> Result<Vec<String>> {
        let rows = sqlx::query(
            "SELECT agent FROM account_allowed_agents WHERE account = ?1 ORDER BY agent",
        )
        .bind(account)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(|r| r.get("agent")).collect())
    }

    pub async fn is_agent_allowed(&self, account: &str, agent: &str) -> Result<bool> {
        let row = sqlx::query(
            "SELECT 1 AS one FROM account_allowed_agents
              WHERE account = ?1 AND agent = ?2",
        )
        .bind(account)
        .bind(agent)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.is_some())
    }

    /// Replace this account's allowlist with the given set atomically.
    /// Caller is responsible for whatever de-duplication / validation
    /// makes sense (admin UI). An empty `agents` slice clears the list,
    /// which under strict-whitelist semantics means "this account can
    /// connect to nothing" — useful for soft-disable.
    pub async fn set_allowed_agents(&self, account: &str, agents: &[String]) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        sqlx::query("DELETE FROM account_allowed_agents WHERE account = ?1")
            .bind(account)
            .execute(&mut *tx)
            .await?;
        for agent in agents {
            sqlx::query(
                "INSERT OR IGNORE INTO account_allowed_agents (account, agent) VALUES (?1, ?2)",
            )
            .bind(account)
            .bind(agent)
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    /// Distinct agent names that still appear in the ACL table. The
    /// admin layer unions this with `registry.list_active()` to build
    /// the "known agents" picker. Sessions history is intentionally
    /// NOT included — once an admin has cleared an old agent from
    /// the ACL it should stop showing up, even though sessions rows
    /// (audit history) still reference its old name.
    pub async fn distinct_known_agents(&self) -> Result<Vec<String>> {
        let rows = sqlx::query(
            "SELECT DISTINCT agent FROM account_allowed_agents ORDER BY agent",
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(|r| r.get("agent")).collect())
    }

    /// Wipe every ACL row that names this agent. Used by the admin
    /// UI's "delete agent" action when an agent name is retired
    /// (renamed, decommissioned, etc).
    pub async fn delete_agent_acl(&self, agent: &str) -> Result<u64> {
        let r = sqlx::query("DELETE FROM account_allowed_agents WHERE agent = ?1")
            .bind(agent)
            .execute(&self.pool)
            .await?;
        Ok(r.rows_affected())
    }

    pub async fn list_allowed_accounts_for_agent(&self, agent: &str) -> Result<Vec<String>> {
        let rows = sqlx::query(
            "SELECT account FROM account_allowed_agents WHERE agent = ?1 ORDER BY account",
        )
        .bind(agent)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(|r| r.get("account")).collect())
    }

    pub async fn set_allowed_accounts_for_agent(
        &self,
        agent: &str,
        accounts: &[String],
    ) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        sqlx::query("DELETE FROM account_allowed_agents WHERE agent = ?1")
            .bind(agent)
            .execute(&mut *tx)
            .await?;
        for account in accounts {
            sqlx::query(
                "INSERT OR IGNORE INTO account_allowed_agents (account, agent) VALUES (?1, ?2)",
            )
            .bind(account)
            .bind(agent)
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    pub async fn count_allowed_accounts_per_agent(&self) -> Result<Vec<(String, i64)>> {
        let rows = sqlx::query(
            "SELECT agent, COUNT(*) AS n FROM account_allowed_agents GROUP BY agent",
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|r| (r.get::<String, _>("agent"), r.get::<i64, _>("n")))
            .collect())
    }

    // ---- audit ---------------------------------------------------------

    pub async fn list_audit_events(
        &self,
        f: &AuditFilter,
        limit: i64,
        offset: i64,
    ) -> Result<Vec<AuditDisplayRow>> {
        use sqlx::QueryBuilder;
        let mut qb = QueryBuilder::new(
            "SELECT id, ts, kind, account, agent, session_id, workspace, detail
               FROM audit_events
              WHERE 1=1",
        );
        if let Some(v) = &f.account {
            qb.push(" AND account = ").push_bind(v.clone());
        }
        if let Some(v) = &f.agent {
            qb.push(" AND agent = ").push_bind(v.clone());
        }
        if let Some(v) = &f.kind {
            qb.push(" AND kind = ").push_bind(v.clone());
        }
        if let Some(v) = f.since {
            qb.push(" AND ts >= ").push_bind(v);
        }
        if let Some(v) = f.until {
            qb.push(" AND ts <= ").push_bind(v);
        }
        qb.push(" ORDER BY ts DESC, id DESC LIMIT ")
            .push_bind(limit)
            .push(" OFFSET ")
            .push_bind(offset);
        let rows = qb.build().fetch_all(&self.pool).await?;
        Ok(rows
            .into_iter()
            .map(|r| AuditDisplayRow {
                id: r.get("id"),
                ts: r.get("ts"),
                kind: r.get("kind"),
                account: r.get("account"),
                agent: r.get("agent"),
                session_id: r.get("session_id"),
                workspace: r.get("workspace"),
                detail: r.get("detail"),
            })
            .collect())
    }

    pub async fn count_audit_events(&self, f: &AuditFilter) -> Result<i64> {
        use sqlx::QueryBuilder;
        let mut qb = QueryBuilder::new("SELECT COUNT(*) AS n FROM audit_events WHERE 1=1");
        if let Some(v) = &f.account {
            qb.push(" AND account = ").push_bind(v.clone());
        }
        if let Some(v) = &f.agent {
            qb.push(" AND agent = ").push_bind(v.clone());
        }
        if let Some(v) = &f.kind {
            qb.push(" AND kind = ").push_bind(v.clone());
        }
        if let Some(v) = f.since {
            qb.push(" AND ts >= ").push_bind(v);
        }
        if let Some(v) = f.until {
            qb.push(" AND ts <= ").push_bind(v);
        }
        let row = qb.build().fetch_one(&self.pool).await?;
        Ok(row.get::<i64, _>("n"))
    }

    pub async fn distinct_audit_kinds(&self) -> Result<Vec<String>> {
        let rows = sqlx::query(
            "SELECT DISTINCT kind FROM audit_events ORDER BY kind",
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(|r| r.get("kind")).collect())
    }

    /// Distinct kinds across both backing tables of the activity view.
    /// Used by the admin UI to populate the kind multi-select. Sorted
    /// alphabetically so dropdown order is stable.
    pub async fn distinct_activity_kinds(&self) -> Result<Vec<String>> {
        let rows = sqlx::query(
            "SELECT kind FROM (
                 SELECT DISTINCT kind FROM audit_events
                 UNION
                 SELECT DISTINCT kind FROM user_interactions
             ) ORDER BY kind",
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(|r| r.get("kind")).collect())
    }

    /// Best-effort insert; logs at debug on failure so a flaky disk
    /// doesn't break PTY flow.
    pub async fn insert_audit(&self, row: &AuditRow) {
        let res = sqlx::query(
            "INSERT INTO audit_events
                (ts, kind, account, agent, session_id, workspace, detail)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        )
        .bind(row.ts)
        .bind(&row.kind)
        .bind(&row.account)
        .bind(&row.agent)
        .bind(&row.session_id)
        .bind(&row.workspace)
        .bind(&row.detail)
        .execute(&self.pool)
        .await;
        if let Err(e) = res {
            tracing::debug!(error = %e, "audit insert failed");
        }
    }

    // ---- sessions ------------------------------------------------------

    pub async fn start_session(
        &self,
        session_id: &str,
        account: &str,
        agent: &str,
        workspace: &str,
    ) {
        let res = sqlx::query(
            "INSERT OR REPLACE INTO sessions
                (session_id, account, agent, workspace, started_at, ended_at, ended_reason)
             VALUES (?1, ?2, ?3, ?4, ?5, NULL, NULL)",
        )
        .bind(session_id)
        .bind(account)
        .bind(agent)
        .bind(workspace)
        .bind(chrono::Utc::now().timestamp())
        .execute(&self.pool)
        .await;
        if let Err(e) = res {
            tracing::debug!(error = %e, "session start insert failed");
        }
    }

    pub async fn list_sessions(
        &self,
        f: &SessionsFilter,
        limit: i64,
        offset: i64,
    ) -> Result<Vec<SessionRow>> {
        use sqlx::QueryBuilder;
        let mut qb = QueryBuilder::new(
            "SELECT session_id, account, agent, workspace, started_at, ended_at, ended_reason
               FROM sessions WHERE 1=1",
        );
        if let Some(v) = &f.account {
            qb.push(" AND account = ").push_bind(v.clone());
        }
        if let Some(v) = &f.agent {
            qb.push(" AND agent = ").push_bind(v.clone());
        }
        if let Some(v) = &f.workspace {
            qb.push(" AND workspace = ").push_bind(v.clone());
        }
        if f.active_only {
            qb.push(" AND ended_at IS NULL");
        }
        if let Some(v) = f.since {
            qb.push(" AND started_at >= ").push_bind(v);
        }
        qb.push(" ORDER BY started_at DESC LIMIT ")
            .push_bind(limit)
            .push(" OFFSET ")
            .push_bind(offset);
        let rows = qb.build().fetch_all(&self.pool).await?;
        Ok(rows
            .into_iter()
            .map(|r| SessionRow {
                session_id: r.get("session_id"),
                account: r.get("account"),
                agent: r.get("agent"),
                workspace: r.get("workspace"),
                started_at: r.get("started_at"),
                ended_at: r.get("ended_at"),
                ended_reason: r.get("ended_reason"),
            })
            .collect())
    }

    pub async fn count_sessions(&self, f: &SessionsFilter) -> Result<i64> {
        use sqlx::QueryBuilder;
        let mut qb = QueryBuilder::new("SELECT COUNT(*) AS n FROM sessions WHERE 1=1");
        if let Some(v) = &f.account {
            qb.push(" AND account = ").push_bind(v.clone());
        }
        if let Some(v) = &f.agent {
            qb.push(" AND agent = ").push_bind(v.clone());
        }
        if let Some(v) = &f.workspace {
            qb.push(" AND workspace = ").push_bind(v.clone());
        }
        if f.active_only {
            qb.push(" AND ended_at IS NULL");
        }
        if let Some(v) = f.since {
            qb.push(" AND started_at >= ").push_bind(v);
        }
        let row = qb.build().fetch_one(&self.pool).await?;
        Ok(row.get::<i64, _>("n"))
    }

    pub async fn get_session(&self, session_id: &str) -> Result<Option<SessionRow>> {
        let row = sqlx::query(
            "SELECT session_id, account, agent, workspace, started_at, ended_at, ended_reason
               FROM sessions WHERE session_id = ?1",
        )
        .bind(session_id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(|r| SessionRow {
            session_id: r.get("session_id"),
            account: r.get("account"),
            agent: r.get("agent"),
            workspace: r.get("workspace"),
            started_at: r.get("started_at"),
            ended_at: r.get("ended_at"),
            ended_reason: r.get("ended_reason"),
        }))
    }

    /// On hub startup any session row still flagged "live" (ended_at
    /// IS NULL) is an orphan from the previous hub process — nothing
    /// is actually attached to it any more. Close them all out so the
    /// admin dashboard tells the truth.
    pub async fn close_orphan_sessions(&self, reason: &str) -> Result<u64> {
        let r = sqlx::query(
            "UPDATE sessions SET ended_at = ?1, ended_reason = ?2 WHERE ended_at IS NULL",
        )
        .bind(chrono::Utc::now().timestamp())
        .bind(reason)
        .execute(&self.pool)
        .await?;
        Ok(r.rows_affected())
    }

    /// Currently-active sessions (no end recorded). Quick stats card.
    pub async fn count_active_sessions(&self) -> Result<i64> {
        let row = sqlx::query("SELECT COUNT(*) AS n FROM sessions WHERE ended_at IS NULL")
            .fetch_one(&self.pool)
            .await?;
        Ok(row.get::<i64, _>("n"))
    }

    // ---- messages ------------------------------------------------------

    /// Append one conversation message. Idempotency is the caller's job
    /// (agent dedupes via jsonl_progress offsets). Token columns are
    /// precomputed here so the daily report never json_extracts a body.
    pub async fn insert_message(&self, row: &MessageRow) {
        let (it, ot, cc, cr) = usage_tokens(&row.kind, &row.body);
        let res = sqlx::query(
            "INSERT INTO messages
                 (cc_session_id, claude_session_id, ts, kind, body,
                  input_tokens, output_tokens, cache_creation_tokens, cache_read_tokens)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        )
        .bind(&row.cc_session_id)
        .bind(&row.claude_session_id)
        .bind(row.ts)
        .bind(&row.kind)
        .bind(&row.body)
        .bind(it)
        .bind(ot)
        .bind(cc)
        .bind(cr)
        .execute(&self.pool)
        .await;
        if let Err(e) = res {
            tracing::debug!(error = %e, "message insert failed");
        }
    }

    pub async fn list_messages_for_session(
        &self,
        cc_session_id: &str,
        limit: i64,
    ) -> Result<Vec<MessageDisplayRow>> {
        let rows = sqlx::query(
            "SELECT id, cc_session_id, claude_session_id, ts, kind, body
               FROM messages
              WHERE cc_session_id = ?1
              ORDER BY id ASC
              LIMIT ?2",
        )
        .bind(cc_session_id)
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|r| MessageDisplayRow {
                id: r.get("id"),
                cc_session_id: r.get("cc_session_id"),
                claude_session_id: r.get("claude_session_id"),
                ts: r.get("ts"),
                kind: r.get("kind"),
                body: r.get("body"),
            })
            .collect())
    }

    pub async fn count_messages_for_session(&self, cc_session_id: &str) -> Result<i64> {
        let row =
            sqlx::query("SELECT COUNT(*) AS n FROM messages WHERE cc_session_id = ?1")
                .bind(cc_session_id)
                .fetch_one(&self.pool)
                .await?;
        Ok(row.get::<i64, _>("n"))
    }

    /// Number of sessions started within the last `seconds` seconds.
    pub async fn count_sessions_since(&self, seconds: i64) -> Result<i64> {
        let cutoff = chrono::Utc::now().timestamp() - seconds;
        let row = sqlx::query("SELECT COUNT(*) AS n FROM sessions WHERE started_at >= ?1")
            .bind(cutoff)
            .fetch_one(&self.pool)
            .await?;
        Ok(row.get::<i64, _>("n"))
    }

    /// Latest `started_at` per (agent, account, workspace). Used by the
    /// admin Workspaces page to show when each slot was last touched.
    pub async fn last_started_per_workspace(
        &self,
    ) -> Result<Vec<(String, String, String, i64)>> {
        let rows = sqlx::query(
            "SELECT agent, account, workspace, MAX(started_at) AS last_started
               FROM sessions
              GROUP BY agent, account, workspace",
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|r| {
                (
                    r.get::<String, _>("agent"),
                    r.get::<String, _>("account"),
                    r.get::<String, _>("workspace"),
                    r.get::<i64, _>("last_started"),
                )
            })
            .collect())
    }

    pub async fn end_session(&self, session_id: &str, reason: Option<&str>) {
        let res = sqlx::query(
            "UPDATE sessions
                SET ended_at = ?1, ended_reason = ?2
              WHERE session_id = ?3 AND ended_at IS NULL",
        )
        .bind(chrono::Utc::now().timestamp())
        .bind(reason)
        .bind(session_id)
        .execute(&self.pool)
        .await;
        if let Err(e) = res {
            tracing::debug!(error = %e, "session end update failed");
        }
    }

    // ---- sessions (admin + webterm; survive hub restart) --------------

    pub async fn insert_admin_session(&self, sid: &str, expires_at: i64) -> Result<()> {
        sqlx::query(
            "INSERT INTO admin_sessions (sid, expires_at) VALUES (?1, ?2)",
        )
        .bind(sid)
        .bind(expires_at)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn valid_admin_session(&self, sid: &str) -> Result<bool> {
        let now = chrono::Utc::now().timestamp();
        let row = sqlx::query(
            "SELECT 1 FROM admin_sessions WHERE sid = ?1 AND expires_at > ?2",
        )
        .bind(sid)
        .bind(now)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.is_some())
    }

    pub async fn delete_admin_session(&self, sid: &str) -> Result<()> {
        sqlx::query("DELETE FROM admin_sessions WHERE sid = ?1")
            .bind(sid)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn insert_user_session(
        &self,
        sid: &str,
        account: &str,
        expires_at: i64,
    ) -> Result<()> {
        sqlx::query(
            "INSERT INTO user_sessions (sid, account, expires_at) VALUES (?1, ?2, ?3)",
        )
        .bind(sid)
        .bind(account)
        .bind(expires_at)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn user_session_account(&self, sid: &str) -> Result<Option<String>> {
        let now = chrono::Utc::now().timestamp();
        let row = sqlx::query(
            "SELECT account FROM user_sessions WHERE sid = ?1 AND expires_at > ?2",
        )
        .bind(sid)
        .bind(now)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(|r| r.get::<String, _>("account")))
    }

    pub async fn delete_user_session(&self, sid: &str) -> Result<()> {
        sqlx::query("DELETE FROM user_sessions WHERE sid = ?1")
            .bind(sid)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Drop sessions whose expiry has elapsed. Called once on hub
    /// startup so the tables don't grow unboundedly across long
    /// uptimes / many cookie cycles.
    pub async fn cleanup_expired_sessions(&self) -> Result<u64> {
        let now = chrono::Utc::now().timestamp();
        let a = sqlx::query("DELETE FROM admin_sessions WHERE expires_at <= ?1")
            .bind(now)
            .execute(&self.pool)
            .await?
            .rows_affected();
        let u = sqlx::query("DELETE FROM user_sessions WHERE expires_at <= ?1")
            .bind(now)
            .execute(&self.pool)
            .await?
            .rows_affected();
        Ok(a + u)
    }

    // ---- user_interactions (captured user prompts from claude jsonl) --

    /// Best-effort insert; INSERT OR IGNORE so the agent's restart-resume
    /// retail doesn't duplicate rows on `(claude_session_id, parent_uuid, ts_ms)`.
    /// Logs at debug on failure so audit traffic never disrupts PTY flow.
    pub async fn insert_user_interaction(&self, row: &UserInteractionRow) {
        let res = sqlx::query(
            "INSERT OR IGNORE INTO user_interactions
                (account, agent, workspace, claude_session_id, prompt_id, parent_uuid,
                 cwd, git_branch, ts_ms, kind, content)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
        )
        .bind(&row.account)
        .bind(&row.agent)
        .bind(&row.workspace)
        .bind(&row.claude_session_id)
        .bind(&row.prompt_id)
        .bind(&row.parent_uuid)
        .bind(&row.cwd)
        .bind(&row.git_branch)
        .bind(row.ts_ms)
        .bind(&row.kind)
        .bind(&row.content)
        .execute(&self.pool)
        .await;
        if let Err(e) = res {
            tracing::debug!(error = %e, "user_interaction insert failed");
        }
    }

    pub async fn list_user_interactions(
        &self,
        f: &UserInteractionFilter,
        limit: i64,
        offset: i64,
    ) -> Result<Vec<UserInteractionDisplayRow>> {
        use sqlx::QueryBuilder;
        let mut qb = QueryBuilder::new(
            "SELECT id, account, agent, workspace, claude_session_id, prompt_id, parent_uuid,
                    cwd, git_branch, ts_ms, kind, content
               FROM user_interactions
              WHERE 1=1",
        );
        if let Some(v) = &f.account {
            qb.push(" AND account = ").push_bind(v.clone());
        }
        if let Some(v) = &f.workspace {
            qb.push(" AND workspace = ").push_bind(v.clone());
        }
        if let Some(v) = &f.kind {
            qb.push(" AND kind = ").push_bind(v.clone());
        }
        if let Some(v) = f.since_ms {
            qb.push(" AND ts_ms >= ").push_bind(v);
        }
        if let Some(v) = f.until_ms {
            qb.push(" AND ts_ms <= ").push_bind(v);
        }
        qb.push(" ORDER BY ts_ms DESC, id DESC LIMIT ")
            .push_bind(limit)
            .push(" OFFSET ")
            .push_bind(offset);
        let rows = qb.build().fetch_all(&self.pool).await?;
        Ok(rows
            .into_iter()
            .map(|r| UserInteractionDisplayRow {
                id: r.get("id"),
                account: r.get("account"),
                agent: r.get("agent"),
                workspace: r.get("workspace"),
                claude_session_id: r.get("claude_session_id"),
                prompt_id: r.get("prompt_id"),
                parent_uuid: r.get("parent_uuid"),
                cwd: r.get("cwd"),
                git_branch: r.get("git_branch"),
                ts_ms: r.get("ts_ms"),
                kind: r.get("kind"),
                content: r.get("content"),
            })
            .collect())
    }

    pub async fn count_user_interactions(&self, f: &UserInteractionFilter) -> Result<i64> {
        use sqlx::QueryBuilder;
        let mut qb = QueryBuilder::new(
            "SELECT COUNT(*) AS n FROM user_interactions WHERE 1=1",
        );
        if let Some(v) = &f.account {
            qb.push(" AND account = ").push_bind(v.clone());
        }
        if let Some(v) = &f.workspace {
            qb.push(" AND workspace = ").push_bind(v.clone());
        }
        if let Some(v) = &f.kind {
            qb.push(" AND kind = ").push_bind(v.clone());
        }
        if let Some(v) = f.since_ms {
            qb.push(" AND ts_ms >= ").push_bind(v);
        }
        if let Some(v) = f.until_ms {
            qb.push(" AND ts_ms <= ").push_bind(v);
        }
        let row = qb.build().fetch_one(&self.pool).await?;
        Ok(row.get::<i64, _>("n"))
    }

    /// Full row including the (sensitive) content text. Returned only
    /// from the explicit `/reveal` admin endpoint, which also writes an
    /// audit event so reveals are forensically traceable.
    pub async fn get_user_interaction(&self, id: i64) -> Result<Option<UserInteractionFullRow>> {
        let row = sqlx::query(
            "SELECT id, account, agent, workspace, claude_session_id, prompt_id, parent_uuid,
                    cwd, git_branch, ts_ms, kind, content
               FROM user_interactions
              WHERE id = ?1",
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(|r| UserInteractionFullRow {
            id: r.get("id"),
            account: r.get("account"),
            agent: r.get("agent"),
            workspace: r.get("workspace"),
            claude_session_id: r.get("claude_session_id"),
            prompt_id: r.get("prompt_id"),
            parent_uuid: r.get("parent_uuid"),
            cwd: r.get("cwd"),
            git_branch: r.get("git_branch"),
            ts_ms: r.get("ts_ms"),
            kind: r.get("kind"),
            content: r.get("content"),
        }))
    }

    // ---- unified activity (audit_events ∪ user_interactions) ---------
    //
    // Single read-only view: each side projects into a common shape
    // (`id, source, ts_ms, kind, account, agent, workspace, session_id,
    // detail`), then UNION ALL + ORDER BY across the merged set. The
    // source filter is applied inside each branch via
    // `($source = 'all' OR $source = 'audit'/'interaction')` so the
    // optimiser short-circuits the inactive branch when the caller
    // pins to one side. `audit_events.ts` is in seconds — we multiply
    // by 1000 inside the SELECT so a cross-table ORDER BY is meaningful.

    pub async fn list_activity(
        &self,
        f: &ActivityFilter,
        limit: i64,
        offset: i64,
    ) -> Result<Vec<ActivityRow>> {
        use sqlx::QueryBuilder;
        let mut qb = QueryBuilder::new("");
        build_activity_select(&mut qb, f, /*as_count=*/ false);
        qb.push(" ORDER BY ts_ms DESC, id DESC LIMIT ")
            .push_bind(limit)
            .push(" OFFSET ")
            .push_bind(offset);
        let rows = qb.build().fetch_all(&self.pool).await?;
        Ok(rows
            .into_iter()
            .map(|r| ActivityRow {
                id: r.get("id"),
                source: r.get("source"),
                ts_ms: r.get("ts_ms"),
                kind: r.get("kind"),
                account: r.get("account"),
                agent: r.get("agent"),
                workspace: r.get("workspace"),
                session_id: r.get("session_id"),
                // Both branches emit valid JSON object text (audit:
                // pass-through of audit_events.detail; interaction:
                // SQLite's json_object()). We keep it as String here
                // and let the admin layer parse — keeps the DB layer
                // free of serde_json dependencies on the row type.
                detail: r.get("detail"),
            })
            .collect())
    }

    pub async fn count_activity(&self, f: &ActivityFilter) -> Result<i64> {
        use sqlx::QueryBuilder;
        let mut qb = QueryBuilder::new("SELECT COUNT(*) AS n FROM (");
        build_activity_select(&mut qb, f, /*as_count=*/ true);
        qb.push(")");
        let row = qb.build().fetch_one(&self.pool).await?;
        Ok(row.get::<i64, _>("n"))
    }

    // ---- user preferences (opaque JSON blob per account) --------------

    /// Returns the raw JSON blob the webterm last stored for this
    /// account, or `None` if the account has never saved preferences.
    /// The hub does not parse `data` — webterm owns the schema.
    pub async fn get_user_preferences(&self, account: &str) -> Result<Option<String>> {
        let row = sqlx::query("SELECT data FROM user_preferences WHERE account = ?1")
            .bind(account)
            .fetch_optional(&self.pool)
            .await?;
        Ok(row.map(|r| r.get::<String, _>("data")))
    }

    /// Upsert the per-account preferences blob. `data` must be valid
    /// JSON; this method does not validate (the API handler does).
    pub async fn set_user_preferences(&self, account: &str, data: &str) -> Result<()> {
        sqlx::query(
            "INSERT INTO user_preferences (account, data, updated_at)
                  VALUES (?1, ?2, ?3)
             ON CONFLICT(account) DO UPDATE
                SET data = excluded.data,
                    updated_at = excluded.updated_at",
        )
        .bind(account)
        .bind(data)
        .bind(chrono::Utc::now().timestamp())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    // ---- invite links --------------------------------------------------

    /// Insert a fresh invite row. Caller has already generated `id`
    /// + `token` (random) and validated the input. `allowed_agents`
    /// is joined with commas into the column (empty list → empty
    /// string). `created_by` is the admin name — always literally
    /// "admin" under the current single-admin model, but plumbed
    /// through as a parameter so a future per-operator admin schema
    /// doesn't need a DB layer change.
    pub async fn insert_invite(
        &self,
        id: &str,
        label: Option<&str>,
        token: &str,
        max_uses: i64,
        allowed_agents: &[String],
        created_by: &str,
        sandbox_mode: &str,
    ) -> Result<()> {
        if !matches!(sandbox_mode, "strict" | "permissive" | "off") {
            anyhow::bail!("invalid sandbox mode '{}'", sandbox_mode);
        }
        let agents_str = allowed_agents.join(",");
        sqlx::query(
            "INSERT INTO invite_links
                (id, label, token, max_uses, used, allowed_agents, active, created_at, created_by, sandbox_mode)
             VALUES (?1, ?2, ?3, ?4, 0, ?5, 1, ?6, ?7, ?8)",
        )
        .bind(id)
        .bind(label)
        .bind(token)
        .bind(max_uses)
        .bind(agents_str)
        .bind(chrono::Utc::now().timestamp())
        .bind(created_by)
        .bind(sandbox_mode)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn get_invite(&self, id: &str) -> Result<Option<DbInvite>> {
        let row = sqlx::query(
            "SELECT id, label, token, max_uses, used, allowed_agents, active, created_at, created_by, sandbox_mode
               FROM invite_links WHERE id = ?1",
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(invite_from_row))
    }

    pub async fn get_invite_by_token(&self, token: &str) -> Result<Option<DbInvite>> {
        let row = sqlx::query(
            "SELECT id, label, token, max_uses, used, allowed_agents, active, created_at, created_by, sandbox_mode
               FROM invite_links WHERE token = ?1",
        )
        .bind(token)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(invite_from_row))
    }

    pub async fn list_invites(&self) -> Result<Vec<DbInvite>> {
        let rows = sqlx::query(
            "SELECT id, label, token, max_uses, used, allowed_agents, active, created_at, created_by, sandbox_mode
               FROM invite_links ORDER BY created_at DESC",
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(invite_from_row).collect())
    }

    pub async fn update_invite_active(&self, id: &str, active: bool) -> Result<()> {
        let rows = sqlx::query("UPDATE invite_links SET active = ?1 WHERE id = ?2")
            .bind(if active { 1_i64 } else { 0_i64 })
            .bind(id)
            .execute(&self.pool)
            .await?
            .rows_affected();
        if rows == 0 {
            anyhow::bail!("invite '{}' not found", id);
        }
        Ok(())
    }

    pub async fn update_invite_max_uses(&self, id: &str, max_uses: i64) -> Result<()> {
        let rows = sqlx::query("UPDATE invite_links SET max_uses = ?1 WHERE id = ?2")
            .bind(max_uses)
            .bind(id)
            .execute(&self.pool)
            .await?
            .rows_affected();
        if rows == 0 {
            anyhow::bail!("invite '{}' not found", id);
        }
        Ok(())
    }

    pub async fn update_invite_label(&self, id: &str, label: Option<&str>) -> Result<()> {
        let rows = sqlx::query("UPDATE invite_links SET label = ?1 WHERE id = ?2")
            .bind(label)
            .bind(id)
            .execute(&self.pool)
            .await?
            .rows_affected();
        if rows == 0 {
            anyhow::bail!("invite '{}' not found", id);
        }
        Ok(())
    }

    pub async fn update_invite_allowed_agents(&self, id: &str, agents: &[String]) -> Result<()> {
        let joined = agents.join(",");
        let rows = sqlx::query("UPDATE invite_links SET allowed_agents = ?1 WHERE id = ?2")
            .bind(joined)
            .bind(id)
            .execute(&self.pool)
            .await?
            .rows_affected();
        if rows == 0 {
            anyhow::bail!("invite '{}' not found", id);
        }
        Ok(())
    }

    pub async fn update_invite_sandbox_mode(&self, id: &str, mode: &str) -> Result<()> {
        if !matches!(mode, "strict" | "permissive" | "off") {
            anyhow::bail!("invalid sandbox mode '{}'", mode);
        }
        let rows = sqlx::query("UPDATE invite_links SET sandbox_mode = ?1 WHERE id = ?2")
            .bind(mode)
            .bind(id)
            .execute(&self.pool)
            .await?
            .rows_affected();
        if rows == 0 {
            anyhow::bail!("invite '{}' not found", id);
        }
        Ok(())
    }

    pub async fn delete_invite(&self, id: &str) -> Result<()> {
        // Acceptances rows are kept on purpose — they're the audit
        // trail of who got in via this link. Dropping them when the
        // admin deletes the link would erase forensic history. The
        // admin UI can still surface them by joining on invite_id
        // without needing the parent row.
        let rows = sqlx::query("DELETE FROM invite_links WHERE id = ?1")
            .bind(id)
            .execute(&self.pool)
            .await?
            .rows_affected();
        if rows == 0 {
            anyhow::bail!("invite '{}' not found", id);
        }
        Ok(())
    }

    /// Record an acceptance and bump `used` in a single transaction so
    /// the counter and the audit row never disagree. Caller must have
    /// already checked `used < max_uses` (or `max_uses == 0`); this
    /// method enforces the row exists but not the cap.
    pub async fn record_invite_acceptance(&self, invite_id: &str, account: &str) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        sqlx::query(
            "INSERT INTO invite_acceptances (invite_id, account, accepted_at)
             VALUES (?1, ?2, ?3)",
        )
        .bind(invite_id)
        .bind(account)
        .bind(chrono::Utc::now().timestamp())
        .execute(&mut *tx)
        .await?;
        let rows = sqlx::query("UPDATE invite_links SET used = used + 1 WHERE id = ?1")
            .bind(invite_id)
            .execute(&mut *tx)
            .await?
            .rows_affected();
        if rows == 0 {
            anyhow::bail!("invite '{}' not found", invite_id);
        }
        tx.commit().await?;
        Ok(())
    }

    /// Account name + acceptance timestamp for every account minted
    /// via this invite. Ordered newest first so the admin UI shows
    /// the most recent acceptances at the top without an extra sort.
    pub async fn list_invite_acceptances(&self, invite_id: &str) -> Result<Vec<(String, i64)>> {
        let rows = sqlx::query(
            "SELECT account, accepted_at FROM invite_acceptances
              WHERE invite_id = ?1
              ORDER BY accepted_at DESC",
        )
        .bind(invite_id)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|r| (r.get::<String, _>("account"), r.get::<i64, _>("accepted_at")))
            .collect())
    }
}

/// One row in `invite_links`. `allowed_agents` is exposed as a Vec
/// (split on commas + empty filter) because the DB layer is the only
/// place that knows about the comma-separated encoding; every caller
/// works in terms of agent names.
#[derive(Debug, Clone)]
pub struct DbInvite {
    pub id: String,
    pub label: Option<String>,
    pub token: String,
    pub max_uses: i64,
    pub used: i64,
    pub allowed_agents: Vec<String>,
    pub active: bool,
    pub created_at: i64,
    pub created_by: String,
    pub sandbox_mode: String,
}

fn invite_from_row(r: sqlx::sqlite::SqliteRow) -> DbInvite {
    let agents_str: String = r.get("allowed_agents");
    let allowed_agents: Vec<String> = agents_str
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    DbInvite {
        id: r.get("id"),
        label: r.get("label"),
        token: r.get("token"),
        max_uses: r.get("max_uses"),
        used: r.get("used"),
        allowed_agents,
        active: r.get::<i64, _>("active") != 0,
        created_at: r.get("created_at"),
        created_by: r.get("created_by"),
        sandbox_mode: r.get("sandbox_mode"),
    }
}

#[derive(Debug, Clone, Default)]
pub struct AuditRow {
    pub ts: i64,
    pub kind: String,
    pub account: Option<String>,
    pub agent: Option<String>,
    pub session_id: Option<String>,
    pub workspace: Option<String>,
    pub detail: Option<String>,
}

#[derive(Debug, Clone)]
pub struct AuditDisplayRow {
    pub id: i64,
    pub ts: i64,
    pub kind: String,
    pub account: Option<String>,
    pub agent: Option<String>,
    pub session_id: Option<String>,
    pub workspace: Option<String>,
    pub detail: Option<String>,
}

#[derive(Debug, Clone)]
pub struct SessionRow {
    pub session_id: String,
    pub account: String,
    pub agent: String,
    pub workspace: String,
    pub started_at: i64,
    pub ended_at: Option<i64>,
    pub ended_reason: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct SessionsFilter {
    pub account: Option<String>,
    pub agent: Option<String>,
    pub workspace: Option<String>,
    pub active_only: bool,
    pub since: Option<i64>,
}

#[derive(Debug, Clone)]
pub struct MessageRow {
    pub cc_session_id: String,
    pub claude_session_id: String,
    pub ts: i64,
    pub kind: String,
    pub body: String,
}

#[derive(Debug, Clone)]
pub struct MessageDisplayRow {
    pub id: i64,
    pub cc_session_id: String,
    pub claude_session_id: String,
    pub ts: i64,
    pub kind: String,
    pub body: String,
}

/// Wire-shaped row the agent ships; everything we need to persist.
#[derive(Debug, Clone)]
pub struct UserInteractionRow {
    pub account: String,
    pub agent: String,
    pub workspace: String,
    pub claude_session_id: String,
    pub prompt_id: Option<String>,
    pub parent_uuid: Option<String>,
    pub cwd: Option<String>,
    pub git_branch: Option<String>,
    pub ts_ms: i64,
    pub kind: String,
    pub content: String,
}

/// List-page row — `content` is intentionally omitted so a bulk list
/// can never leak sensitive text. Use `get_user_interaction(id)` +
/// the `/reveal` endpoint to surface a single row's content.
#[derive(Debug, Clone)]
pub struct UserInteractionDisplayRow {
    pub id: i64,
    pub account: String,
    pub agent: String,
    pub workspace: String,
    pub claude_session_id: String,
    pub prompt_id: Option<String>,
    pub parent_uuid: Option<String>,
    pub cwd: Option<String>,
    pub git_branch: Option<String>,
    pub ts_ms: i64,
    pub kind: String,
    pub content: String,
}

#[derive(Debug, Clone)]
pub struct UserInteractionFullRow {
    pub id: i64,
    pub account: String,
    pub agent: String,
    pub workspace: String,
    pub claude_session_id: String,
    pub prompt_id: Option<String>,
    pub parent_uuid: Option<String>,
    pub cwd: Option<String>,
    pub git_branch: Option<String>,
    pub ts_ms: i64,
    pub kind: String,
    pub content: String,
}

#[derive(Debug, Clone, Default)]
pub struct UserInteractionFilter {
    pub account: Option<String>,
    pub workspace: Option<String>,
    pub kind: Option<String>,
    pub since_ms: Option<i64>,
    pub until_ms: Option<i64>,
}

#[derive(Debug, Clone, Default)]
pub struct AuditFilter {
    pub account: Option<String>,
    pub agent: Option<String>,
    pub kind: Option<String>,
    pub since: Option<i64>,
    pub until: Option<i64>,
}

/// Filter for the unified activity view. `source` is the literal
/// string `"audit"`, `"interaction"`, or `"all"` (None == "all").
/// Anything else from the wire is normalised to "all" by the admin
/// layer before reaching here.
#[derive(Debug, Clone, Default)]
pub struct ActivityFilter {
    pub source: Option<String>,
    pub account: Option<String>,
    pub agent: Option<String>,
    pub workspace: Option<String>,
    /// Empty / None means no filter. Non-empty vec means
    /// `kind IN (a, b, c)` — multi-select.
    pub kind: Option<Vec<String>>,
    pub since_ms: Option<i64>,
    pub until_ms: Option<i64>,
}

/// One row of the merged audit+interaction stream. `detail` is the
/// raw JSON object text emitted by either branch of the UNION; the
/// admin layer parses it into `serde_json::Value` for the wire.
#[derive(Debug, Clone)]
pub struct ActivityRow {
    pub id: i64,
    pub source: String,
    pub ts_ms: i64,
    pub kind: String,
    pub account: Option<String>,
    pub agent: Option<String>,
    pub workspace: Option<String>,
    pub session_id: Option<String>,
    pub detail: Option<String>,
}

/// Emit `AND kind IN (?, ?, …)` for a non-empty kind filter, or
/// nothing at all if the filter is unset/empty. Shared by both
/// branches of the activity UNION so the filter shape stays
/// consistent (otherwise a `kind=foo,bar` query would only match
/// audit rows, not interaction ones, or vice versa).
fn push_kind_in(
    qb: &mut sqlx::QueryBuilder<'_, sqlx::Sqlite>,
    kinds: &Option<Vec<String>>,
) {
    let Some(kinds) = kinds else { return };
    if kinds.is_empty() {
        return;
    }
    qb.push(" AND kind IN (");
    let mut sep = qb.separated(", ");
    for k in kinds {
        sep.push_bind(k.clone());
    }
    qb.push(")");
}

/// Push the UNION ALL body into `qb`. Used by both `list_activity`
/// (wrapped with ORDER BY + LIMIT/OFFSET) and `count_activity`
/// (wrapped as `SELECT COUNT(*) FROM (...)`). Kept as a free function
/// because the two callers share every filter clause verbatim — the
/// only difference is what wraps the union.
///
/// `as_count` is currently informational; both branches need the
/// same shape because SQLite still types the columns of a UNION
/// regardless of an outer COUNT(*). Kept as a parameter so future
/// optimisation (e.g. dropping `detail` projection for the count
/// path) is a one-line tweak.
fn build_activity_select(
    qb: &mut sqlx::QueryBuilder<'_, sqlx::Sqlite>,
    f: &ActivityFilter,
    _as_count: bool,
) {
    let source = f.source.as_deref().unwrap_or("all");

    // --- audit_events branch --------------------------------------
    qb.push(
        "SELECT id, 'audit' AS source, ts * 1000 AS ts_ms, kind,
                account, agent, workspace, session_id, detail
           FROM audit_events
          WHERE (",
    );
    qb.push_bind(source.to_string())
        .push(" = 'all' OR ")
        .push_bind(source.to_string())
        .push(" = 'audit')");
    if let Some(v) = &f.account {
        qb.push(" AND account = ").push_bind(v.clone());
    }
    if let Some(v) = &f.agent {
        qb.push(" AND agent = ").push_bind(v.clone());
    }
    if let Some(v) = &f.workspace {
        qb.push(" AND workspace = ").push_bind(v.clone());
    }
    push_kind_in(qb, &f.kind);
    // since_ms / until_ms compare against the *projected* ts_ms,
    // but the underlying column is in seconds. Divide on the
    // bound side (ceil/floor not critical at second granularity).
    if let Some(v) = f.since_ms {
        // audit_events.ts is seconds; bound is ms. >= on ms means
        // >= on (ts*1000), so equivalently ts >= ceil(v/1000).
        let secs = v.div_euclid(1000);
        qb.push(" AND ts >= ").push_bind(secs);
    }
    if let Some(v) = f.until_ms {
        // For the upper bound, allow any second whose *start* is
        // <= until_ms. ts <= floor(v/1000) is the safe bound.
        let secs = v.div_euclid(1000);
        qb.push(" AND ts <= ").push_bind(secs);
    }

    qb.push(" UNION ALL ");

    // --- user_interactions branch ---------------------------------
    qb.push(
        "SELECT id, 'interaction' AS source, ts_ms, kind,
                account, agent, workspace,
                claude_session_id AS session_id,
                json_object(
                    'content',     content,
                    'cwd',         cwd,
                    'git_branch',  git_branch,
                    'prompt_id',   prompt_id,
                    'parent_uuid', parent_uuid
                ) AS detail
           FROM user_interactions
          WHERE (",
    );
    qb.push_bind(source.to_string())
        .push(" = 'all' OR ")
        .push_bind(source.to_string())
        .push(" = 'interaction')");
    if let Some(v) = &f.account {
        qb.push(" AND account = ").push_bind(v.clone());
    }
    if let Some(v) = &f.agent {
        qb.push(" AND agent = ").push_bind(v.clone());
    }
    if let Some(v) = &f.workspace {
        qb.push(" AND workspace = ").push_bind(v.clone());
    }
    push_kind_in(qb, &f.kind);
    if let Some(v) = f.since_ms {
        qb.push(" AND ts_ms >= ").push_bind(v);
    }
    if let Some(v) = f.until_ms {
        qb.push(" AND ts_ms <= ").push_bind(v);
    }
}

// ---- data maintenance / retention cleanup --------------------------------

/// Row counts for the prunable time-series tables. `audit_events` is
/// deliberately excluded — the audit trail is retained.
#[derive(Debug, Clone, Copy, Default, serde::Serialize)]
pub struct CleanupCounts {
    pub messages: i64,
    pub sessions: i64,
    pub user_interactions: i64,
}

impl Db {
    /// Count rows older than `cutoff_secs` across the prunable tables —
    /// used to preview a cleanup before committing to it.
    pub async fn cleanup_counts(&self, cutoff_secs: i64) -> Result<CleanupCounts> {
        let cutoff_ms = cutoff_secs.saturating_mul(1000);
        let messages: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM messages WHERE ts < ?1")
            .bind(cutoff_secs)
            .fetch_one(&self.pool)
            .await?;
        let sessions: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM sessions WHERE started_at < ?1")
            .bind(cutoff_secs)
            .fetch_one(&self.pool)
            .await?;
        let user_interactions: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM user_interactions WHERE ts_ms < ?1")
                .bind(cutoff_ms)
                .fetch_one(&self.pool)
                .await?;
        Ok(CleanupCounts {
            messages,
            sessions,
            user_interactions,
        })
    }

    /// Delete rows older than `cutoff_secs` from the prunable tables and
    /// return how many were removed from each. Does NOT vacuum — the caller
    /// decides whether to reclaim the freed space.
    pub async fn cleanup_delete(&self, cutoff_secs: i64) -> Result<CleanupCounts> {
        let cutoff_ms = cutoff_secs.saturating_mul(1000);
        let messages = sqlx::query("DELETE FROM messages WHERE ts < ?1")
            .bind(cutoff_secs)
            .execute(&self.pool)
            .await?
            .rows_affected() as i64;
        let sessions = sqlx::query("DELETE FROM sessions WHERE started_at < ?1")
            .bind(cutoff_secs)
            .execute(&self.pool)
            .await?
            .rows_affected() as i64;
        let user_interactions = sqlx::query("DELETE FROM user_interactions WHERE ts_ms < ?1")
            .bind(cutoff_ms)
            .execute(&self.pool)
            .await?
            .rows_affected() as i64;
        Ok(CleanupCounts {
            messages,
            sessions,
            user_interactions,
        })
    }

    /// Like `cleanup_delete` but deletes in small batches with a yield
    /// between them, so a large auto-cleanup never holds a long write lock
    /// that would stall agents shipping messages. Used by the daily job.
    pub async fn cleanup_delete_batched(&self, cutoff_secs: i64) -> Result<CleanupCounts> {
        let cutoff_ms = cutoff_secs.saturating_mul(1000);
        let mut totals = CleanupCounts::default();
        // (table, ts-column, cutoff value, which counter)
        let targets: [(&str, &str, i64); 3] = [
            ("messages", "ts", cutoff_secs),
            ("sessions", "started_at", cutoff_secs),
            ("user_interactions", "ts_ms", cutoff_ms),
        ];
        for (table, col, cutoff) in targets {
            loop {
                let sql = format!(
                    "DELETE FROM {table} WHERE rowid IN
                         (SELECT rowid FROM {table} WHERE {col} < ?1 LIMIT 5000)"
                );
                let n = sqlx::query(&sql)
                    .bind(cutoff)
                    .execute(&self.pool)
                    .await?
                    .rows_affected() as i64;
                match table {
                    "messages" => totals.messages += n,
                    "sessions" => totals.sessions += n,
                    _ => totals.user_interactions += n,
                }
                if n < 5000 {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            }
        }
        Ok(totals)
    }

    /// Rewrite the database file to reclaim space freed by deletes. Takes an
    /// exclusive lock for the duration — callers should treat it as a brief
    /// write-stall.
    pub async fn vacuum(&self) -> Result<()> {
        sqlx::query("VACUUM").execute(&self.pool).await?;
        Ok(())
    }

    /// Read the auto-cleanup settings (enabled + retention months).
    pub async fn auto_cleanup_settings(&self) -> (bool, u32) {
        let enabled = self
            .get_meta("cleanup_enabled")
            .await
            .ok()
            .flatten()
            .map(|v| v == "1")
            .unwrap_or(false);
        let months = self
            .get_meta("cleanup_retention_months")
            .await
            .ok()
            .flatten()
            .and_then(|v| v.parse::<u32>().ok())
            .unwrap_or(1);
        (enabled, months)
    }

    /// Persist the auto-cleanup settings.
    pub async fn set_auto_cleanup_settings(&self, enabled: bool, months: u32) -> Result<()> {
        self.set_meta("cleanup_enabled", if enabled { "1" } else { "0" }).await?;
        self.set_meta("cleanup_retention_months", &months.to_string()).await?;
        Ok(())
    }
}

#[cfg(test)]
mod token_tests {
    use super::*;
    use sqlx::Row;

    #[test]
    fn usage_tokens_extracts_assistant_usage() {
        let body = r#"{"message":{"usage":{"input_tokens":5,"output_tokens":7,"cache_creation_input_tokens":3,"cache_read_input_tokens":11}}}"#;
        assert_eq!(usage_tokens("assistant", body), (Some(5), Some(7), Some(3), Some(11)));
    }

    #[test]
    fn usage_tokens_non_assistant_is_none() {
        let body = r#"{"message":{"content":"hi"}}"#;
        assert_eq!(usage_tokens("user", body), (None, None, None, None));
    }

    #[test]
    fn usage_tokens_missing_usage_defaults_zero() {
        let body = r#"{"message":{"content":[]}}"#;
        assert_eq!(usage_tokens("assistant", body), (Some(0), Some(0), Some(0), Some(0)));
    }

    async fn temp_db() -> Db {
        // Unique per call: parallel tests share a pid (and can collide on a
        // coarse clock), so an atomic sequence guarantees distinct files.
        use std::sync::atomic::{AtomicU64, Ordering};
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let seq = SEQ.fetch_add(1, Ordering::Relaxed);
        let path =
            std::env::temp_dir().join(format!("cc-test-{}-{}.db", std::process::id(), seq));
        Db::open(&path).await.unwrap()
    }

    fn msg(kind: &str, ts: i64, body: &str) -> MessageRow {
        MessageRow {
            cc_session_id: "s".into(),
            claude_session_id: "c".into(),
            ts,
            kind: kind.into(),
            body: body.into(),
        }
    }

    #[tokio::test]
    async fn insert_populates_token_columns_and_sums() {
        let db = temp_db().await;
        db.insert_message(&msg(
            "assistant",
            1000,
            r#"{"message":{"usage":{"input_tokens":10,"output_tokens":20,"cache_creation_input_tokens":1,"cache_read_input_tokens":2}}}"#,
        ))
        .await;
        db.insert_message(&msg(
            "assistant",
            1000,
            r#"{"message":{"usage":{"input_tokens":5,"output_tokens":6,"cache_creation_input_tokens":0,"cache_read_input_tokens":0}}}"#,
        ))
        .await;
        db.insert_message(&msg("user", 1000, r#"{"message":{"content":"hi"}}"#))
            .await;

        // Mirrors the tokens-daily query: index-only SUM over assistant rows.
        let row = sqlx::query(
            "SELECT SUM(input_tokens) AS i, SUM(output_tokens) AS o,
                    SUM(cache_creation_tokens) AS cc, SUM(cache_read_tokens) AS cr
               FROM messages WHERE ts >= ?1 AND kind = 'assistant'",
        )
        .bind(0i64)
        .fetch_one(&db.pool)
        .await
        .unwrap();
        assert_eq!(row.get::<i64, _>("i"), 15);
        assert_eq!(row.get::<i64, _>("o"), 26);
        assert_eq!(row.get::<i64, _>("cc"), 1);
        assert_eq!(row.get::<i64, _>("cr"), 2);
    }

    #[tokio::test]
    async fn backfill_populates_legacy_null_rows() {
        let db = temp_db().await;
        // Simulate a pre-migration assistant row: token columns left NULL.
        sqlx::query(
            "INSERT INTO messages (cc_session_id, claude_session_id, ts, kind, body)
             VALUES ('s','c',1000,'assistant',?1)",
        )
        .bind(r#"{"message":{"usage":{"input_tokens":42,"output_tokens":1,"cache_creation_input_tokens":0,"cache_read_input_tokens":0}}}"#)
        .execute(&db.pool)
        .await
        .unwrap();
        // Run the backfill UPDATE (same statement spawn_token_backfill uses).
        sqlx::query(
            "UPDATE messages
                SET input_tokens          = COALESCE(CAST(json_extract(body,'$.message.usage.input_tokens') AS INTEGER), 0),
                    output_tokens         = COALESCE(CAST(json_extract(body,'$.message.usage.output_tokens') AS INTEGER), 0),
                    cache_creation_tokens = COALESCE(CAST(json_extract(body,'$.message.usage.cache_creation_input_tokens') AS INTEGER), 0),
                    cache_read_tokens     = COALESCE(CAST(json_extract(body,'$.message.usage.cache_read_input_tokens') AS INTEGER), 0)
              WHERE kind = 'assistant' AND input_tokens IS NULL",
        )
        .execute(&db.pool)
        .await
        .unwrap();
        let row = sqlx::query("SELECT input_tokens AS i FROM messages WHERE ts = 1000")
            .fetch_one(&db.pool)
            .await
            .unwrap();
        assert_eq!(row.get::<i64, _>("i"), 42);
    }

    #[tokio::test]
    async fn cleanup_deletes_old_keeps_recent_and_handles_ms() {
        let db = temp_db().await;
        let cutoff = 1_000_000i64;
        let usage = r#"{"message":{"usage":{"input_tokens":1,"output_tokens":1,"cache_creation_input_tokens":0,"cache_read_input_tokens":0}}}"#;
        // messages: one older than cutoff, one newer (seconds).
        db.insert_message(&msg("assistant", cutoff - 10, usage)).await;
        db.insert_message(&msg("assistant", cutoff + 10, usage)).await;
        // user_interactions stores ts in MILLISECONDS — exercise the *1000.
        for (ts_ms, who) in [((cutoff - 10) * 1000, "old"), ((cutoff + 10) * 1000, "new")] {
            sqlx::query(
                "INSERT INTO user_interactions
                     (account, agent, workspace, claude_session_id, ts_ms, kind, content)
                 VALUES ('a','ag','w','c',?1,'k',?2)",
            )
            .bind(ts_ms)
            .bind(who)
            .execute(&db.pool)
            .await
            .unwrap();
        }

        let counts = db.cleanup_counts(cutoff).await.unwrap();
        assert_eq!(counts.messages, 1);
        assert_eq!(counts.user_interactions, 1);

        let deleted = db.cleanup_delete(cutoff).await.unwrap();
        assert_eq!(deleted.messages, 1);
        assert_eq!(deleted.user_interactions, 1);

        // The recent rows survive.
        let m: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM messages")
            .fetch_one(&db.pool)
            .await
            .unwrap();
        let u: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM user_interactions")
            .fetch_one(&db.pool)
            .await
            .unwrap();
        assert_eq!(m, 1);
        assert_eq!(u, 1);

        db.vacuum().await.unwrap();
    }

    #[tokio::test]
    async fn stats_daily_rollup_aggregates_by_day() {
        let db = temp_db().await;
        let usage = |i: i64, o: i64| {
            format!(
                r#"{{"message":{{"usage":{{"input_tokens":{i},"output_tokens":{o},"cache_creation_input_tokens":0,"cache_read_input_tokens":0}}}}}}"#
            )
        };
        // Two assistant + one user message, all on the same UTC day.
        let day_ts = 1_700_000_000i64; // fixed, well-defined day
        db.insert_message(&msg("assistant", day_ts, &usage(10, 20))).await;
        db.insert_message(&msg("assistant", day_ts + 5, &usage(5, 6))).await;
        db.insert_message(&msg("user", day_ts + 1, r#"{"message":{"content":"hi"}}"#))
            .await;

        db.refresh_stats_daily(0, day_ts + 100).await.unwrap();

        let day = chrono::DateTime::<chrono::Utc>::from_timestamp(day_ts, 0)
            .unwrap()
            .format("%Y-%m-%d")
            .to_string();
        let row = sqlx::query(
            "SELECT user_msgs, assistant_msgs, other_msgs, input_tokens, output_tokens
               FROM stats_daily WHERE day = ?1",
        )
        .bind(&day)
        .fetch_one(&db.pool)
        .await
        .unwrap();
        assert_eq!(row.get::<i64, _>("user_msgs"), 1);
        assert_eq!(row.get::<i64, _>("assistant_msgs"), 2);
        assert_eq!(row.get::<i64, _>("other_msgs"), 0);
        assert_eq!(row.get::<i64, _>("input_tokens"), 15);
        assert_eq!(row.get::<i64, _>("output_tokens"), 26);

        // Idempotent re-run yields the same numbers (INSERT OR REPLACE).
        db.refresh_stats_daily(0, day_ts + 200).await.unwrap();
        let again: i64 =
            sqlx::query_scalar("SELECT assistant_msgs FROM stats_daily WHERE day = ?1")
                .bind(&day)
                .fetch_one(&db.pool)
                .await
                .unwrap();
        assert_eq!(again, 2);
    }

    #[tokio::test]
    async fn auto_cleanup_settings_roundtrip() {
        let db = temp_db().await;
        // Default when unset: disabled, 1 month.
        assert_eq!(db.auto_cleanup_settings().await, (false, 1));
        db.set_auto_cleanup_settings(true, 3).await.unwrap();
        assert_eq!(db.auto_cleanup_settings().await, (true, 3));
    }
}
