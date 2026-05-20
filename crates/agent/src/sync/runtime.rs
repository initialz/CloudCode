//! Per-session push worker that bridges the watcher + push queue to
//! the WS tunnel.
//!
//! One [`run_push_worker`] task is spawned per active PtyOpen (i.e.
//! per (session_id, account, workspace)). It owns three inputs:
//!
//! 1. The watcher's `WatchEvent` receiver — turns filesystem changes
//!    into [`QueueOp`]s and persists them via [`PushQueue::enqueue`]
//!    after coalescing redundant entries on the same path.
//! 2. A periodic queue scan — pulls the oldest `PEEK_BATCH` ops not
//!    yet "in flight" and ships them over the WS tx, recording the
//!    queue id in a `pending` map keyed by path.
//! 3. An ack channel — the WS read loop forwards every
//!    `WorkspaceFileAck` for this session here so the worker can
//!    `queue.ack(id)` the matching row.
//!
//! Why one task instead of three?
//!   - The `pending` map is the only piece of mutable state shared
//!     between "ship a frame" and "receive an ack". Keeping it in one
//!     task means no `Mutex` / `RwLock` — `tokio::select!` on the
//!     three sources is enough.
//!   - It also means cancellation is trivial: drop the shutdown
//!     `mpsc::Sender` and the task exits on the next select! tick.
//!
//! Reliability notes:
//!   - The queue is durable. If the agent crashes or the WS drops
//!     mid-send, the next worker boot picks up where the old one
//!     stopped — `pending` is in-memory, but the row is still in
//!     SQLite, so the worst case is one duplicate push per outstanding
//!     ack (hub-side is idempotent on `(account, workspace, path)`).
//!   - On `ok = false` we leave the row alone; the next scan will
//!     resend it. Worst case is a tight loop on a permanently-failing
//!     push; we mitigate that with an exponential backoff between
//!     scans whenever the last scan saw any failures.

use crate::pty::OutFrame;
use crate::sync::ignore_filter::IgnoreFilter;
use crate::sync::push_queue::{PushQueue, QueueOp};
use crate::sync::watcher::WatchEvent;
use crate::tunnel::ClientMsg;
use std::collections::HashMap;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use uuid::Uuid;

/// Result of a single push ack, routed by the WS read loop to the
/// worker that owns the session.
#[derive(Debug, Clone)]
pub struct AckMsg {
    pub path: String,
    pub ok: bool,
    pub error: Option<String>,
}

/// How many rows the worker drags out of SQLite per scan.
const PEEK_BATCH: usize = 50;
/// Quiet poll cadence when there's nothing to send / no events
/// arriving. The watcher pushes events through the same select, so
/// this only matters as a backstop in case the watcher misses or
/// the queue still has entries from a previous session.
const SCAN_INTERVAL: Duration = Duration::from_millis(500);
/// Backoff applied after a scan that saw at least one `ok = false`
/// ack — keeps a permanently-failing push from busy-looping the WS.
const BACKOFF_AFTER_FAILURE: Duration = Duration::from_secs(2);

/// Inputs the worker drives. Kept as a struct so `PtyManager` can
/// build the bundle once and hand it off into `tokio::spawn`.
pub struct PushWorker {
    pub session_id: Uuid,
    pub account: String,
    pub workspace: String,
    pub workspace_root: PathBuf,
    /// Shared with the watcher — used inside `enqueue_subtree` to skip
    /// noisy entries (`.git/`, `node_modules/`, `target/`, anything the
    /// per-workspace `.cloudcodeignore` lists) during a directory
    /// fan-out walk. Without this, a single `Changed { path: "node_modules" }`
    /// event would push thousands of files we'd never want on the hub.
    pub ignore_filter: Arc<IgnoreFilter>,
    pub queue: Arc<PushQueue>,
    pub watch_rx: mpsc::Receiver<WatchEvent>,
    pub ack_rx: mpsc::Receiver<AckMsg>,
    pub shutdown_rx: mpsc::Receiver<()>,
    /// Live ws tx slot owned by `PtyManager`. We snapshot on each
    /// send so a transient WS reconnect doesn't permanently sink
    /// pushes — the row stays in the queue and gets re-shipped on
    /// the next scan after a new tx has been bound.
    pub ws_tx_slot: Arc<std::sync::RwLock<Option<mpsc::Sender<OutFrame>>>>,
}

/// Drive the loop until shutdown / channels drained.
///
/// The task exits when:
///   - the shutdown channel fires (PtyClose / reader EOF), OR
///   - the WS tx is closed (writer task gone — hub disconnect), OR
///   - both the watcher and ack channels are closed (caller dropped
///     everything; treat the same as shutdown).
pub async fn run_push_worker(mut w: PushWorker) {
    let mut pending: HashMap<String, u64> = HashMap::new();
    let mut tick = tokio::time::interval(SCAN_INTERVAL);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut next_scan_delay: Option<Duration> = None;

    loop {
        // Honour any pending backoff. We use a one-shot sleep instead of
        // mutating the interval so a watcher event during the backoff
        // still wakes us promptly.
        let backoff_sleep: futures::future::BoxFuture<()> = match next_scan_delay.take() {
            Some(d) => Box::pin(tokio::time::sleep(d)),
            None => Box::pin(futures::future::pending()),
        };
        tokio::select! {
            // Biased so the periodic scan + shutdown are polled
            // before watch_rx. Without this, a fan-out that enqueues
            // 1000+ files in one go starves `tick.tick()`: `select!`
            // sees watch_rx perpetually ready and keeps picking it,
            // and `MissedTickBehavior::Delay` only re-arms the tick
            // *after* we re-enter the loop. End result: 2000+
            // enqueues without a single `scan_and_send` invocation.
            biased;
            _ = w.shutdown_rx.recv() => {
                tracing::debug!(session = %w.session_id, "push worker: shutdown");
                break;
            }
            _ = tick.tick() => {
                if let Err(e) = scan_and_send(&mut w, &mut pending).await {
                    tracing::warn!(session = %w.session_id, error = %e, "push worker: scan");
                }
            }
            _ = backoff_sleep => {
                // Backoff elapsed; fall through to the scan tick below
                // by re-arming the interval and continuing.
                tick.reset();
            }
            ack = w.ack_rx.recv() => {
                let Some(ack) = ack else {
                    // The session is being torn down by ws read loop.
                    // Stop accepting new acks but keep the worker alive
                    // for shutdown_rx to fire.
                    continue;
                };
                if let Some(id) = pending.remove(&ack.path) {
                    if ack.ok {
                        if let Err(e) = w.queue.ack(id).await {
                            tracing::warn!(session = %w.session_id, error = %e, "push worker: queue.ack");
                        }
                    } else {
                        tracing::warn!(
                            session = %w.session_id,
                            path = %ack.path,
                            err = ?ack.error,
                            "push worker: ack reported failure; will retry"
                        );
                        next_scan_delay = Some(BACKOFF_AFTER_FAILURE);
                    }
                } else {
                    tracing::debug!(
                        session = %w.session_id,
                        path = %ack.path,
                        "push worker: ack for unknown path (already dequeued?)"
                    );
                }
            }
            evt = w.watch_rx.recv() => {
                let Some(evt) = evt else {
                    // Watcher gone -> sync engine stopped; we can still
                    // drain whatever's in the queue from acks coming in,
                    // but nothing fresh will arrive. Keep going until
                    // queue + ack channel are both empty.
                    if !drain_remaining(&mut w, &mut pending).await {
                        break;
                    }
                    continue;
                };
                if let Err(e) = handle_watch_event(&w, evt).await {
                    tracing::warn!(session = %w.session_id, error = %e, "push worker: handle watch event");
                }
                // Immediately attempt to ship any rows we just
                // enqueued. Without this, an inbound burst of watch
                // events (a `git clone` fans out 1000+ files) keeps
                // `watch_rx` perpetually ready, the biased select
                // never gets to `tick.tick()` until the burst dies
                // down, and shipping is delayed by an entire fan-out
                // worth of work.
                if let Err(e) = scan_and_send(&mut w, &mut pending).await {
                    tracing::warn!(session = %w.session_id, error = %e, "push worker: post-watch scan");
                }
            }
        }
    }

    tracing::debug!(session = %w.session_id, "push worker: exiting");
}

/// Persist one watcher event in the queue (with coalescing). Reads
/// file contents lazily so removed files don't trip on a missing read.
async fn handle_watch_event(w: &PushWorker, evt: WatchEvent) -> anyhow::Result<()> {
    tracing::debug!(session = %w.session_id, evt = ?evt, "watch event received");
    let (path, op) = match evt {
        WatchEvent::Changed { path } => {
            let abs = w
                .workspace_root
                .join(&w.account)
                .join(&w.workspace)
                .join(&path);
            // Directory event: walk the subtree and enqueue every
            // regular file inside that isn't already shipped. macOS
            // FSEvents notoriously coalesces "many files created at
            // once" (think `git clone`, `cargo build`, `tar -xf`)
            // into a single parent-directory event, so we cannot
            // rely on per-file Create events arriving for everything
            // underneath. Walking on-receipt fans the dir event back
            // out into per-file work.
            match tokio::fs::symlink_metadata(&abs).await {
                Ok(m) if m.is_dir() => {
                    return enqueue_subtree(w, &path).await;
                }
                _ => {}
            }
            // Read the file before enqueueing. If the file is gone by
            // the time we get here (race between event and read), treat
            // it as a delete instead — the watcher will catch up on
            // the next sweep.
            match tokio::fs::read(&abs).await {
                Ok(bytes) => {
                    let p = path.to_string_lossy().to_string();
                    (
                        p.clone(),
                        QueueOp::PushFile {
                            account: w.account.clone(),
                            workspace: w.workspace.clone(),
                            path: p,
                            content: bytes,
                        },
                    )
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    let p = path.to_string_lossy().to_string();
                    (
                        p.clone(),
                        QueueOp::DeleteFile {
                            account: w.account.clone(),
                            workspace: w.workspace.clone(),
                            path: p,
                        },
                    )
                }
                Err(e) => return Err(e.into()),
            }
        }
        WatchEvent::Removed { path } => {
            let p = path.to_string_lossy().to_string();
            (
                p.clone(),
                QueueOp::DeleteFile {
                    account: w.account.clone(),
                    workspace: w.workspace.clone(),
                    path: p,
                },
            )
        }
    };

    tracing::info!(session = %w.session_id, path = %path, "sync: enqueue push");
    w.queue.enqueue(op).await?;
    // Drop any older rows for this path now that the newest one is in.
    let _ = w
        .queue
        .coalesce_path(&w.account, &w.workspace, &path)
        .await?;
    Ok(())
}

/// Recursive enqueue used when a Changed event arrives for a
/// directory. Walks the subtree synchronously (FS calls go to a
/// blocking task to avoid stalling the watcher channel), enqueues
/// every regular file as a PushFile, and skips anything the ignore
/// filter rejects. Errors on individual files are logged at warn
/// and don't abort the rest of the walk — partial sync is better
/// than nothing.
async fn enqueue_subtree(w: &PushWorker, dir_rel: &Path) -> anyhow::Result<()> {
    tracing::info!(
        session = %w.session_id,
        dir = %dir_rel.display(),
        "sync: fan out directory event"
    );
    let root = w.workspace_root.join(&w.account).join(&w.workspace);
    let dir_abs = root.join(dir_rel);
    let root_for_walk = root.clone();
    let dir_for_walk = dir_abs.clone();
    let dir_rel_for_walk = dir_rel.to_path_buf();
    let session = w.session_id;
    let filter = w.ignore_filter.clone();
    // notify the WalkDir/walk crate isn't a workspace dep; use a
    // tiny hand-rolled stack walker on a blocking thread so we don't
    // bring in a new dep just for this.
    let walk = tokio::task::spawn_blocking(move || -> WalkResult {
        let mut result = WalkResult::default();
        let mut stack = vec![dir_for_walk];
        while let Some(d) = stack.pop() {
            // Skip a directory wholesale when the ignore filter
            // matches it — `.git/`, `node_modules/`, `target/` etc.
            // We pass `is_dir=true` so `node_modules/` style
            // patterns trigger correctly.
            if let Ok(rel_dir) = d.strip_prefix(&root_for_walk) {
                if !rel_dir.as_os_str().is_empty()
                    && filter.is_ignored(rel_dir, true)
                {
                    result.ignored_dirs += 1;
                    continue;
                }
            }
            let rd = match std::fs::read_dir(&d) {
                Ok(r) => r,
                Err(e) => {
                    tracing::warn!(
                        session = %session,
                        dir = %d.display(),
                        error = %e,
                        "subtree walk: read_dir"
                    );
                    result.read_dir_errors += 1;
                    continue;
                }
            };
            for entry in rd.flatten() {
                let p = entry.path();
                let ft = match entry.file_type() {
                    Ok(t) => t,
                    Err(_) => continue,
                };
                if ft.is_dir() {
                    stack.push(p);
                    continue;
                }
                if !ft.is_file() {
                    // Symlinks / sockets / fifos — hub doesn't model these.
                    result.skipped_special += 1;
                    continue;
                }
                let Ok(rel) = p.strip_prefix(&root_for_walk).map(|r| r.to_path_buf()) else {
                    continue;
                };
                if filter.is_ignored(&rel, false) {
                    result.ignored_files += 1;
                    continue;
                }
                match std::fs::read(&p) {
                    Ok(bytes) => {
                        result
                            .files
                            .push((rel.to_string_lossy().into_owned(), bytes));
                    }
                    Err(e) => {
                        tracing::warn!(
                            session = %session,
                            file = %p.display(),
                            error = %e,
                            "subtree walk: read"
                        );
                        result.read_errors += 1;
                    }
                }
            }
        }
        result.dir_rel = dir_rel_for_walk;
        result
    })
    .await
    .map_err(|e| anyhow::anyhow!("join walker: {e}"))?;

    tracing::info!(
        session = %w.session_id,
        dir = %walk.dir_rel.display(),
        enqueued = walk.files.len(),
        ignored_dirs = walk.ignored_dirs,
        ignored_files = walk.ignored_files,
        skipped_special = walk.skipped_special,
        read_errors = walk.read_errors,
        read_dir_errors = walk.read_dir_errors,
        "sync: fan out complete"
    );

    for (rel_path, bytes) in walk.files {
        let op = QueueOp::PushFile {
            account: w.account.clone(),
            workspace: w.workspace.clone(),
            path: rel_path.clone(),
            content: bytes,
        };
        w.queue.enqueue(op).await?;
        let _ = w
            .queue
            .coalesce_path(&w.account, &w.workspace, &rel_path)
            .await?;
    }
    Ok(())
}

#[derive(Default)]
struct WalkResult {
    dir_rel: PathBuf,
    files: Vec<(String, Vec<u8>)>,
    ignored_dirs: usize,
    ignored_files: usize,
    skipped_special: usize,
    read_errors: usize,
    read_dir_errors: usize,
}

/// Pull the oldest queue rows for this `(account, workspace)` and
/// ship anything that's not currently in flight. Returns Ok(()) on
/// success — the count is logged at trace level only.
async fn scan_and_send(
    w: &mut PushWorker,
    pending: &mut HashMap<String, u64>,
) -> anyhow::Result<()> {
    let rows = w
        .queue
        .peek_oldest(&w.account, &w.workspace, PEEK_BATCH)
        .await?;
    for (id, op) in rows {
        let path = match &op {
            QueueOp::PushFile { path, .. } | QueueOp::DeleteFile { path, .. } => path.as_str(),
        };
        if pending.contains_key(path) {
            // Already shipped, awaiting ack.
            continue;
        }
        // Send and remember.
        let frame = match &op {
            QueueOp::PushFile {
                path, content, ..
            } => ClientMsg::WorkspacePushFile {
                session_id: w.session_id,
                path: path.clone(),
                content: content.clone(),
            },
            QueueOp::DeleteFile { path, .. } => ClientMsg::WorkspaceDeleteFile {
                session_id: w.session_id,
                path: path.clone(),
            },
        };
        // Snapshot the current WS tx. If no WS is bound (mid-
        // reconnect) or the channel is closed, stop scanning — the
        // row stays in the durable queue and we'll retry on the
        // next tick after `ws.rs` binds a fresh tx.
        let tx_snap = w.ws_tx_slot.read().ok().and_then(|g| g.clone());
        let Some(tx) = tx_snap else {
            return Ok(());
        };
        if tx.send(OutFrame::Text(frame)).await.is_err() {
            return Ok(());
        }
        tracing::info!(session = %w.session_id, path = %path, "sync: sent push frame to hub");
        pending.insert(path.to_string(), id);
    }
    Ok(())
}

/// After the watcher channel closes, keep the worker alive long
/// enough to receive in-flight acks. Returns `true` if there's still
/// work to do (so the outer loop should continue), `false` if the
/// queue is empty and no acks are pending.
async fn drain_remaining(
    w: &mut PushWorker,
    pending: &mut HashMap<String, u64>,
) -> bool {
    // Cheap check: if we have nothing pending and the queue has
    // nothing for us, stop. peek_oldest now scopes to (account,
    // workspace) at the SQL layer, so any returned row is for us.
    let len_for_us = match w.queue.peek_oldest(&w.account, &w.workspace, 1).await {
        Ok(rows) => !rows.is_empty(),
        Err(_) => false,
    };
    !pending.is_empty() || len_for_us
}

#[cfg(test)]
mod tests {
    //! End-to-end tests for handle_watch_event + run_push_worker
    //! over a real on-disk workspace. Pin the surface so the next
    //! "fix" doesn't silently stop pushing files.

    use super::*;
    use crate::tunnel::ClientMsg;
    use std::sync::Arc;
    use tempfile::TempDir;
    use tokio::sync::mpsc;
    use tokio::time::{timeout, Duration};

    struct Harness {
        // tempdirs stay alive for the duration of the test
        _wsdir: TempDir,
        _qdir: TempDir,
        watch_tx: mpsc::Sender<WatchEvent>,
        ack_tx: mpsc::Sender<AckMsg>,
        shutdown_tx: mpsc::Sender<()>,
        rx: mpsc::Receiver<OutFrame>,
        ws_slot: Arc<std::sync::RwLock<Option<mpsc::Sender<OutFrame>>>>,
        ws_dir: std::path::PathBuf,
        worker_handle: tokio::task::JoinHandle<()>,
    }

    async fn build_harness() -> Harness {
        let wsdir = tempfile::tempdir().unwrap();
        let qdir = tempfile::tempdir().unwrap();
        let queue = Arc::new(
            PushQueue::open(&qdir.path().join("q.db"))
                .await
                .expect("open queue"),
        );
        // workspace_root/account/workspace must exist before events
        // come in — handle_watch_event uses it to stat the path.
        let ws_dir = wsdir.path().join("alice").join("w1");
        std::fs::create_dir_all(&ws_dir).unwrap();
        let ignore = Arc::new(IgnoreFilter::new(&ws_dir).expect("ignore filter"));

        let (watch_tx, watch_rx) = mpsc::channel(32);
        let (ack_tx, ack_rx) = mpsc::channel(32);
        let (shutdown_tx, shutdown_rx) = mpsc::channel(1);
        let (tx, rx) = mpsc::channel(64);
        let ws_slot = Arc::new(std::sync::RwLock::new(Some(tx)));
        let worker = PushWorker {
            session_id: Uuid::new_v4(),
            account: "alice".into(),
            workspace: "w1".into(),
            workspace_root: wsdir.path().to_path_buf(),
            ignore_filter: ignore,
            queue,
            watch_rx,
            ack_rx,
            shutdown_rx,
            ws_tx_slot: ws_slot.clone(),
        };
        let worker_handle = tokio::spawn(async move { run_push_worker(worker).await });
        Harness {
            _wsdir: wsdir,
            _qdir: qdir,
            watch_tx,
            ack_tx,
            shutdown_tx,
            rx,
            ws_slot,
            ws_dir,
            worker_handle,
        }
    }

    async fn next_frame(h: &mut Harness) -> ClientMsg {
        let out = timeout(Duration::from_secs(2), h.rx.recv())
            .await
            .expect("frame within 2s")
            .expect("channel open");
        match out {
            OutFrame::Text(m) => m,
            OutFrame::Binary(_) => panic!("expected text frame, got binary"),
        }
    }

    async fn shutdown(h: Harness) {
        let _ = h.shutdown_tx.send(()).await;
        let _ = h.worker_handle.await;
    }

    /// The bread-and-butter case: a regular file appears on disk, the
    /// watcher fires Changed, the worker reads + ships it. Regression
    /// for "files completely stopped pushing after the dir-skip fix".
    #[tokio::test]
    async fn regular_file_change_enqueues_and_ships_push_file() {
        let mut h = build_harness().await;
        std::fs::write(h.ws_dir.join("hello.txt"), b"howdy").unwrap();

        h.watch_tx
            .send(WatchEvent::Changed {
                path: "hello.txt".into(),
            })
            .await
            .unwrap();

        let frame = next_frame(&mut h).await;
        match frame {
            ClientMsg::WorkspacePushFile { path, content, .. } => {
                assert_eq!(path, "hello.txt");
                assert_eq!(content, b"howdy");
            }
            other => panic!("expected WorkspacePushFile, got {:?}", other),
        }

        // Ack so the queue clears.
        h.ack_tx
            .send(AckMsg {
                path: "hello.txt".into(),
                ok: true,
                error: None,
            })
            .await
            .unwrap();
        shutdown(h).await;
    }

    /// Directory Changed events fan out into PushFile events for
    /// every regular file inside. This is the macOS FSEvents
    /// coalescing case — `git clone`, `cargo build`, `tar -xf` etc
    /// often arrive as a single parent-directory event, and if we
    /// only skipped them we'd silently lose the entire subtree.
    #[tokio::test]
    async fn directory_change_fans_out_to_subtree_files() {
        let mut h = build_harness().await;
        // Lay out a small subtree on disk: subdir/a.txt,
        // subdir/inner/b.txt, subdir/inner/empty/ (empty dirs skipped).
        std::fs::create_dir_all(h.ws_dir.join("subdir/inner/empty")).unwrap();
        std::fs::write(h.ws_dir.join("subdir/a.txt"), b"AAA").unwrap();
        std::fs::write(h.ws_dir.join("subdir/inner/b.txt"), b"BBB").unwrap();

        // ONE Changed event for the directory — the watcher would
        // emit this when FSEvents coalesces the bulk creation.
        h.watch_tx
            .send(WatchEvent::Changed { path: "subdir".into() })
            .await
            .unwrap();

        // Both files must reach the wire (order isn't fixed by
        // stack-based walk — collect into a set).
        let mut seen: std::collections::HashMap<String, Vec<u8>> = Default::default();
        for _ in 0..2 {
            let frame = next_frame(&mut h).await;
            match frame {
                ClientMsg::WorkspacePushFile { path, content, .. } => {
                    seen.insert(path, content);
                }
                _ => panic!("expected WorkspacePushFile only during subtree fan-out"),
            }
        }
        assert_eq!(seen.get("subdir/a.txt"), Some(&b"AAA".to_vec()));
        assert_eq!(seen.get("subdir/inner/b.txt"), Some(&b"BBB".to_vec()));
        // Empty dirs don't produce frames.
        let extra = timeout(Duration::from_millis(200), h.rx.recv()).await;
        assert!(extra.is_err(), "empty dirs shouldn't generate frames");

        for path in seen.keys() {
            h.ack_tx
                .send(AckMsg {
                    path: path.clone(),
                    ok: true,
                    error: None,
                })
                .await
                .unwrap();
        }
        shutdown(h).await;
    }

    /// Push worker must survive a WS reconnect: when the slot is
    /// cleared (writer task gone), enqueues continue, the row stays
    /// in the queue, and once a fresh tx is bound the next scan
    /// ships the backlog. Without this, a single transient WS reset
    /// would permanently sink anything the user was about to push.
    #[tokio::test]
    async fn push_survives_ws_reconnect() {
        let mut h = build_harness().await;

        // Disconnect: clear the slot before the first event arrives.
        // Note: clearing the slot drops the originally-bound Sender,
        // which closes the channel that `h.rx` is the receiver for —
        // recv() on a closed channel returns `Ok(None)` immediately,
        // not a timeout, so guard on "no actual frame body" rather
        // than on "timeout".
        *h.ws_slot.write().unwrap() = None;

        std::fs::write(h.ws_dir.join("a.txt"), b"AAA").unwrap();
        h.watch_tx
            .send(WatchEvent::Changed { path: "a.txt".into() })
            .await
            .unwrap();
        let stray = timeout(Duration::from_millis(150), h.rx.recv()).await;
        assert!(
            !matches!(stray, Ok(Some(_))),
            "no frame should ship while ws slot is None"
        );

        // Reconnect with a brand-new channel (mimics ws.rs::run_once
        // binding the writer's fresh tx after a reset).
        let (new_tx, mut new_rx) = mpsc::channel(64);
        *h.ws_slot.write().unwrap() = Some(new_tx);

        // The next scan tick / watch event must drain the queued row
        // onto the NEW channel.
        let frame = timeout(Duration::from_secs(2), new_rx.recv())
            .await
            .expect("frame within 2s after reconnect")
            .expect("channel open");
        match frame {
            OutFrame::Text(ClientMsg::WorkspacePushFile { path, content, .. }) => {
                assert_eq!(path, "a.txt");
                assert_eq!(content, b"AAA");
            }
            _ => panic!("expected WorkspacePushFile after reconnect"),
        }
        h.ack_tx
            .send(AckMsg {
                path: "a.txt".into(),
                ok: true,
                error: None,
            })
            .await
            .unwrap();
        shutdown(h).await;
    }

    /// If the file is already gone by the time the worker reads it
    /// (race between Changed event and the file being unlinked
    /// immediately after), treat it as a delete. The previous
    /// behaviour for this case must survive the dir-skip refactor.
    #[tokio::test]
    async fn vanished_file_change_falls_through_to_delete() {
        let mut h = build_harness().await;
        // Note: never created on disk.
        h.watch_tx
            .send(WatchEvent::Changed {
                path: "ghost.txt".into(),
            })
            .await
            .unwrap();
        let frame = next_frame(&mut h).await;
        match frame {
            ClientMsg::WorkspaceDeleteFile { path, .. } => {
                assert_eq!(path, "ghost.txt");
            }
            other => panic!("expected WorkspaceDeleteFile, got {:?}", other),
        }
        h.ack_tx
            .send(AckMsg {
                path: "ghost.txt".into(),
                ok: true,
                error: None,
            })
            .await
            .unwrap();
        shutdown(h).await;
    }
}
