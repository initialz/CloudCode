//! Filesystem watcher for the workspace sync engine.
//!
//! Wraps `notify::RecommendedWatcher` so callers consume a typed
//! tokio mpsc of [`WatchEvent`] instead of raw notify events. Two
//! transformations happen between notify and the caller:
//!
//! 1. **Ignore filtering** — every event path is normalised to a
//!    workspace-relative path and passed through [`IgnoreFilter`].
//!    Ignored paths are dropped before they hit the channel.
//! 2. **Time coalescing** — multiple notify events for the same path
//!    within a short window (`COALESCE_WINDOW`) collapse into one
//!    emitted event. This matters because editors typically write
//!    via rename-over (Vim writes a temp file then renames it,
//!    triggering several notify events for a single logical save).
//!
//! ## Non-obvious decisions
//!
//! - **Directory renames**: notify reports them as a single event
//!   on the directory itself, *not* per child. Once a worker drains
//!   the queue and discovers the directory has been renamed, it
//!   will rescan. For now the watcher emits `Removed` for the old
//!   path and `Changed` for the new path; the worker (added later)
//!   is responsible for fanning out per-child operations.
//! - **`is_dir` at emit time**: by the time we get the event, the
//!   file may already be gone (`stat` fails). We default to `false`
//!   in that case; `IgnoreFilter::matched_path_or_any_parents`
//!   still catches paths inside ignored directories.
//! - **Drop = stop**: dropping `WorkspaceWatcher` drops the inner
//!   `RecommendedWatcher` and signals the bridge task to exit.

use crate::sync::ignore_filter::IgnoreFilter;
use anyhow::{Context, Result};
use notify::{EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;

/// Time window during which repeated events for the same path
/// collapse into one emitted event.
pub const COALESCE_WINDOW: Duration = Duration::from_millis(100);

#[derive(Debug, Clone)]
pub enum WatchEvent {
    /// File was created, modified, or appeared via rename-to.
    Changed { path: PathBuf },
    /// File was deleted, or disappeared via rename-from.
    Removed { path: PathBuf },
}

impl WatchEvent {
    /// Convenience accessor — kept for debug logging in callers that
    /// don't pattern-match on the variant; both call sites currently
    /// destructure, so this is dead in release but useful in tests.
    #[allow(dead_code)]
    fn path(&self) -> &Path {
        match self {
            WatchEvent::Changed { path } | WatchEvent::Removed { path } => path,
        }
    }
}

/// Owns the notify watcher and the bridge task. Drop to stop.
pub struct WorkspaceWatcher {
    _watcher: RecommendedWatcher,
    shutdown: mpsc::Sender<()>,
}

impl Drop for WorkspaceWatcher {
    fn drop(&mut self) {
        let _ = self.shutdown.try_send(());
    }
}

/// Raw kind extracted from a notify event so the bridge task can
/// decide whether to emit `Changed` or `Removed`.
#[derive(Debug, Clone, Copy)]
enum RawKind {
    Changed,
    Removed,
}

impl WorkspaceWatcher {
    /// Start watching `root` recursively. Events that pass `filter`
    /// (and the coalescing window) are sent on `event_tx` as
    /// workspace-relative paths.
    pub fn start(
        root: PathBuf,
        filter: IgnoreFilter,
        event_tx: mpsc::Sender<WatchEvent>,
    ) -> Result<Self> {
        // Canonicalize the watch root before handing it to notify. On
        // macOS `/var` is a symlink to `/private/var`, and `tmp` /
        // `etc` follow the same pattern; FSEvents emits paths in
        // canonical form (`/private/var/...`) while the caller may
        // pass us the symlink-side form. The bridge task's
        // `strip_prefix(root)` only works when both sides agree, so
        // canonicalize once here and use that throughout.
        let root = std::fs::canonicalize(&root)
            .with_context(|| format!("canonicalize workspace dir {}", root.display()))?;
        // Bridge: notify -> sync mpsc -> tokio task -> filtered mpsc.
        let (raw_tx, raw_rx) = std::sync::mpsc::channel::<(PathBuf, RawKind, bool)>();

        let mut watcher: RecommendedWatcher =
            notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
                let evt = match res {
                    Ok(e) => e,
                    Err(e) => {
                        tracing::debug!(error = %e, "workspace watcher: notify error");
                        return;
                    }
                };
                let (kind, is_dir_hint) = classify(&evt.kind);
                let Some(kind) = kind else {
                    return;
                };
                for p in evt.paths {
                    let _ = raw_tx.send((p, kind, is_dir_hint));
                }
            })
            .context("create workspace watcher")?;

        watcher
            .watch(&root, RecursiveMode::Recursive)
            .with_context(|| format!("watch workspace dir {}", root.display()))?;

        let (shutdown_tx, mut shutdown_rx) = mpsc::channel::<()>(1);
        let filter = Arc::new(filter);
        let root = Arc::new(root);

        // Bridge task: pulls from the sync channel, applies filter +
        // coalescing window, forwards to the tokio mpsc.
        tokio::spawn(async move {
            // Per-path scheduling. Each entry holds the time at which
            // we'll next consider emitting an event for this path.
            // Newer events within the window replace the prior kind
            // but don't push the deadline out — that way an editor
            // hammering the same file still drains within ~window.
            let mut pending: HashMap<PathBuf, (RawKind, Instant)> = HashMap::new();
            // We poll the sync channel non-blockingly inside a tokio
            // interval. A dedicated blocking thread + a oneshot would
            // work too, but the polling approach avoids extra threads
            // and matches the cadence of the coalescing window.
            let mut tick =
                tokio::time::interval(Duration::from_millis(25));
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

            loop {
                tokio::select! {
                    _ = shutdown_rx.recv() => break,
                    _ = tick.tick() => {
                        drain_raw(&raw_rx, &root, &filter, &mut pending);
                        flush_due(&mut pending, &event_tx).await;
                    }
                }
            }
        });

        Ok(Self {
            _watcher: watcher,
            shutdown: shutdown_tx,
        })
    }
}

/// Map a notify `EventKind` into our binary classification. Returns
/// `None` for events we don't care about (access, metadata-only).
/// The `bool` is a hint about whether the event is for a directory;
/// notify rarely populates this reliably, so callers shouldn't trust
/// it for correctness — only as a fast-path for the ignore filter.
fn classify(kind: &EventKind) -> (Option<RawKind>, bool) {
    use notify::event::*;
    match kind {
        EventKind::Create(CreateKind::Folder) => (Some(RawKind::Changed), true),
        EventKind::Create(_) => (Some(RawKind::Changed), false),
        EventKind::Modify(ModifyKind::Data(_)) => (Some(RawKind::Changed), false),
        EventKind::Modify(ModifyKind::Name(rename_mode)) => match rename_mode {
            // To/From map cleanly to Changed/Removed. `Both` and
            // `Any` (single-path rename) we treat as Changed and let
            // the next sweep / explicit Remove event clean up the
            // old path.
            RenameMode::To => (Some(RawKind::Changed), false),
            RenameMode::From => (Some(RawKind::Removed), false),
            _ => (Some(RawKind::Changed), false),
        },
        EventKind::Remove(RemoveKind::Folder) => (Some(RawKind::Removed), true),
        EventKind::Remove(_) => (Some(RawKind::Removed), false),
        _ => (None, false),
    }
}

/// Drain whatever notify has produced since the last tick into the
/// pending map, applying the ignore filter on the way in.
fn drain_raw(
    raw_rx: &std::sync::mpsc::Receiver<(PathBuf, RawKind, bool)>,
    root: &Path,
    filter: &IgnoreFilter,
    pending: &mut HashMap<PathBuf, (RawKind, Instant)>,
) {
    while let Ok((abs_path, kind, is_dir_hint)) = raw_rx.try_recv() {
        let rel = match abs_path.strip_prefix(root) {
            Ok(p) => p.to_path_buf(),
            Err(_) => {
                // Outside the workspace root. notify sometimes emits
                // events for the root itself; drop those.
                continue;
            }
        };
        if rel.as_os_str().is_empty() {
            continue;
        }
        // Best-effort directory check: trust the hint, otherwise
        // stat'ing here would race the filesystem.
        if filter.is_ignored(&rel, is_dir_hint) {
            continue;
        }
        // Coalesce: newer kind replaces older, but the deadline is
        // set when the path first enters the window so we always
        // drain within ~COALESCE_WINDOW of the first event.
        let now = Instant::now();
        pending
            .entry(rel)
            .and_modify(|slot| slot.0 = kind)
            .or_insert((kind, now + COALESCE_WINDOW));
    }
}

/// Emit events whose coalescing deadline has passed.
async fn flush_due(
    pending: &mut HashMap<PathBuf, (RawKind, Instant)>,
    event_tx: &mpsc::Sender<WatchEvent>,
) {
    let now = Instant::now();
    // Collect ready keys first to satisfy the borrow checker.
    let ready: Vec<PathBuf> = pending
        .iter()
        .filter(|(_, (_, deadline))| *deadline <= now)
        .map(|(p, _)| p.clone())
        .collect();
    for p in ready {
        if let Some((kind, _)) = pending.remove(&p) {
            let evt = match kind {
                RawKind::Changed => WatchEvent::Changed { path: p },
                RawKind::Removed => WatchEvent::Removed { path: p },
            };
            // Receiver gone == watcher being torn down; silently stop.
            if event_tx.send(evt).await.is_err() {
                pending.clear();
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    //! Integration-style watcher tests require a real filesystem and
    //! are timing-sensitive; we skip them by default. Run manually
    //! with `cargo test -p cloudcode-agent --features '' -- --ignored watcher`.
    use super::*;

    #[tokio::test]
    #[ignore]
    async fn smoke_creates_file_emits_changed() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let filter = IgnoreFilter::new(&root).unwrap();
        let (tx, mut rx) = mpsc::channel(16);
        let _w = WorkspaceWatcher::start(root.clone(), filter, tx).unwrap();

        // Give the watcher a moment to subscribe.
        tokio::time::sleep(Duration::from_millis(100)).await;
        std::fs::write(root.join("hello.txt"), b"hi").unwrap();

        let evt = tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .expect("watcher event")
            .expect("channel open");
        assert!(matches!(evt, WatchEvent::Changed { .. }));
        assert_eq!(evt.path(), Path::new("hello.txt"));
    }
}
