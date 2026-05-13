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
use sqlx::{Row, SqlitePool};
use std::path::Path;
use std::str::FromStr;

#[derive(Clone)]
pub struct Db {
    pub pool: SqlitePool,
}

#[derive(Debug, Clone)]
pub struct DbAccount {
    pub name: String,
    pub token_hash: String,
    pub token_prefix: Option<String>,
    pub created_at: i64,
    pub disabled: bool,
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
            .busy_timeout(std::time::Duration::from_secs(5));
        let pool = SqlitePoolOptions::new()
            .max_connections(8)
            .connect_with(opts)
            .await
            .with_context(|| format!("opening sqlite at {}", path.display()))?;
        let db = Self { pool };
        db.run_migrations().await?;
        Ok(db)
    }

    async fn run_migrations(&self) -> Result<()> {
        // No external migration tool — hub owns its schema. Each statement
        // is idempotent (`IF NOT EXISTS`) so an existing db just gets the
        // new objects on upgrade.
        let stmts = [
            "CREATE TABLE IF NOT EXISTS accounts (
                name         TEXT PRIMARY KEY,
                token_hash   TEXT NOT NULL,
                token_prefix TEXT,
                created_at   INTEGER NOT NULL,
                disabled     INTEGER NOT NULL DEFAULT 0
            )",
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
            "CREATE TABLE IF NOT EXISTS messages (
                id                INTEGER PRIMARY KEY AUTOINCREMENT,
                cc_session_id     TEXT NOT NULL,
                claude_session_id TEXT NOT NULL,
                ts                INTEGER NOT NULL,
                kind              TEXT NOT NULL,
                body              TEXT NOT NULL
            )",
            "CREATE INDEX IF NOT EXISTS idx_messages_cc_session ON messages(cc_session_id, id)",
            "CREATE INDEX IF NOT EXISTS idx_messages_claude_session ON messages(claude_session_id, id)",
            "CREATE INDEX IF NOT EXISTS idx_messages_ts ON messages(ts DESC)",
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
            // (account, agent) pair, otherwise the account is denied. An
            // account with zero rows cannot select any agent (admin must
            // explicitly grant access from the admin UI). To keep upgrades
            // from breaking existing deployments, the seed below grants
            // each pre-existing account everything it has historically
            // connected to (derived from the sessions table). The
            // INSERT OR IGNORE is harmless on subsequent hub starts.
            "CREATE TABLE IF NOT EXISTS account_allowed_agents (
                account TEXT NOT NULL,
                agent   TEXT NOT NULL,
                PRIMARY KEY (account, agent)
            )",
            "INSERT OR IGNORE INTO account_allowed_agents (account, agent)
                SELECT DISTINCT s.account, s.agent
                  FROM sessions s
                  JOIN accounts a ON a.name = s.account",
        ];
        for sql in stmts {
            sqlx::query(sql)
                .execute(&self.pool)
                .await
                .with_context(|| format!("migrate: {}", sql.split_whitespace().take(4).collect::<Vec<_>>().join(" ")))?;
        }
        Ok(())
    }

    // ---- accounts ------------------------------------------------------

    pub async fn account_count(&self) -> Result<i64> {
        let row = sqlx::query("SELECT COUNT(*) AS n FROM accounts")
            .fetch_one(&self.pool)
            .await?;
        Ok(row.get::<i64, _>("n"))
    }

    pub async fn list_accounts(&self) -> Result<Vec<DbAccount>> {
        let rows = sqlx::query(
            "SELECT name, token_hash, token_prefix, created_at, disabled
             FROM accounts ORDER BY name",
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|r| DbAccount {
                name: r.get("name"),
                token_hash: r.get("token_hash"),
                token_prefix: r.get("token_prefix"),
                created_at: r.get("created_at"),
                disabled: r.get::<i64, _>("disabled") != 0,
            })
            .collect())
    }

    pub async fn insert_account(
        &self,
        name: &str,
        token_hash: &str,
        token_prefix: Option<&str>,
    ) -> Result<()> {
        sqlx::query(
            "INSERT INTO accounts (name, token_hash, token_prefix, created_at, disabled)
             VALUES (?1, ?2, ?3, ?4, 0)",
        )
        .bind(name)
        .bind(token_hash)
        .bind(token_prefix)
        .bind(chrono::Utc::now().timestamp())
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

    pub async fn delete_account(&self, name: &str) -> Result<()> {
        let rows = sqlx::query("DELETE FROM accounts WHERE name = ?1")
            .bind(name)
            .execute(&self.pool)
            .await?
            .rows_affected();
        if rows == 0 {
            anyhow::bail!("account '{}' not found", name);
        }
        Ok(())
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

    /// Union of every agent name we've ever seen — historical sessions
    /// plus current allowlist entries. Used by the admin UI to render
    /// the "known agents" multi-select even when an agent is offline.
    pub async fn distinct_known_agents(&self) -> Result<Vec<String>> {
        let rows = sqlx::query(
            "SELECT DISTINCT agent FROM (
                SELECT agent FROM sessions
                UNION
                SELECT agent FROM account_allowed_agents
            ) ORDER BY agent",
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(|r| r.get("agent")).collect())
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
    /// (agent dedupes via jsonl_progress offsets).
    pub async fn insert_message(&self, row: &MessageRow) {
        let res = sqlx::query(
            "INSERT INTO messages (cc_session_id, claude_session_id, ts, kind, body)
             VALUES (?1, ?2, ?3, ?4, ?5)",
        )
        .bind(&row.cc_session_id)
        .bind(&row.claude_session_id)
        .bind(row.ts)
        .bind(&row.kind)
        .bind(&row.body)
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

#[derive(Debug, Clone, Default)]
pub struct AuditFilter {
    pub account: Option<String>,
    pub agent: Option<String>,
    pub kind: Option<String>,
    pub since: Option<i64>,
    pub until: Option<i64>,
}
