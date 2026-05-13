//! Tail claude's per-project conversation log.
//!
//! Claude writes one JSONL line per event to
//! `~/.claude/projects/<encoded-cwd>/<claude-session-uuid>.jsonl`.
//! We watch that directory with `notify`, track each file's byte
//! offset, and ship every new line up to the hub as a `Message`
//! frame so the admin UI can render the conversation later.
//!
//! Lifetime: tied to one cloudcode session. `spawn()` returns a
//! `WatcherHandle`; dropping it asks the task to stop. The watcher
//! also wakes itself every couple of seconds because filesystem
//! notifications get lost surprisingly often on macOS (especially
//! when claude writes from a sandboxed child).

use crate::pty::OutFrame;
use crate::tunnel::ClientMsg;
use notify::{Config, RecommendedWatcher, RecursiveMode, Watcher};
use std::collections::HashMap;
use std::io::SeekFrom;
use std::path::{Path, PathBuf};
use tokio::io::{AsyncReadExt, AsyncSeekExt};
use tokio::sync::mpsc;
use uuid::Uuid;

pub struct WatcherHandle {
    shutdown: mpsc::Sender<()>,
}

impl Drop for WatcherHandle {
    fn drop(&mut self) {
        // Best-effort wake-up; if the task already exited, this no-ops.
        let _ = self.shutdown.try_send(());
    }
}

/// Map a workspace cwd to claude's per-project directory under
/// `~/.claude/projects/`. Claude encodes the absolute path by
/// replacing every `/` with `-`, so `/Users/x/proja` becomes
/// `-Users-x-proja`. Our workspace paths are restricted to
/// `[A-Za-z0-9_-]` segments, so no surprises.
pub fn project_dir(home: &Path, cwd: &Path) -> PathBuf {
    let encoded: String = cwd
        .to_string_lossy()
        .chars()
        .map(|c| if c == '/' { '-' } else { c })
        .collect();
    home.join(".claude").join("projects").join(encoded)
}

/// Spawn the watcher. Returns a handle that stops the task on drop.
pub fn spawn(
    cc_session_id: Uuid,
    cwd: PathBuf,
    home: PathBuf,
    out_tx: mpsc::Sender<OutFrame>,
) -> WatcherHandle {
    let (shutdown_tx, mut shutdown_rx) = mpsc::channel::<()>(1);
    let dir = project_dir(&home, &cwd);

    tokio::spawn(async move {
        if let Err(e) = tokio::fs::create_dir_all(&dir).await {
            tracing::debug!(error = %e, dir = %dir.display(), "jsonl: create project dir");
        }

        // notify uses a sync callback. Forward into a tokio channel.
        let (evt_tx, mut evt_rx) = mpsc::channel::<()>(64);
        let mut watcher: RecommendedWatcher = match notify::recommended_watcher(
            move |_res: notify::Result<notify::Event>| {
                let _ = evt_tx.blocking_send(());
            },
        ) {
            Ok(w) => w,
            Err(e) => {
                tracing::warn!(session = %cc_session_id, error = %e, "jsonl: watcher init failed");
                return;
            }
        };
        if let Err(e) = watcher.configure(Config::default()) {
            tracing::debug!(error = %e, "jsonl: watcher configure");
        }
        if let Err(e) = watcher.watch(&dir, RecursiveMode::NonRecursive) {
            tracing::warn!(session = %cc_session_id, dir = %dir.display(), error = %e, "jsonl: watch failed");
            return;
        }

        tracing::info!(session = %cc_session_id, dir = %dir.display(), "jsonl: watcher started");

        let mut offsets: HashMap<PathBuf, u64> = HashMap::new();
        // Initial sweep so any pre-existing file content is shipped.
        scan_once(&dir, &mut offsets, cc_session_id, &out_tx).await;

        let mut tick = tokio::time::interval(std::time::Duration::from_secs(2));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

        loop {
            tokio::select! {
                _ = shutdown_rx.recv() => break,
                _ = evt_rx.recv() => {
                    scan_once(&dir, &mut offsets, cc_session_id, &out_tx).await;
                }
                _ = tick.tick() => {
                    scan_once(&dir, &mut offsets, cc_session_id, &out_tx).await;
                }
            }
        }

        drop(watcher);
        tracing::info!(session = %cc_session_id, "jsonl: watcher stopped");
    });

    WatcherHandle {
        shutdown: shutdown_tx,
    }
}

/// Scan the directory once. For every `.jsonl` file whose size has
/// grown since the last sweep, read the new bytes, parse each
/// newline-terminated line, and emit a Message frame. Partial trailing
/// lines (no final `\n`) are intentionally left for next sweep.
async fn scan_once(
    dir: &Path,
    offsets: &mut HashMap<PathBuf, u64>,
    cc_session_id: Uuid,
    out_tx: &mpsc::Sender<OutFrame>,
) {
    let mut entries = match tokio::fs::read_dir(dir).await {
        Ok(e) => e,
        Err(_) => return,
    };

    while let Ok(Some(entry)) = entries.next_entry().await {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
            continue;
        }
        let claude_session_id = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .to_string();
        if claude_session_id.is_empty() {
            continue;
        }

        let size = match tokio::fs::metadata(&path).await {
            Ok(m) => m.len(),
            Err(_) => continue,
        };
        let offset = offsets.get(&path).copied().unwrap_or(0);
        if size <= offset {
            continue;
        }

        let mut file = match tokio::fs::File::open(&path).await {
            Ok(f) => f,
            Err(_) => continue,
        };
        if file.seek(SeekFrom::Start(offset)).await.is_err() {
            continue;
        }
        let mut buf = String::new();
        if file.read_to_string(&mut buf).await.is_err() {
            continue;
        }

        let mut bytes_advanced: u64 = 0;
        for raw in buf.split_inclusive('\n') {
            if !raw.ends_with('\n') {
                // Partial line at tail — wait for the rest next sweep.
                break;
            }
            let line_bytes = raw.len() as u64;
            let trimmed = raw.trim_end_matches('\n').trim();
            bytes_advanced += line_bytes;
            if trimmed.is_empty() {
                continue;
            }
            let Ok(json) = serde_json::from_str::<serde_json::Value>(trimmed) else {
                continue;
            };
            let kind = json
                .get("type")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown")
                .to_string();
            let ts = json
                .get("timestamp")
                .and_then(|v| v.as_str())
                .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
                .map(|d| d.timestamp())
                .unwrap_or_else(|| chrono::Utc::now().timestamp());

            let frame = ClientMsg::Message {
                session_id: cc_session_id,
                claude_session_id: claude_session_id.clone(),
                ts,
                kind,
                body: trimmed.to_string(),
            };
            if out_tx.send(OutFrame::Text(frame)).await.is_err() {
                // Hub gone. Stop scanning; the task will exit on next loop.
                return;
            }
        }

        offsets.insert(path, offset + bytes_advanced);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn encodes_cwd_to_project_dir_name() {
        let home = Path::new("/Users/me");
        let cwd = Path::new("/Users/me/cloudcode-agent/workspaces/me/proja");
        let dir = project_dir(home, cwd);
        assert_eq!(
            dir,
            Path::new("/Users/me/.claude/projects/-Users-me-cloudcode-agent-workspaces-me-proja")
        );
    }
}
