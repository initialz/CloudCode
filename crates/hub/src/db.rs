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

    // ---- audit ---------------------------------------------------------

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
