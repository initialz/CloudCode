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
    page_ws_url, pick_page_entry_for_session, ScreencastSession, TargetWatcher, Viewport,
};
use crate::pty::OutFrame;
use crate::tunnel::{pack_pty_frame, ClientMsg, TargetInfo, ViewerInputEvent, TAG_SCREENCAST_FRAME};
use dashmap::DashMap;
use std::sync::atomic::{AtomicBool, Ordering};
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
    /// The viewer's desired browser viewport (CSS px), set by `ViewerResize`.
    /// Stored at the viewer level (not the screencast) so a tab switch
    /// re-applies the current size to the freshly-started screencast instead
    /// of snapping back to the default. `std::sync::Mutex` (no await held).
    viewport: std::sync::Mutex<Viewport>,
    /// The per-connection sender to the ws writer task (agent → hub).
    tx: mpsc::Sender<OutFrame>,
    /// Set by a deliberate teardown ([`ViewerEntry::shutdown`] — detach or
    /// attach-replace) BEFORE the forwarder is aborted. The forwarder's exit
    /// path reads it to tell "watcher died under us" (browser-level CDP ws
    /// lost → emit `ViewerClosed`) apart from "we are being torn down on
    /// purpose" (hub route already going away → stay quiet). An `AtomicBool`
    /// on the shared inner is the simplest race-safe carrier: the flag is
    /// stored before `abort()`, so a forwarder that still gets to run its
    /// exit path observes it. (The abort itself usually cancels the forwarder
    /// before it reaches the check; the flag covers the window where the
    /// channel-closed exit raced the abort.)
    detached: AtomicBool,
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
        // Re-apply the viewer's current viewport so a tab switch keeps the
        // panel-sized page instead of reverting to the default.
        let vp = self.current_viewport();
        let new = match ScreencastSession::start_on_target_sized(&ws_url, self.frame_tx.clone(), vp)
            .await
        {
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

    /// The viewer's current desired viewport.
    fn current_viewport(&self) -> Viewport {
        *self.viewport.lock().expect("viewport lock")
    }

    /// Apply a viewer resize: clamp, remember it (so a future tab switch
    /// re-applies it), and forward to the live screencast (no-op if idle —
    /// the next `start_on_target_sized` picks up the remembered viewport).
    async fn resize(&self, width: u32, height: u32) {
        let vp = Viewport::clamped(width, height);
        *self.viewport.lock().expect("viewport lock") = vp;
        if let Some((_, session)) = self.stream.lock().await.as_ref() {
            session.resize(vp.width, vp.height);
        }
    }
}

/// One attached viewer: its shared state, its target watcher, and the task
/// forwarding watcher changes to the hub.
///
/// Teardown cascade (covers both explicit `detach` and "manager dropped on
/// reconnect"): dropping the entry drops the [`TargetWatcher`], whose `Drop`
/// aborts its ws task → the change channel closes → the forwarder's `recv`
/// yields `None` and it exits (its `ViewerClosed` report on that path fails
/// harmlessly on a dropped connection — the `OutFrame` channel is gone too)
/// → its `Arc<ViewerInner>` drops → the
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
        // Mark the teardown deliberate BEFORE aborting: if the forwarder is
        // mid-exit (watcher channel closed in the same instant), it must NOT
        // report `ViewerClosed` for a detach the hub itself initiated.
        self.inner.detached.store(true, Ordering::SeqCst);
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
        let (chg_tx, chg_rx) = mpsc::channel::<Vec<TargetInfo>>(TARGETS_QUEUE);
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
            viewport: std::sync::Mutex::new(Viewport::default()),
            tx: self.tx.clone(),
            detached: AtomicBool::new(false),
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

        // Forwarder: see [`forward_watcher_changes`] (factored out so the
        // watcher-loss → `ViewerClosed` exit path is unit-testable without a
        // real CDP connection).
        let forwarder = tokio::spawn(forward_watcher_changes(Arc::clone(&inner), chg_rx));

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

    /// Apply a viewer viewport resize (the app panel measured its size). The
    /// page reflows to `width×height` CSS px and the screencast frames arrive
    /// at the matching aspect ratio. Remembered per-viewer so a later tab
    /// switch re-applies it. No-op for an unknown viewer.
    pub async fn resize(&self, viewer_session_id: Uuid, width: u32, height: u32) {
        let Some(inner) = self
            .sessions
            .get(&viewer_session_id)
            .map(|e| Arc::clone(&e.inner))
        else {
            tracing::debug!(viewer = %viewer_session_id, "resize for unknown viewer; dropping");
            return;
        };
        inner.resize(width, height).await;
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

/// The forwarder task body: every watcher change (including the watcher's
/// initial list, possibly empty) goes to the hub as `ViewerTargets`, then
/// drives the auto-select rules:
///   * current target destroyed → stop, start the [`preferred_target`] of
///     the remaining pages (or go idle if none);
///   * idle and a page target appears → start streaming the preferred one.
/// The app mirrors this auto-select logic UI-side (`app::viewer::panel::
/// auto_select`) — the two must stay in lockstep, like the wire shape.
///
/// Exit: the loop ends when the watcher's change channel closes. That is
/// either a deliberate teardown (detach / attach-replace aborted us and is
/// dropping the watcher — `inner.detached` is set first) or the watcher's
/// browser-level CDP ws died under us (agent Chrome crashed/was killed). In
/// the latter case the viewer would otherwise just freeze on its last frame,
/// so we report `ViewerClosed` for a clean close on the app side. (When the
/// whole hub connection is being dropped — manager drop on reconnect — the
/// send simply fails; harmless.)
async fn forward_watcher_changes(inner: Arc<ViewerInner>, mut chg_rx: mpsc::Receiver<Vec<TargetInfo>>) {
    let viewer_session_id = inner.viewer_session_id;
    while let Some(targets) = chg_rx.recv().await {
        if inner
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
        match inner.current_target().await {
            Some(cur) if !targets.iter().any(|t| t.id == cur) => {
                // Our tab vanished: drop the dead stream, then pick up the
                // preferred surviving page (or stay idle).
                inner.stop_current().await;
                if let Some(next) = preferred_target(&targets) {
                    inner.switch_to(next.id.clone()).await;
                }
            }
            None => {
                if let Some(next) = preferred_target(&targets) {
                    inner.switch_to(next.id.clone()).await;
                }
            }
            Some(_) => {} // Current tab still alive: nothing to do.
        }
    }
    // Watcher change channel closed. Deliberate teardown (detached flag set
    // by `ViewerEntry::shutdown` before aborting us) → the hub is already
    // tearing the viewer route down; stay quiet. Otherwise the browser-level
    // CDP ws died (e.g. agent Chrome crashed): report a clean close instead
    // of leaving the viewer frozen on its last frame.
    if !inner.detached.load(Ordering::SeqCst) {
        tracing::warn!(
            viewer = %viewer_session_id,
            "target watcher channel closed (browser connection lost); closing viewer"
        );
        let _ = inner
            .tx
            .send(OutFrame::Text(ClientMsg::ViewerClosed {
                viewer_session_id,
                reason: Some("browser connection lost".to_string()),
            }))
            .await;
    }
}

/// The target the screencast should land on when (re)selecting without an
/// explicit user pick: the first non-(`about:blank` | `chrome://`) page,
/// falling back to the first page, else `None`.
///
/// LOCKSTEP: the app's `app::viewer::panel::auto_select` applies the same
/// preference when its `current` is gone/unset — the wire doesn't carry
/// "attached", so the app *mirrors* this decision to keep the highlighted
/// tab on the tab actually being streamed. Change one, change both. The
/// initial attach pick (`pick_page_entry_for_session`, P4) already prefers
/// non-blank pages the same way.
///
/// PURE.
fn preferred_target(targets: &[TargetInfo]) -> Option<&TargetInfo> {
    targets
        .iter()
        .find(|t| !t.url.starts_with("about:blank") && !t.url.starts_with("chrome://"))
        .or_else(|| targets.first())
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

    fn ti(id: &str, url: &str) -> TargetInfo {
        TargetInfo {
            id: id.into(),
            title: format!("title {id}"),
            url: url.into(),
            kind: "page".into(),
        }
    }

    // ---- preferred_target (lockstep with app::viewer::panel::auto_select) --

    #[test]
    fn preferred_target_table() {
        let blank_then_real = vec![ti("B", "about:blank"), ti("R", "https://example.com/")];
        let only_blank = vec![ti("B", "about:blank")];
        let chrome_then_real = vec![ti("C", "chrome://newtab/"), ti("R", "https://example.com/")];
        let all_blankish = vec![ti("B", "about:blank"), ti("C", "chrome://gpu")];
        let cases: Vec<(&[TargetInfo], Option<&str>, &str)> = vec![
            (&blank_then_real, Some("R"), "blank + real → the real page"),
            (&only_blank, Some("B"), "only blank → fall back to it"),
            (&chrome_then_real, Some("R"), "chrome:// + real → the real page"),
            (&all_blankish, Some("B"), "all blankish → first page"),
            (&[], None, "empty → none"),
        ];
        for (targets, want, why) in cases {
            assert_eq!(preferred_target(targets).map(|t| t.id.as_str()), want, "{why}");
        }
    }

    // ---- forwarder exit path (Fix 1): watcher loss vs deliberate detach ----

    /// A `ViewerInner` with live channels but no CDP anywhere near it — enough
    /// to drive [`forward_watcher_changes`] (which only touches CDP via
    /// `switch_to`, never reached when only empty target lists flow).
    fn test_inner(out_tx: mpsc::Sender<OutFrame>) -> Arc<ViewerInner> {
        let (frame_tx, _frame_rx_kept_alive) = mpsc::channel::<Vec<u8>>(FRAME_QUEUE);
        // Leak the receiver into a no-op task so frame_tx stays usable.
        tokio::spawn(async move {
            let mut rx = _frame_rx_kept_alive;
            while rx.recv().await.is_some() {}
        });
        Arc::new(ViewerInner {
            viewer_session_id: Uuid::new_v4(),
            cdp_http_url: "http://127.0.0.1:1".into(), // never dialed in these tests
            frame_tx,
            stream: tokio::sync::Mutex::new(None),
            viewport: std::sync::Mutex::new(Viewport::default()),
            tx: out_tx,
            detached: AtomicBool::new(false),
        })
    }

    /// Watcher change channel closing WITHOUT a detach (browser-level CDP ws
    /// died) → the forwarder reports `ViewerClosed { reason: "browser
    /// connection lost" }` so the viewer gets a clean close, not a frozen
    /// last frame.
    #[tokio::test]
    async fn forwarder_emits_viewer_closed_when_watcher_dies() {
        let (out_tx, mut out_rx) = mpsc::channel::<OutFrame>(8);
        let inner = test_inner(out_tx);
        let viewer = inner.viewer_session_id;
        let (chg_tx, chg_rx) = mpsc::channel::<Vec<TargetInfo>>(TARGETS_QUEUE);

        let fwd = tokio::spawn(forward_watcher_changes(Arc::clone(&inner), chg_rx));
        // One ordinary (empty) change flows through as ViewerTargets…
        chg_tx.send(vec![]).await.unwrap();
        // …then the watcher dies: its change sender drops.
        drop(chg_tx);
        fwd.await.unwrap();

        match out_rx.recv().await {
            Some(OutFrame::Text(ClientMsg::ViewerTargets { viewer_session_id, targets })) => {
                assert_eq!(viewer_session_id, viewer);
                assert!(targets.is_empty());
            }
            Some(OutFrame::Text(_)) => panic!("expected ViewerTargets first, got another text frame"),
            Some(OutFrame::Binary(_)) => panic!("expected ViewerTargets first, got a binary frame"),
            None => panic!("expected ViewerTargets first, channel closed"),
        }
        match out_rx.recv().await {
            Some(OutFrame::Text(ClientMsg::ViewerClosed { viewer_session_id, reason })) => {
                assert_eq!(viewer_session_id, viewer);
                assert_eq!(reason.as_deref(), Some("browser connection lost"));
            }
            Some(OutFrame::Text(_)) => panic!("expected ViewerClosed, got another text frame"),
            Some(OutFrame::Binary(_)) => panic!("expected ViewerClosed, got a binary frame"),
            None => panic!("expected ViewerClosed on watcher loss, channel closed"),
        }
    }

    /// The same channel close during a DELIBERATE teardown (detach set the
    /// flag before aborting) must stay quiet — the hub viewer route is
    /// already being torn down.
    #[tokio::test]
    async fn forwarder_stays_quiet_on_deliberate_detach() {
        let (out_tx, mut out_rx) = mpsc::channel::<OutFrame>(8);
        let inner = test_inner(out_tx);
        let (chg_tx, chg_rx) = mpsc::channel::<Vec<TargetInfo>>(TARGETS_QUEUE);

        // Detach marks first (as ViewerEntry::shutdown does), then the
        // watcher drops. (No abort here: this exercises exactly the race
        // window where the forwarder reaches its exit path anyway.)
        inner.detached.store(true, Ordering::SeqCst);
        let fwd = tokio::spawn(forward_watcher_changes(Arc::clone(&inner), chg_rx));
        drop(chg_tx);
        fwd.await.unwrap();

        // Drop the last `ViewerInner` (it holds the only remaining out_tx
        // clone) so a quiet channel reads as a clean None, not a hang.
        drop(inner);
        assert!(
            out_rx.recv().await.is_none(),
            "no ViewerClosed (or anything else) on a deliberate detach"
        );
    }

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
