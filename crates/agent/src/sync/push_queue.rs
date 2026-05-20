//! SQLite-backed FIFO push queue for the workspace sync engine.
//!
//! The watcher enqueues file pushes / deletes here; the (not yet
//! implemented) push worker drains the queue and ships frames to the
//! hub. The queue lives on disk so an agent restart — or a long
//! network outage — doesn't lose pending work.
//!
//! Schema is intentionally tiny: one row per operation, in insertion
//! order. `coalesce_path` collapses redundant entries for the same
//! file so a noisy editor (think Vim's swap-file dance) doesn't push
//! the same content ten times in a row.
//!
//! Concurrency: a single sqlx `SqlitePool` is shared. All writes go
//! through that pool; SQLite serializes them internally. The watcher
//! and push worker can call into this type from any task without
//! external locking.

use anyhow::{Context, Result};
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions};
use sqlx::{Row, SqlitePool};
use std::path::{Path, PathBuf};
use std::str::FromStr;

/// One unit of work waiting to be shipped to the hub.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QueueOp {
    PushFile {
        account: String,
        workspace: String,
        path: String,
        content: Vec<u8>,
    },
    DeleteFile {
        account: String,
        workspace: String,
        path: String,
    },
}

impl QueueOp {
    fn account(&self) -> &str {
        match self {
            QueueOp::PushFile { account, .. } | QueueOp::DeleteFile { account, .. } => account,
        }
    }
    fn workspace(&self) -> &str {
        match self {
            QueueOp::PushFile { workspace, .. } | QueueOp::DeleteFile { workspace, .. } => {
                workspace
            }
        }
    }
    fn path(&self) -> &str {
        match self {
            QueueOp::PushFile { path, .. } | QueueOp::DeleteFile { path, .. } => path,
        }
    }
    fn op_kind(&self) -> &'static str {
        match self {
            QueueOp::PushFile { .. } => "push",
            QueueOp::DeleteFile { .. } => "delete",
        }
    }
}

/// Default location for the queue db, derived from `update::state_dir()`.
/// Returns `None` if no state dir can be determined (e.g. no `HOME` set).
pub fn default_db_path() -> Option<PathBuf> {
    crate::update::state_dir().map(|d| d.join("agent").join("sync").join("push_queue.db"))
}

pub struct PushQueue {
    db: SqlitePool,
}

impl PushQueue {
    /// Open (or create) the queue db at `db_path`. Creates the parent
    /// directory if needed. WAL journal mode so the watcher and push
    /// worker don't block each other.
    pub async fn open(db_path: &Path) -> Result<Self> {
        if let Some(parent) = db_path.parent() {
            if !parent.as_os_str().is_empty() && !parent.exists() {
                std::fs::create_dir_all(parent)
                    .with_context(|| format!("creating queue dir {}", parent.display()))?;
            }
        }
        let dsn = format!("sqlite://{}", db_path.display());
        let opts = SqliteConnectOptions::from_str(&dsn)?
            .create_if_missing(true)
            .journal_mode(SqliteJournalMode::Wal)
            .busy_timeout(std::time::Duration::from_secs(5));
        let pool = SqlitePoolOptions::new()
            .max_connections(4)
            .connect_with(opts)
            .await
            .with_context(|| format!("opening push queue at {}", db_path.display()))?;

        let queue = Self { db: pool };
        queue.run_migrations().await?;
        Ok(queue)
    }

    async fn run_migrations(&self) -> Result<()> {
        let stmts = [
            "CREATE TABLE IF NOT EXISTS push_queue (
                id           INTEGER PRIMARY KEY AUTOINCREMENT,
                account      TEXT NOT NULL,
                workspace    TEXT NOT NULL,
                path         TEXT NOT NULL,
                op           TEXT NOT NULL,
                content      BLOB,
                enqueued_at  INTEGER NOT NULL
            )",
            "CREATE INDEX IF NOT EXISTS idx_push_queue_account_workspace
                 ON push_queue(account, workspace, id)",
        ];
        for sql in stmts {
            sqlx::query(sql)
                .execute(&self.db)
                .await
                .with_context(|| format!("migrate: {}", sql.split_whitespace().take(4).collect::<Vec<_>>().join(" ")))?;
        }
        Ok(())
    }

    /// Append an operation. Returns the row id.
    pub async fn enqueue(&self, op: QueueOp) -> Result<u64> {
        let now = chrono::Utc::now().timestamp();
        let (content, content_is_some) = match &op {
            QueueOp::PushFile { content, .. } => (content.clone(), true),
            QueueOp::DeleteFile { .. } => (Vec::new(), false),
        };
        let mut q = sqlx::query(
            "INSERT INTO push_queue (account, workspace, path, op, content, enqueued_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        )
        .bind(op.account())
        .bind(op.workspace())
        .bind(op.path())
        .bind(op.op_kind());
        q = if content_is_some {
            q.bind(content)
        } else {
            q.bind(Option::<Vec<u8>>::None)
        };
        let res = q
            .bind(now)
            .execute(&self.db)
            .await
            .context("enqueue push op")?;
        Ok(res.last_insert_rowid() as u64)
    }

    /// Return the oldest `limit` operations, ascending by id. Does
    /// **not** remove them — the worker calls [`ack`] once the hub has
    /// confirmed receipt.
    /// Peek the oldest queued rows for a given (account, workspace).
    /// Scoped at the SQL level rather than in the caller because the
    /// queue is shared across all sessions on this agent — without the
    /// filter, an orphan row from a previous session sitting at the
    /// head silently starves every subsequent scan.
    pub async fn peek_oldest(
        &self,
        account: &str,
        workspace: &str,
        limit: usize,
    ) -> Result<Vec<(u64, QueueOp)>> {
        let rows = sqlx::query(
            "SELECT id, account, workspace, path, op, content
               FROM push_queue
              WHERE account = ?1 AND workspace = ?2
              ORDER BY id ASC
              LIMIT ?3",
        )
        .bind(account)
        .bind(workspace)
        .bind(limit as i64)
        .fetch_all(&self.db)
        .await
        .context("peek push queue")?;

        let mut out = Vec::with_capacity(rows.len());
        for r in rows {
            let id: i64 = r.get("id");
            let account: String = r.get("account");
            let workspace: String = r.get("workspace");
            let path: String = r.get("path");
            let op: String = r.get("op");
            let content: Option<Vec<u8>> = r.get("content");
            let op = match op.as_str() {
                "push" => QueueOp::PushFile {
                    account,
                    workspace,
                    path,
                    content: content.unwrap_or_default(),
                },
                "delete" => QueueOp::DeleteFile {
                    account,
                    workspace,
                    path,
                },
                other => {
                    return Err(anyhow::anyhow!(
                        "push_queue row {id} has unknown op {other:?}"
                    ));
                }
            };
            out.push((id as u64, op));
        }
        Ok(out)
    }

    /// Mark a row done. No-op if the row is already gone.
    pub async fn ack(&self, id: u64) -> Result<()> {
        sqlx::query("DELETE FROM push_queue WHERE id = ?1")
            .bind(id as i64)
            .execute(&self.db)
            .await
            .context("ack push op")?;
        Ok(())
    }

    /// Total queue depth. Currently only exercised in unit tests; kept
    /// public for future telemetry / admin endpoints.
    #[allow(dead_code)]
    pub async fn len(&self) -> Result<u64> {
        let row = sqlx::query("SELECT COUNT(*) AS n FROM push_queue")
            .fetch_one(&self.db)
            .await
            .context("len push queue")?;
        Ok(row.get::<i64, _>("n") as u64)
    }

    /// Drop every row for `(account, workspace, path)` **except the
    /// most recent one**. Call this right after `enqueue` when the
    /// caller knows the new op fully supersedes any older op on the
    /// same path (e.g. a fresh "push" overwrites an earlier "push" or
    /// "delete" — the newest content is authoritative).
    ///
    /// Returns the number of rows deleted.
    pub async fn coalesce_path(
        &self,
        account: &str,
        workspace: &str,
        path: &str,
    ) -> Result<u64> {
        let res = sqlx::query(
            "DELETE FROM push_queue
              WHERE account = ?1 AND workspace = ?2 AND path = ?3
                AND id < (
                    SELECT MAX(id) FROM push_queue
                     WHERE account = ?1 AND workspace = ?2 AND path = ?3
                )",
        )
        .bind(account)
        .bind(workspace)
        .bind(path)
        .execute(&self.db)
        .await
        .context("coalesce push queue")?;
        Ok(res.rows_affected())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn fresh_queue() -> (tempfile::TempDir, PushQueue) {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("q.db");
        let q = PushQueue::open(&db_path).await.unwrap();
        (dir, q)
    }

    fn push(account: &str, ws: &str, path: &str, body: &[u8]) -> QueueOp {
        QueueOp::PushFile {
            account: account.into(),
            workspace: ws.into(),
            path: path.into(),
            content: body.to_vec(),
        }
    }
    fn del(account: &str, ws: &str, path: &str) -> QueueOp {
        QueueOp::DeleteFile {
            account: account.into(),
            workspace: ws.into(),
            path: path.into(),
        }
    }

    #[tokio::test]
    async fn enqueue_peek_ack_roundtrip() {
        let (_d, q) = fresh_queue().await;
        let id1 = q.enqueue(push("alice", "ws1", "src/a.rs", b"one")).await.unwrap();
        let id2 = q.enqueue(del("alice", "ws1", "src/b.rs")).await.unwrap();

        assert_eq!(q.len().await.unwrap(), 2);

        let head = q.peek_oldest("alice", "ws1", 10).await.unwrap();
        assert_eq!(head.len(), 2);
        assert_eq!(head[0].0, id1);
        assert_eq!(head[1].0, id2);
        match &head[0].1 {
            QueueOp::PushFile { path, content, .. } => {
                assert_eq!(path, "src/a.rs");
                assert_eq!(content, b"one");
            }
            other => panic!("expected push, got {other:?}"),
        }
        assert!(matches!(head[1].1, QueueOp::DeleteFile { .. }));

        q.ack(id1).await.unwrap();
        assert_eq!(q.len().await.unwrap(), 1);
        let head = q.peek_oldest("alice", "ws1", 10).await.unwrap();
        assert_eq!(head.len(), 1);
        assert_eq!(head[0].0, id2);
    }

    #[tokio::test]
    async fn ack_unknown_id_is_noop() {
        let (_d, q) = fresh_queue().await;
        // No row 999; should not error.
        q.ack(999).await.unwrap();
    }

    #[tokio::test]
    async fn coalesce_keeps_newest_only() {
        let (_d, q) = fresh_queue().await;
        let _ = q.enqueue(push("alice", "ws1", "a.rs", b"v1")).await.unwrap();
        let _ = q.enqueue(push("alice", "ws1", "a.rs", b"v2")).await.unwrap();
        let id3 = q.enqueue(push("alice", "ws1", "a.rs", b"v3")).await.unwrap();
        // Unrelated path is untouched.
        let id_other = q.enqueue(push("alice", "ws1", "b.rs", b"x")).await.unwrap();

        let deleted = q.coalesce_path("alice", "ws1", "a.rs").await.unwrap();
        assert_eq!(deleted, 2);
        assert_eq!(q.len().await.unwrap(), 2);

        let head = q.peek_oldest("alice", "ws1", 10).await.unwrap();
        // Surviving rows: the newest "a.rs" (id3) and "b.rs" (id_other).
        let ids: Vec<u64> = head.iter().map(|(i, _)| *i).collect();
        assert!(ids.contains(&id3));
        assert!(ids.contains(&id_other));

        // Newest a.rs has content "v3", not "v1" or "v2".
        for (_, op) in &head {
            if let QueueOp::PushFile { path, content, .. } = op {
                if path == "a.rs" {
                    assert_eq!(content, b"v3");
                }
            }
        }
    }

    #[tokio::test]
    async fn coalesce_scopes_per_account_and_workspace() {
        let (_d, q) = fresh_queue().await;
        q.enqueue(push("alice", "ws1", "a.rs", b"alice")).await.unwrap();
        q.enqueue(push("bob", "ws1", "a.rs", b"bob")).await.unwrap();
        q.enqueue(push("alice", "ws2", "a.rs", b"otherws")).await.unwrap();

        // Coalesce alice/ws1 — only one row matches, nothing to drop.
        let deleted = q.coalesce_path("alice", "ws1", "a.rs").await.unwrap();
        assert_eq!(deleted, 0);
        assert_eq!(q.len().await.unwrap(), 3);
    }

    #[tokio::test]
    async fn fifo_ordering_preserved() {
        let (_d, q) = fresh_queue().await;
        let mut ids = vec![];
        for i in 0..5 {
            let path = format!("file{i}.txt");
            ids.push(q.enqueue(push("a", "ws", &path, &[i as u8])).await.unwrap());
        }
        let head = q.peek_oldest("a", "ws", 10).await.unwrap();
        let got: Vec<u64> = head.iter().map(|(i, _)| *i).collect();
        assert_eq!(got, ids);
    }

    #[tokio::test]
    async fn persistence_across_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("q.db");

        {
            let q = PushQueue::open(&db_path).await.unwrap();
            q.enqueue(push("alice", "ws1", "x.rs", b"persist me")).await.unwrap();
            q.enqueue(del("alice", "ws1", "y.rs")).await.unwrap();
            assert_eq!(q.len().await.unwrap(), 2);
        }
        // Drop pool, reopen.
        let q2 = PushQueue::open(&db_path).await.unwrap();
        assert_eq!(q2.len().await.unwrap(), 2);
        let head = q2.peek_oldest("alice", "ws1", 10).await.unwrap();
        assert_eq!(head.len(), 2);
        match &head[0].1 {
            QueueOp::PushFile { path, content, .. } => {
                assert_eq!(path, "x.rs");
                assert_eq!(content, b"persist me");
            }
            other => panic!("expected push, got {other:?}"),
        }
    }

    /// Regression: the queue is shared across all sessions on an
    /// agent, so peek_oldest MUST filter by (account, workspace) in
    /// SQL. The previous global-ORDER-BY behaviour meant an orphan
    /// row at the head from a different session would be returned
    /// every scan, the caller would skip it, and rows for the
    /// current session would never be peeked — silent starvation.
    #[tokio::test]
    async fn peek_oldest_skips_other_sessions_rows() {
        let (_d, q) = fresh_queue().await;
        // Lay down an orphan row first so it has the lowest id.
        let orphan_id = q
            .enqueue(push("bob", "wsX", "stale.rs", b"old"))
            .await
            .unwrap();
        // Then enqueue something for the session we're peeking on.
        let mine_id = q
            .enqueue(push("alice", "ws1", "mine.rs", b"new"))
            .await
            .unwrap();
        // Peek as alice/ws1 — orphan must not appear, mine must.
        let head = q.peek_oldest("alice", "ws1", 10).await.unwrap();
        assert_eq!(head.len(), 1);
        assert_eq!(head[0].0, mine_id);
        // Sanity: peek as bob/wsX sees only its own orphan.
        let other = q.peek_oldest("bob", "wsX", 10).await.unwrap();
        assert_eq!(other.len(), 1);
        assert_eq!(other[0].0, orphan_id);
    }

    #[tokio::test]
    async fn peek_respects_limit() {
        let (_d, q) = fresh_queue().await;
        for i in 0..10 {
            q.enqueue(push("a", "ws", &format!("f{i}"), b"x")).await.unwrap();
        }
        let head = q.peek_oldest("a", "ws", 3).await.unwrap();
        assert_eq!(head.len(), 3);
    }
}
