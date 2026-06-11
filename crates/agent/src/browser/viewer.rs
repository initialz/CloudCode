//! Agent-side viewer manager (Desktop App P2 Task 2; multi-target in P6).
//!
//! Bridges the hub's `ServerMsg::Viewer*` control frames to the CDP
//! [`ScreencastSession`] machinery: one live screencast per `viewer_session_id`.
//!
//!   * [`ViewerManager::attach`] starts a [`TargetWatcher`] (browser-level CDP
//!     target discovery) plus an initial screencast against the resident
//!     Chrome, and spawns a pump that wraps each JPEG frame in a
//!     `TAG_SCREENCAST_FRAME` binary frame (keyed by `viewer_session_id`) and
//!     sends it to the hub over the per-connection `OutFrame` channel. Every
//!     target-list change goes up as `ClientMsg::ViewerTargets`.
//!   * [`ViewerManager::select_target`] switches the screencast to another tab
//!     (hub relays the app's tab click as `ServerMsg::ViewerSelectTarget`).
//!   * [`ViewerManager::input`] forwards a viewer's input event into the
//!     matching screencast (CDP `Input.*`).
//!   * [`ViewerManager::detach`] stops + removes watcher and screencast.
//!
//! The manager is **per hub connection**: it's created in `ws::run_once` with
//! that connection's `OutFrame` sender and dropped when the connection ends,
//! which stops every in-flight screencast and watcher (no cross-reconnect
//! leaks — see the teardown-cascade note on [`ViewerEntry`]).
//!
//! P4 Task 1 wires `session_id` into page selection: `attach` resolves a
//! per-session page hint from the [`EndpointState`] (the mcp endpoint that owns
//! each session's playwright-mcp) and screencasts THAT session's page. When no
//! hint is known — the agent's current default playwright-mcp config shares one
//! browser context across sessions, so there is no distinct per-session target —
//! it falls back to the active page (documented residual risk; see
//! `docs/superpowers/plans/2026-06-10-p4-page-mapping-notes.md`).

use crate::browser::chrome::ChromeManager;
use crate::browser::mcp_endpoint::EndpointState;
use crate::browser::screencast::{
    page_ws_url, pick_page_entry_for_session, ScreencastSession, TargetWatcher,
};
use crate::pty::OutFrame;
use crate::tunnel::{pack_pty_frame, ClientMsg, TargetInfo, ViewerInputEvent, TAG_SCREENCAST_FRAME};
use dashmap::DashMap;
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use uuid::Uuid;

/// How many JPEG frames we let queue between the screencast read loop and the
/// hub send pump before dropping. Frames are large and latency-sensitive, so a
/// shallow buffer keeps us close to live instead of building backlog.
const FRAME_QUEUE: usize = 8;

/// Buffer for the watcher's target-list change channel. Changes are tiny and
/// rare (tab open/close/navigate), so a small buffer is plenty.
const TARGETS_QUEUE: usize = 16;

/// The mutable streaming state of one viewer: which CDP target is currently
/// being screencast, and the live session doing it. `None` = idle (no page
/// targets yet; the forwarder auto-selects the first one to appear).
type StreamSlot = Option<(String /* target id */, ScreencastSession)>;

/// Shared per-viewer state, used by `attach`/`select_target`/`input` and the
/// watcher-forwarder task.
struct ViewerInner {
    viewer_session_id: Uuid,
    cdp_http_url: String,
    /// Master clone of the per-viewer frame channel sender. Each
    /// `ScreencastSession` gets a clone; holding one here keeps the channel
    /// (and thus the frame pump) alive across target switches and idle
    /// periods, instead of the pump misreading a tab switch as "stream ended".
    frame_tx: mpsc::Sender<Vec<u8>>,
    /// Current screencast. A tokio `Mutex` because switching holds the lock
    /// across `stop().await`; that also serializes concurrent switches
    /// (select vs auto-select). `input` uses `try_lock` and drops the event
    /// during a switch (momentary; input lag beats blocking the read loop).
    stream: tokio::sync::Mutex<StreamSlot>,
    /// The per-connection sender to the ws writer task (agent → hub).
    tx: mpsc::Sender<OutFrame>,
}

impl ViewerInner {
    /// Switch the screencast to `target_id`: resolve its page ws url, start a
    /// new session, then stop + replace the old one. Returns `false` (leaving
    /// the current stream untouched) when the target is gone (stale tab click)
    /// or the connect fails.
    async fn switch_to(&self, target_id: String) -> bool {
        let Some(ws_url) = page_ws_url(&self.cdp_http_url, &target_id).await else {
            tracing::warn!(
                viewer = %self.viewer_session_id,
                target = %target_id,
                "select_target: no such page target (stale id?)"
            );
            return false;
        };
        let new = match ScreencastSession::start_on_target(&ws_url, self.frame_tx.clone()).await {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(
                    viewer = %self.viewer_session_id,
                    target = %target_id,
                    error = %e,
                    "select_target: screencast start failed"
                );
                return false;
            }
        };
        let mut slot = self.stream.lock().await;
        if let Some((_, old)) = slot.take() {
            old.stop().await;
        }
        *slot = Some((target_id, new));
        true
    }

    /// Stop the current screencast (if any) and go idle.
    async fn stop_current(&self) {
        if let Some((_, old)) = self.stream.lock().await.take() {
            old.stop().await;
        }
    }

    /// The target id currently being screencast, if any.
    async fn current_target(&self) -> Option<String> {
        self.stream.lock().await.as_ref().map(|(id, _)| id.clone())
    }
}

/// One attached viewer: its shared state, its target watcher, and the task
/// forwarding watcher changes to the hub.
///
/// Teardown cascade (covers both explicit `detach` and "manager dropped on
/// reconnect"): dropping the entry drops the [`TargetWatcher`], whose `Drop`
/// aborts its ws task → the change channel closes → the forwarder's `recv`
/// yields `None` and it exits → its `Arc<ViewerInner>` drops → the
/// `ScreencastSession` inside (if any) drops, and ITS `Drop` aborts the CDP
/// ws task. `shutdown` additionally aborts the forwarder and stops the
/// screencast promptly (flushing `Page.stopScreencast`).
struct ViewerEntry {
    inner: Arc<ViewerInner>,
    watcher: Arc<TargetWatcher>,
    forwarder: JoinHandle<()>,
}

impl ViewerEntry {
    async fn shutdown(self) {
        self.forwarder.abort();
        self.inner.stop_current().await;
        // `self.watcher` drops here, aborting the watcher's ws task.
    }
}

/// One hub connection's set of live screencasts, keyed by `viewer_session_id`.
pub struct ViewerManager {
    chrome: Arc<ChromeManager>,
    /// The mcp endpoint state, queried for each session's screencast page hint
    /// (P4 Task 1). Shares Arc internals with the endpoint claude POSTs to, so a
    /// hint recorded against a session is visible here.
    mcp: EndpointState,
    /// The per-connection sender to the ws writer task (agent → hub).
    tx: mpsc::Sender<OutFrame>,
    sessions: DashMap<Uuid, ViewerEntry>,
}

impl ViewerManager {
    pub fn new(chrome: Arc<ChromeManager>, mcp: EndpointState, tx: mpsc::Sender<OutFrame>) -> Self {
        Self {
            chrome,
            mcp,
            tx,
            sessions: DashMap::new(),
        }
    }

    /// Attach a viewer: start a target watcher + (when a page exists) an
    /// initial screencast, and pump frames/targets to the hub.
    ///
    /// `session_id` identifies the PTY/browser session the viewer asked to
    /// watch. We resolve that session's page target hint from the mcp endpoint
    /// and screencast THAT page; if no hint is known (default config — sessions
    /// share one browser context, so there is no distinct per-session target),
    /// we fall back to the active page.
    ///
    /// Unlike pre-P6, "no page target right now" is NOT an error: the viewer
    /// stays attached and idle (the hub gets an empty `ViewerTargets`), and the
    /// forwarder auto-starts on the first page target that appears. Only a
    /// browser we can't even watch (watcher start failure) reports
    /// `ClientMsg::ViewerClosed`.
    pub async fn attach(&self, viewer_session_id: Uuid, session_id: Uuid) {
        // Replacing an existing attach for the same viewer: stop the old one.
        if let Some((_, old)) = self.sessions.remove(&viewer_session_id) {
            old.shutdown().await;
        }

        let cdp_http_url = self.chrome.cdp_http_url();

        // Target watcher first — one per viewer. (A shared watcher with
        // per-viewer subscriptions would save browser-level CDP connections,
        // but viewer count is single digits, so per-viewer is the simpler
        // correct choice; revisit if viewers ever multiply.)
        let (chg_tx, mut chg_rx) = mpsc::channel::<Vec<TargetInfo>>(TARGETS_QUEUE);
        let watcher = match TargetWatcher::start(&cdp_http_url, chg_tx).await {
            Ok(w) => Arc::new(w),
            Err(e) => {
                tracing::warn!(
                    viewer = %viewer_session_id,
                    session = %session_id,
                    error = %e,
                    "target watcher start failed"
                );
                let _ = self
                    .tx
                    .send(OutFrame::Text(ClientMsg::ViewerClosed {
                        viewer_session_id,
                        reason: Some(format!("target watcher start failed: {e}")),
                    }))
                    .await;
                return;
            }
        };

        let (frame_tx, mut frame_rx) = mpsc::channel::<Vec<u8>>(FRAME_QUEUE);
        let inner = Arc::new(ViewerInner {
            viewer_session_id,
            cdp_http_url: cdp_http_url.clone(),
            frame_tx: frame_tx.clone(),
            stream: tokio::sync::Mutex::new(None),
            tx: self.tx.clone(),
        });

        // Initial screencast: per-session page hint (P4), else active page.
        // Tracked by target id so tab switching / destroyed-target handling
        // knows what's on screen. Failure leaves the viewer idle.
        let hint = self.mcp.page_hint_for(session_id);
        match fetch_and_pick(&cdp_http_url, hint.as_deref()).await {
            Some((target_id, ws_url)) => {
                match ScreencastSession::start_on_target(&ws_url, frame_tx.clone()).await {
                    Ok(s) => {
                        *inner.stream.lock().await = Some((target_id, s));
                    }
                    Err(e) => {
                        tracing::warn!(
                            viewer = %viewer_session_id,
                            session = %session_id,
                            error = %e,
                            "initial screencast start failed; viewer idle"
                        );
                    }
                }
            }
            None => {
                tracing::info!(
                    viewer = %viewer_session_id,
                    session = %session_id,
                    "no page target yet; viewer idle until one appears"
                );
            }
        }

        // Frame pump: wrap each JPEG as a TAG_SCREENCAST_FRAME (keyed by
        // viewer_session_id) and ship it to the hub. Per-VIEWER, not
        // per-screencast: `inner.frame_tx` keeps the channel open across tab
        // switches and idle periods, so the pump only ends when the entry is
        // dropped (detach / reconnect) — a closed page is an auto-switch (or
        // an empty ViewerTargets), not a `ViewerClosed`.
        let pump_tx = self.tx.clone();
        tokio::spawn(async move {
            while let Some(jpeg) = frame_rx.recv().await {
                let frame = pack_pty_frame(TAG_SCREENCAST_FRAME, viewer_session_id, &jpeg);
                if pump_tx.send(OutFrame::Binary(frame)).await.is_err() {
                    // Hub connection gone; nothing more to do.
                    return;
                }
            }
        });

        // Forwarder: every watcher change (including the watcher's initial
        // list, possibly empty) goes to the hub as ViewerTargets, then drives
        // the auto-select rules:
        //   * current target destroyed → stop, start the first remaining page
        //     (or go idle if none);
        //   * idle and a page target appears → start streaming it.
        // The app mirrors this auto-select logic UI-side.
        let fwd_inner = Arc::clone(&inner);
        let forwarder = tokio::spawn(async move {
            while let Some(targets) = chg_rx.recv().await {
                if fwd_inner
                    .tx
                    .send(OutFrame::Text(ClientMsg::ViewerTargets {
                        viewer_session_id,
                        targets: targets.clone(),
                    }))
                    .await
                    .is_err()
                {
                    return; // Hub connection gone.
                }
                match fwd_inner.current_target().await {
                    Some(cur) if !targets.iter().any(|t| t.id == cur) => {
                        // Our tab vanished: drop the dead stream, then pick up
                        // the first surviving page (or stay idle).
                        fwd_inner.stop_current().await;
                        if let Some(first) = targets.first() {
                            fwd_inner.switch_to(first.id.clone()).await;
                        }
                    }
                    None => {
                        if let Some(first) = targets.first() {
                            fwd_inner.switch_to(first.id.clone()).await;
                        }
                    }
                    Some(_) => {} // Current tab still alive: nothing to do.
                }
            }
        });

        self.sessions.insert(
            viewer_session_id,
            ViewerEntry {
                inner,
                watcher,
                forwarder,
            },
        );
    }

    /// Switch a viewer's screencast to another CDP target (tab click in the
    /// app). On a stale target id (tab closed between list and click) the
    /// current stream is kept and a fresh `ViewerTargets` snapshot is sent so
    /// the app can re-render its tab bar.
    pub async fn select_target(&self, viewer_session_id: Uuid, target_id: String) {
        // Clone the Arcs out and drop the map guard before any await.
        let Some((inner, watcher)) = self
            .sessions
            .get(&viewer_session_id)
            .map(|e| (Arc::clone(&e.inner), Arc::clone(&e.watcher)))
        else {
            tracing::debug!(viewer = %viewer_session_id, "select_target for unknown viewer; dropping");
            return;
        };
        if !inner.switch_to(target_id).await {
            // The list changed under the viewer — re-sync its tab bar.
            let _ = inner
                .tx
                .send(OutFrame::Text(ClientMsg::ViewerTargets {
                    viewer_session_id,
                    targets: watcher.snapshot(),
                }))
                .await;
        }
    }

    /// Inject a viewer input event into the matching screencast (no-op if the
    /// viewer has no live session, is idle, or is mid-switch). Synchronous —
    /// the CDP write is queued inside the screencast task.
    pub fn input(&self, viewer_session_id: Uuid, event: &ViewerInputEvent) {
        let Some(inner) = self
            .sessions
            .get(&viewer_session_id)
            .map(|e| Arc::clone(&e.inner))
        else {
            tracing::debug!(viewer = %viewer_session_id, "input for unknown viewer; dropping");
            return;
        };
        match inner.stream.try_lock() {
            Ok(slot) => match slot.as_ref() {
                Some((_, session)) => session.input(event),
                None => {
                    tracing::debug!(viewer = %viewer_session_id, "input while idle; dropping")
                }
            },
            Err(_) => {
                // A switch holds the lock; this event belongs to the old tab
                // anyway, so dropping it is the right call.
                tracing::debug!(viewer = %viewer_session_id, "input during target switch; dropping");
            }
        };
    }

    /// Stop and remove everything for `viewer_session_id`: the forwarder, the
    /// screencast (flushing `Page.stopScreencast`, per T1's leak note), and
    /// the target watcher.
    pub async fn detach(&self, viewer_session_id: Uuid) {
        if let Some((_, entry)) = self.sessions.remove(&viewer_session_id) {
            entry.shutdown().await;
        }
    }
}

/// `GET /json` + [`pick_page_entry_for_session`]: resolve the initial
/// `(target_id, ws_url)` for an attach. `None` on fetch failure or when no
/// suitable page exists (both leave the viewer idle).
async fn fetch_and_pick(cdp_http_url: &str, hint: Option<&str>) -> Option<(String, String)> {
    let list_url = format!("{cdp_http_url}/json");
    let body = match reqwest::get(&list_url).await {
        Ok(r) => r.text().await.ok()?,
        Err(e) => {
            tracing::warn!(error = %e, "GET {list_url} failed");
            return None;
        }
    };
    pick_page_entry_for_session(&body, hint)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::BrowserConfig;
    use crate::tunnel::unpack_pty_frame;
    use std::time::Duration;

    /// P2 end-to-end (T1+T2) integration: drive the real agent-internal viewer
    /// path against a real headless Chrome — `ViewerManager::attach` →
    /// `ScreencastSession` → a `TAG_SCREENCAST_FRAME`-tagged binary frame lands
    /// on the per-connection `OutFrame` channel keyed by the viewer_session_id —
    /// then inject a viewer input (mouse + IME InsertText) and detach. This is
    /// the agent half of the screencast pipe with NO hub/browser-UI: it proves
    /// the same wiring the hub's viewer ws relay drives in production.
    ///
    /// P6 additions ride along: the attach also emits `ViewerTargets` text
    /// frames (the drain loop skips Text frames, so the JPEG assertion is
    /// unchanged).
    ///
    /// Run manually:
    /// `cargo test -p cloudcode-agent viewer_attach_streams -- --ignored --nocapture`
    /// Prereqs: a real Chrome/Chromium install on PATH (no internet needed; the
    /// page is a self-contained `data:` URL).
    #[tokio::test]
    #[ignore = "requires a real Chrome install; run manually"]
    async fn viewer_attach_streams_tagged_jpeg_frame_and_accepts_input() {
        let cfg = BrowserConfig {
            enabled: true,
            chrome_path: None,
            cdp_port: 19245,
            mcp_port: 7111,
            mcp_command: None,
        };
        let tmp = tempfile::tempdir().unwrap();
        let chrome = Arc::new(ChromeManager::new(cfg.clone(), tmp.path()));
        chrome
            .start()
            .await
            .expect("Chrome should start and become ready");
        let cdp = chrome.cdp_http_url();

        // Open a non-blank page so the screencast has real pixels. Newer Chrome
        // wants PUT on /json/new; older accepts GET. Try PUT first, GET fallback.
        let data_url = "data:text/html,<h1 style=font-size:80px>P2-VIEWER</h1>";
        let new_url = format!("{cdp}/json/new?{data_url}");
        let http = reqwest::Client::new();
        let opened = match http.put(&new_url).send().await {
            Ok(r) if r.status().is_success() => true,
            _ => matches!(http.get(&new_url).send().await, Ok(r) if r.status().is_success()),
        };
        assert!(opened, "failed to open a data: page target via /json/new");
        // Give Chrome a moment to register + render the new target.
        tokio::time::sleep(Duration::from_millis(800)).await;

        // Per-connection OutFrame channel — exactly what ws::run_once hands the
        // ViewerManager. The hub's viewer ws relay drains this same channel.
        let (out_tx, mut out_rx) = mpsc::channel::<OutFrame>(32);
        let mcp = EndpointState::new(Arc::clone(&chrome), cfg.clone());
        let manager = ViewerManager::new(Arc::clone(&chrome), mcp, out_tx);

        let viewer_session_id = Uuid::new_v4();
        // No page hint recorded → active-page fallback (default config).
        let session_id = Uuid::new_v4();
        manager.attach(viewer_session_id, session_id).await;

        // Drain OutFrames until the first Binary screencast frame (skip any
        // interleaved Text — e.g. the P6 ViewerTargets list). Must arrive
        // within 5s. A ViewerClosed Text means the attach failed — surface it
        // loudly.
        let frame = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                match out_rx.recv().await {
                    Some(OutFrame::Binary(b)) => break b,
                    Some(OutFrame::Text(ClientMsg::ViewerClosed { reason, .. })) => {
                        panic!("screencast closed before any frame: {reason:?}");
                    }
                    Some(OutFrame::Text(_)) => continue,
                    None => panic!("OutFrame channel closed before any frame"),
                }
            }
        })
        .await
        .expect("a screencast frame should arrive within 5s");

        // Unpack the binary frame: TAG_SCREENCAST_FRAME, keyed by the
        // viewer_session_id, payload is a raw JPEG (magic bytes FF D8).
        let (tag, id, payload) =
            unpack_pty_frame(&frame).expect("frame must unpack as [tag][16B id][payload]");
        assert_eq!(
            tag, TAG_SCREENCAST_FRAME,
            "binary frame must carry the screencast tag 0x03"
        );
        assert_eq!(
            id, viewer_session_id,
            "frame must be keyed by the viewer_session_id"
        );
        assert!(payload.len() >= 2, "JPEG payload too short");
        assert_eq!(
            &payload[0..2],
            &[0xFF, 0xD8],
            "payload must start with JPEG magic bytes FF D8; got {:02X} {:02X}",
            payload[0],
            payload[1]
        );
        eprintln!(
            "got TAG_SCREENCAST_FRAME (0x{tag:02X}) viewer={id} JPEG {} bytes, magic {:02X} {:02X}",
            payload.len(),
            payload[0],
            payload[1]
        );

        // Inject viewer input down the same path the hub relay uses: a mouse
        // move and an IME InsertText (Chinese). Must not panic; must NOT trip a
        // ViewerClosed error on the channel.
        manager.input(viewer_session_id, &ViewerInputEvent::MouseMove { x: 20.0, y: 20.0 });
        manager.input(
            viewer_session_id,
            &ViewerInputEvent::InsertText { text: "你好".into() },
        );
        // Input to an unknown viewer is a harmless no-op (no panic).
        manager.input(Uuid::new_v4(), &ViewerInputEvent::MouseMove { x: 0.0, y: 0.0 });

        // Drain briefly: confirm the channel is still alive and hasn't surfaced
        // an error ViewerClosed off the input path.
        match tokio::time::timeout(Duration::from_millis(300), out_rx.recv()).await {
            Ok(Some(OutFrame::Text(ClientMsg::ViewerClosed { reason, .. }))) => {
                panic!("unexpected ViewerClosed after input: {reason:?}");
            }
            // More frames or a quiet channel are both fine.
            _ => {}
        }

        // Detach tears down the CDP ws + read task + target watcher (no leak).
        manager.detach(viewer_session_id).await;
        drop(chrome);
    }
}
