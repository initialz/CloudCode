//! Agent-side CDP screencast client (Desktop App P2 Task 1).
//!
//! A minimal hand-rolled Chrome DevTools Protocol (CDP) client over
//! `tokio-tungstenite` — deliberately NOT chromiumoxide, to keep the single
//! self-contained agent binary philosophy.
//!
//! It connects to a Chrome PAGE target's debugger websocket, runs
//! `Page.startScreencast`, streams the JPEG frames out over an mpsc channel, and
//! injects `Input.*` commands (mouse / keyboard / IME text) back into the page.
//!
//! The module is split into:
//!   * pure, unit-tested helpers — target picking, CDP command builders, the
//!     `ViewerInputEvent` → CDP mapper, and frame extraction; and
//!   * the live [`ScreencastSession`], which owns the websocket read/write task
//!     and is exercised by the (ignored) real-Chrome integration test.
//!
//! `ViewerInputEvent` is intentionally clean (serde tag/snake_case) because P2
//! Task 2 shares it across the agent↔hub protocol.

// Several items here (`ScreencastSession::input`, `stop`, the input mappers) are
// only consumed by P2 Task 2/3 and the ignored integration test. Mirror the
// rest of the `browser` module's `#[allow(dead_code)]` so the workspace's
// zero-warning bar isn't tripped before the consumers land.
#![allow(dead_code)]

use crate::tunnel::{TargetInfo, ViewerInputEvent};
use anyhow::{anyhow, Result};
use base64::Engine as _;
use futures::{SinkExt, StreamExt};
use serde_json::{json, Value};
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_tungstenite::tungstenite::Message;

// `ViewerInputEvent` is the canonical agent↔hub protocol type, defined in
// `crate::tunnel` (P2 Task 2). It's imported here so `input_to_cdp` and the
// `ScreencastSession::input` path map the same wire shape the hub forwards.

// ---------------------------------------------------------------------------
// Pure helper: pick the best page target.
// ---------------------------------------------------------------------------

/// Parse the `GET /json` target array and return the `webSocketDebuggerUrl` of
/// the page target to screencast for a given session.
///
/// `target_hint` is the CDP **target id** (the `id` field of a `/json` entry) of
/// the page belonging to the session being viewed, when the agent has been able
/// to learn it (see [`EndpointState::page_hint_for`]). The `id`/`targetId` is
/// the only per-target correlation field `/json` exposes — `browserContextId` is
/// NOT in `/json`, only in CDP `Target.getTargets` (see the P4 page-mapping
/// notes). So the hint we carry through the screencast path is a target id.
///
/// Behaviour:
///   * **with a hint** — return the `webSocketDebuggerUrl` of the `type=="page"`
///     whose `id` matches the hint. If no such page exists (it was closed, or
///     the hint is stale), return `None` rather than leaking a different
///     session's page: a hinted attach that can't find its page should fail
///     closed, not silently screencast someone else's tab.
///   * **without a hint** (`None`) — fall back to the active-page heuristic:
///       1. a `type == "page"` whose `url` is a *real* page (not `about:blank` /
///          `chrome://…`) — first such wins;
///       2. otherwise the first `type == "page"` at all;
///       3. otherwise `None`.
///
/// Garbage / non-array input yields `None`.
///
/// NOTE: in the agent's current default (non-`--isolated`) playwright-mcp
/// config, all sessions share ONE browser context and ONE set of tabs, so
/// `page_hint_for` returns `None` and this falls back to the active page. The
/// hint pathway is the plumbing for an `--isolated` config where each session
/// gets a distinct context/target (which is what actually closes the P2
/// cross-page leak). See `docs/superpowers/plans/2026-06-10-p4-page-mapping-notes.md`.
pub fn pick_page_for_session(targets_json: &str, target_hint: Option<&str>) -> Option<String> {
    pick_page_entry_for_session(targets_json, target_hint).map(|(_, ws)| ws)
}

/// Like [`pick_page_for_session`], but also returns the chosen page's CDP
/// **target id** alongside its ws url, as `(target_id, ws_url)`. The viewer
/// manager (P6) needs the id to track which tab the screencast is on so the
/// tab bar can highlight it and `targetDestroyed` can be matched against it.
pub fn pick_page_entry_for_session(
    targets_json: &str,
    target_hint: Option<&str>,
) -> Option<(String, String)> {
    let arr = serde_json::from_str::<Value>(targets_json).ok()?;
    let arr = arr.as_array()?;

    let is_page = |t: &Value| t.get("type").and_then(Value::as_str) == Some("page");
    let entry_of = |t: &Value| -> Option<(String, String)> {
        let id = t.get("id").and_then(Value::as_str)?.to_string();
        let ws = t.get("webSocketDebuggerUrl").and_then(Value::as_str)?;
        Some((id, ws.to_string()))
    };

    // Hinted: only the page whose target id matches. Fail closed if absent — do
    // NOT fall back to the active page, or a hinted viewer could be handed a
    // different session's tab (the very leak this targeting closes).
    if let Some(hint) = target_hint {
        for t in arr {
            if !is_page(t) {
                continue;
            }
            if t.get("id").and_then(Value::as_str) == Some(hint) {
                return entry_of(t);
            }
        }
        return None;
    }

    // Unhinted fallback — Pass 1: a real, non-blank page with a ws url.
    for t in arr {
        if !is_page(t) {
            continue;
        }
        let url = t.get("url").and_then(Value::as_str).unwrap_or("");
        if url.starts_with("about:blank") || url.starts_with("chrome://") {
            continue;
        }
        if let Some(e) = entry_of(t) {
            return Some(e);
        }
    }

    // Unhinted fallback — Pass 2: the first page of any kind that has a ws url.
    for t in arr {
        if is_page(t) {
            if let Some(e) = entry_of(t) {
                return Some(e);
            }
        }
    }

    None
}

// ---------------------------------------------------------------------------
// Pure helpers: CDP command builders (compact JSON strings).
// ---------------------------------------------------------------------------

/// Compact-serialize a CDP command. Infallible for the shapes we build here, so
/// fall back to `{}` rather than panicking if serialization somehow fails.
fn compact(v: Value) -> String {
    serde_json::to_string(&v).unwrap_or_else(|_| "{}".to_string())
}

fn cmd_page_enable(id: i64) -> String {
    compact(json!({ "id": id, "method": "Page.enable" }))
}

/// Chrome only emits screencast frames for the FOREGROUND tab — verified
/// against real Chrome in `multi_target_watcher_and_switch`: starting a
/// screencast on a background tab yields zero frames. So every
/// `start_on_target` brings its page to front first. (Tab switching = the
/// agent-side browser switches tabs too, which matches the P6 "mirror the
/// visible tab" design.)
fn cmd_bring_to_front(id: i64) -> String {
    compact(json!({ "id": id, "method": "Page.bringToFront" }))
}

/// Default screencast viewport before any viewer resize arrives. Matches the
/// pre-resize fixed size so behaviour is unchanged until the app measures its
/// panel and sends a `ViewerResize`.
pub const DEFAULT_SCREENCAST_WIDTH: u32 = 1280;
pub const DEFAULT_SCREENCAST_HEIGHT: u32 = 800;

/// Clamp bounds for a requested viewport (CSS px). Mirrors the agent-side
/// clamp the design calls for; the app sends the panel's logical size, but a
/// rogue / extreme value must never reach CDP.
pub const MIN_VIEWPORT: u32 = 200;
pub const MAX_VIEWPORT: u32 = 4096;

/// Clamp a requested viewport dimension to `MIN_VIEWPORT..=MAX_VIEWPORT`.
pub fn clamp_viewport(v: u32) -> u32 {
    v.clamp(MIN_VIEWPORT, MAX_VIEWPORT)
}

fn cmd_start_screencast(id: i64) -> String {
    cmd_start_screencast_sized(id, DEFAULT_SCREENCAST_WIDTH, DEFAULT_SCREENCAST_HEIGHT)
}

/// `Page.startScreencast` with explicit `maxWidth`/`maxHeight` so the frames
/// arrive at the requested viewport instead of the fixed default. Chrome
/// accepts re-issuing `startScreencast` on a live page with new max dims,
/// which is how [`ScreencastSession::resize`] changes the frame size.
fn cmd_start_screencast_sized(id: i64, w: u32, h: u32) -> String {
    compact(json!({
        "id": id,
        "method": "Page.startScreencast",
        "params": {
            "format": "jpeg",
            "quality": 60,
            "maxWidth": w,
            "maxHeight": h,
            "everyNthFrame": 1,
        }
    }))
}

/// `Emulation.setDeviceMetricsOverride` — reflow the PAGE to `w×h` CSS px
/// (device scale 1, non-mobile). This is the standard CDP way to make a live
/// page lay out at a new viewport; pairing it with a sized screencast makes
/// the frames match the app panel's aspect ratio (no letterbox bars).
fn cmd_set_device_metrics(id: i64, w: u32, h: u32) -> String {
    compact(json!({
        "id": id,
        "method": "Emulation.setDeviceMetricsOverride",
        "params": {
            "width": w,
            "height": h,
            "deviceScaleFactor": 1,
            "mobile": false,
        }
    }))
}

/// `Emulation.clearDeviceMetricsOverride` — drop any viewport override so the
/// page reverts to its window size. Sent on stop so a detached viewer doesn't
/// leave the agent's page reflowed.
fn cmd_clear_device_metrics(id: i64) -> String {
    compact(json!({ "id": id, "method": "Emulation.clearDeviceMetricsOverride" }))
}

fn cmd_screencast_ack(id: i64, cdp_session: i64) -> String {
    compact(json!({
        "id": id,
        "method": "Page.screencastFrameAck",
        "params": { "sessionId": cdp_session }
    }))
}

fn cmd_stop_screencast(id: i64) -> String {
    compact(json!({ "id": id, "method": "Page.stopScreencast" }))
}

/// Map a [`ViewerInputEvent`] to its CDP `Input.*` command (compact JSON).
fn input_to_cdp(id: i64, ev: &ViewerInputEvent) -> String {
    let v = match ev {
        ViewerInputEvent::MouseMove { x, y } => json!({
            "id": id,
            "method": "Input.dispatchMouseEvent",
            "params": { "type": "mouseMoved", "x": x, "y": y }
        }),
        ViewerInputEvent::MouseButton {
            x,
            y,
            button,
            down,
            click_count,
        } => json!({
            "id": id,
            "method": "Input.dispatchMouseEvent",
            "params": {
                "type": if *down { "mousePressed" } else { "mouseReleased" },
                "x": x,
                "y": y,
                "button": button,
                "clickCount": click_count,
            }
        }),
        ViewerInputEvent::Wheel { x, y, dx, dy } => json!({
            "id": id,
            "method": "Input.dispatchMouseEvent",
            "params": {
                "type": "mouseWheel",
                "x": x,
                "y": y,
                "deltaX": dx,
                "deltaY": dy,
            }
        }),
        ViewerInputEvent::Key {
            key,
            code,
            text,
            down,
            modifiers,
        } => json!({
            "id": id,
            "method": "Input.dispatchKeyEvent",
            "params": {
                "type": if *down { "keyDown" } else { "keyUp" },
                "key": key,
                "code": code,
                "text": text,
                "modifiers": modifiers,
            }
        }),
        ViewerInputEvent::InsertText { text } => json!({
            "id": id,
            "method": "Input.insertText",
            "params": { "text": text }
        }),
    };
    compact(v)
}

// ---------------------------------------------------------------------------
// Pure helper: extract a screencast frame from an incoming CDP message.
// ---------------------------------------------------------------------------

/// If `cdp_msg` is a `Page.screencastFrame` event, return its
/// `(base64 data, sessionId)`; otherwise `None`.
fn extract_screencast_frame(cdp_msg: &Value) -> Option<(String, i64)> {
    if cdp_msg.get("method").and_then(Value::as_str) != Some("Page.screencastFrame") {
        return None;
    }
    let params = cdp_msg.get("params")?;
    let data = params.get("data").and_then(Value::as_str)?.to_string();
    let session = params.get("sessionId").and_then(Value::as_i64)?;
    Some((data, session))
}

// ---------------------------------------------------------------------------
// Live session.
// ---------------------------------------------------------------------------

/// A running screencast: owns the websocket read/write task and a command
/// channel to enqueue outgoing CDP commands (input injection, stop).
pub struct ScreencastSession {
    /// Outgoing CDP command strings → the ws-writer side of the task.
    cmd_tx: mpsc::Sender<String>,
    /// The spawned task driving the websocket. Aborted on [`stop`].
    task: JoinHandle<()>,
    /// Monotonic CDP command id source. Shared with the read loop (which mints
    /// ids for its acks) so ids never collide.
    next_id: Arc<AtomicI64>,
}

/// A requested viewport size (CSS px), already clamped to
/// `MIN_VIEWPORT..=MAX_VIEWPORT`. `Default` is the pre-resize fixed size so a
/// session started before any `ViewerResize` behaves exactly as before.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Viewport {
    pub width: u32,
    pub height: u32,
}

impl Default for Viewport {
    fn default() -> Self {
        Viewport {
            width: DEFAULT_SCREENCAST_WIDTH,
            height: DEFAULT_SCREENCAST_HEIGHT,
        }
    }
}

impl Viewport {
    /// Build a clamped viewport from raw requested dims.
    pub fn clamped(width: u32, height: u32) -> Viewport {
        Viewport {
            width: clamp_viewport(width),
            height: clamp_viewport(height),
        }
    }
}

impl Drop for ScreencastSession {
    /// Abort the ws task on drop. Critical: dropping a `JoinHandle` only
    /// DETACHES the task — without this, an abrupt agent↔hub disconnect (no
    /// `ViewerDetach` delivered, so `stop()` never runs) would leak a CDP
    /// screencast that keeps JPEG-encoding against Chrome until the page or
    /// the agent process dies. With this, dropping ViewerManager on a
    /// reconnect truly stops every in-flight screencast.
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl ScreencastSession {
    /// Connect to the page target behind `cdp_http_url`, start a JPEG
    /// screencast, and stream decoded frames to `frame_tx`.
    ///
    /// Thin wrapper over [`Self::start_on_target`]: resolves the page ws url
    /// via `GET /json` + [`pick_page_for_session`] first. `target_hint` is the
    /// per-session page target id (when known); `None` falls back to the
    /// active page. The P6 multi-target viewer mostly bypasses this and calls
    /// `start_on_target` with a ws url it picked from the target list itself.
    pub async fn start(
        cdp_http_url: &str,
        target_hint: Option<&str>,
        frame_tx: mpsc::Sender<Vec<u8>>,
    ) -> Result<Self> {
        let list_url = format!("{cdp_http_url}/json");
        let body = reqwest::get(&list_url)
            .await
            .map_err(|e| anyhow!("GET {list_url}: {e}"))?
            .text()
            .await
            .map_err(|e| anyhow!("reading {list_url} body: {e}"))?;

        let ws_url = pick_page_for_session(&body, target_hint)
            .ok_or_else(|| anyhow!("no suitable page target found at {list_url}"))?;

        Self::start_on_target(&ws_url, frame_tx).await
    }

    /// Like [`Self::start_on_target`] but at a specific viewport: the page is
    /// reflowed via `Emulation.setDeviceMetricsOverride` and the screencast
    /// started at matching max dims. The viewer manager calls this so a tab
    /// switch re-applies the viewer's current size instead of snapping back
    /// to the default.
    pub async fn start_on_target_sized(
        ws_url: &str,
        frame_tx: mpsc::Sender<Vec<u8>>,
        viewport: Viewport,
    ) -> Result<Self> {
        Self::start_on_target_inner(ws_url, frame_tx, viewport).await
    }

    /// Connect directly to a page target's debugger websocket, start a JPEG
    /// screencast, and stream decoded frames to `frame_tx`. No page picking —
    /// the caller already knows the exact target (P6 tab switching).
    ///
    /// Steps:
    ///   1. `connect_async` the page debugger ws;
    ///   2. send `Page.enable` + `Page.bringToFront` + `Page.startScreencast`
    ///      (background tabs emit NO frames — see [`cmd_bring_to_front`]);
    ///   3. spawn a task that selects between the ws read stream (decode frames
    ///      → `frame_tx`, ack each) and a command receiver (forward queued
    ///      outgoing commands to the ws).
    pub async fn start_on_target(ws_url: &str, frame_tx: mpsc::Sender<Vec<u8>>) -> Result<Self> {
        Self::start_on_target_inner(ws_url, frame_tx, Viewport::default()).await
    }

    async fn start_on_target_inner(
        ws_url: &str,
        frame_tx: mpsc::Sender<Vec<u8>>,
        viewport: Viewport,
    ) -> Result<Self> {
        let (ws, _) = tokio_tungstenite::connect_async(ws_url)
            .await
            .map_err(|e| anyhow!("connecting CDP ws {ws_url}: {e}"))?;
        let (mut sink, mut stream) = ws.split();

        let next_id = Arc::new(AtomicI64::new(1));
        let mint = |id: &AtomicI64| id.fetch_add(1, Ordering::Relaxed);

        // Bring the page up (and to the FRONT — background tabs emit no
        // frames) and start the screencast.
        sink.send(Message::Text(cmd_page_enable(mint(&next_id))))
            .await
            .map_err(|e| anyhow!("sending Page.enable: {e}"))?;
        sink.send(Message::Text(cmd_bring_to_front(mint(&next_id))))
            .await
            .map_err(|e| anyhow!("sending Page.bringToFront: {e}"))?;
        // Reflow the page to the requested viewport BEFORE starting the
        // screencast, but only when a viewer has actually resized (non-
        // default). At the default we skip the override entirely so behaviour
        // is byte-for-byte unchanged from before this feature.
        if viewport != Viewport::default() {
            sink.send(Message::Text(cmd_set_device_metrics(
                mint(&next_id),
                viewport.width,
                viewport.height,
            )))
            .await
            .map_err(|e| anyhow!("sending Emulation.setDeviceMetricsOverride: {e}"))?;
        }
        sink.send(Message::Text(cmd_start_screencast_sized(
            mint(&next_id),
            viewport.width,
            viewport.height,
        )))
        .await
        .map_err(|e| anyhow!("sending Page.startScreencast: {e}"))?;

        // Command channel: input()/stop enqueue here; the task forwards to ws.
        let (cmd_tx, mut cmd_rx) = mpsc::channel::<String>(64);

        let task_id = Arc::clone(&next_id);
        let task = tokio::spawn(async move {
            loop {
                tokio::select! {
                    // Outgoing: forward queued CDP commands to the ws.
                    cmd = cmd_rx.recv() => {
                        match cmd {
                            Some(text) => {
                                if let Err(e) = sink.send(Message::Text(text)).await {
                                    tracing::debug!(error = %e, "screencast ws send failed; ending");
                                    break;
                                }
                            }
                            // All senders dropped: nothing more to write. Keep
                            // reading frames until the ws itself closes.
                            None => {}
                        }
                    }
                    // Incoming: parse, decode screencast frames, ack them.
                    msg = stream.next() => {
                        match msg {
                            Some(Ok(Message::Text(text))) => {
                                let Ok(val) = serde_json::from_str::<Value>(&text) else {
                                    continue;
                                };
                                if let Some((data, cdp_session)) = extract_screencast_frame(&val) {
                                    match base64::engine::general_purpose::STANDARD.decode(&data) {
                                        Ok(bytes) => {
                                            // Drop on full: a slow viewer must not
                                            // back-pressure Chrome's encoder.
                                            if frame_tx.try_send(bytes).is_err() {
                                                tracing::trace!("frame_tx full; dropping screencast frame");
                                            }
                                        }
                                        Err(e) => {
                                            tracing::debug!(error = %e, "screencast frame base64 decode failed");
                                        }
                                    }
                                    // Must ack or Chrome stops pushing.
                                    let ack = cmd_screencast_ack(task_id.fetch_add(1, Ordering::Relaxed), cdp_session);
                                    if let Err(e) = sink.send(Message::Text(ack)).await {
                                        tracing::debug!(error = %e, "screencast ack send failed; ending");
                                        break;
                                    }
                                }
                                // Other CDP messages (command replies, events) ignored.
                            }
                            Some(Ok(Message::Close(_))) | None => {
                                tracing::debug!("screencast ws closed");
                                break;
                            }
                            Some(Ok(_)) => {} // ping/pong/binary: ignore.
                            Some(Err(e)) => {
                                tracing::debug!(error = %e, "screencast ws read error; ending");
                                break;
                            }
                        }
                    }
                }
            }
        });

        Ok(Self {
            cmd_tx,
            task,
            next_id,
        })
    }

    /// Enqueue a user-input event for injection into the page. Non-blocking: if
    /// the command channel is full we drop the event and log (input lag is
    /// preferable to blocking the caller / unbounded queueing).
    pub fn input(&self, ev: &ViewerInputEvent) {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let cmd = input_to_cdp(id, ev);
        if self.cmd_tx.try_send(cmd).is_err() {
            tracing::warn!("screencast cmd channel full; dropping input event");
        }
    }

    /// Resize this viewer's browser viewport to `(w, h)` CSS px so the page
    /// reflows to the app panel's aspect ratio.
    ///
    /// The CDP sequence (queued through the SAME cmd channel as input, so it
    /// serializes after any in-flight injection):
    ///   1. `Emulation.setDeviceMetricsOverride {width, height}` — reflows the
    ///      live page to the new viewport;
    ///   2. `Page.startScreencast` re-issued with the new `maxWidth`/
    ///      `maxHeight` — Chrome accepts re-running startScreencast on a live
    ///      page and the subsequent frames arrive at the new size.
    ///
    /// `w`/`h` are clamped to `MIN_VIEWPORT..=MAX_VIEWPORT` first. Non-blocking
    /// (`try_send`); if the cmd channel is momentarily full the resize is
    /// dropped and the next one wins (the app re-sends on the trailing edge).
    pub fn resize(&self, w: u32, h: u32) {
        let vp = Viewport::clamped(w, h);
        let metrics = cmd_set_device_metrics(
            self.next_id.fetch_add(1, Ordering::Relaxed),
            vp.width,
            vp.height,
        );
        let restart = cmd_start_screencast_sized(
            self.next_id.fetch_add(1, Ordering::Relaxed),
            vp.width,
            vp.height,
        );
        if self.cmd_tx.try_send(metrics).is_err() || self.cmd_tx.try_send(restart).is_err() {
            tracing::warn!("screencast cmd channel full; dropping resize");
        }
    }

    /// Stop the screencast and tear the session down: enqueue
    /// `Emulation.clearDeviceMetricsOverride` (so a detached viewer doesn't
    /// leave the page reflowed) + `Page.stopScreencast` (both best-effort),
    /// then abort the ws task so the websocket closes.
    pub async fn stop(self) {
        let _ = self
            .cmd_tx
            .try_send(cmd_clear_device_metrics(self.next_id.fetch_add(1, Ordering::Relaxed)));
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let _ = self.cmd_tx.try_send(cmd_stop_screencast(id));
        // Give the task a brief moment to flush the stop command before abort.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        self.task.abort();
    }
}

// ---------------------------------------------------------------------------
// Target watching (P6 multi-target viewer).
// ---------------------------------------------------------------------------

/// Should this CDP target appear in the viewer's tab list?
///
/// All `type == "page"` targets count — INCLUDING `about:blank` (a fresh blank
/// tab claude just opened is a real tab the user should see) — EXCEPT browser
/// chrome: `chrome://…` internals and `devtools://…` panels.
fn is_listable_page(kind: &str, url: &str) -> bool {
    kind == "page" && !url.starts_with("chrome://") && !url.starts_with("devtools://")
}

/// Parse a CDP `targetInfo` object (`params.targetInfo` of the
/// `Target.targetCreated` / `Target.targetInfoChanged` events) into a
/// [`TargetInfo`], regardless of whether it's a listable page. `None` if the
/// required fields are missing.
fn parse_cdp_target_info(info: &Value) -> Option<TargetInfo> {
    Some(TargetInfo {
        id: info.get("targetId").and_then(Value::as_str)?.to_string(),
        title: info
            .get("title")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        url: info
            .get("url")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        kind: info.get("type").and_then(Value::as_str)?.to_string(),
    })
}

/// Apply one browser-level CDP `Target.*` event to the viewer's target list.
/// Pure — the [`TargetWatcher`] read loop folds incoming events through this.
///
/// Returns the updated list and whether it actually changed:
///   * `Target.targetCreated`     — push if it's a listable page not already
///     present (Chrome replays `targetCreated` for every existing target right
///     after `Target.setDiscoverTargets`, hence the dedup).
///   * `Target.targetInfoChanged` — update title/url in place. Also handles
///     listability flips: a tracked page navigating to `chrome://…` is
///     removed; an untracked page navigating from `chrome://…` to a real url
///     is added.
///   * `Target.targetDestroyed`   — remove by `params.targetId`.
///
/// Anything else (other methods, non-page targets, malformed params) leaves
/// the list untouched with `changed = false`.
pub fn apply_target_event(
    mut list: Vec<TargetInfo>,
    event: &Value,
) -> (Vec<TargetInfo>, bool /* changed */) {
    let method = event.get("method").and_then(Value::as_str).unwrap_or("");
    match method {
        "Target.targetCreated" | "Target.targetInfoChanged" => {
            let Some(info) = event
                .get("params")
                .and_then(|p| p.get("targetInfo"))
                .and_then(parse_cdp_target_info)
            else {
                return (list, false);
            };
            let pos = list.iter().position(|t| t.id == info.id);
            if !is_listable_page(&info.kind, &info.url) {
                // Not listable (non-page, chrome://, devtools://). If we were
                // tracking it (page navigated INTO chrome://), drop it.
                return match pos {
                    Some(i) => {
                        list.remove(i);
                        (list, true)
                    }
                    None => (list, false),
                };
            }
            match pos {
                Some(i) => {
                    if list[i] == info {
                        (list, false) // No-op change (same title/url).
                    } else {
                        list[i] = info;
                        (list, true)
                    }
                }
                None => {
                    list.push(info);
                    (list, true)
                }
            }
        }
        "Target.targetDestroyed" => {
            let Some(id) = event
                .get("params")
                .and_then(|p| p.get("targetId"))
                .and_then(Value::as_str)
            else {
                return (list, false);
            };
            match list.iter().position(|t| t.id == id) {
                Some(i) => {
                    list.remove(i);
                    (list, true)
                }
                None => (list, false),
            }
        }
        _ => (list, false),
    }
}

/// Resolve the **browser-level** debugger ws url from `GET /json/version`
/// (`webSocketDebuggerUrl`). This is the endpoint that accepts
/// `Target.setDiscoverTargets`; page-level sockets don't see other targets.
async fn browser_ws_url(cdp_http_url: &str) -> Result<String> {
    let url = format!("{cdp_http_url}/json/version");
    let body = reqwest::get(&url)
        .await
        .map_err(|e| anyhow!("GET {url}: {e}"))?
        .text()
        .await
        .map_err(|e| anyhow!("reading {url} body: {e}"))?;
    let v: Value = serde_json::from_str(&body).map_err(|e| anyhow!("parsing {url}: {e}"))?;
    v.get("webSocketDebuggerUrl")
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| anyhow!("no webSocketDebuggerUrl in {url}"))
}

/// Resolve the page debugger ws url for `target_id` via `GET /json`.
///
/// CDP's `Target.targetCreated` event does NOT carry the ws url, so when the
/// viewer selects a tab we re-fetch `/json` (cheap, and selection is rare)
/// and join on the `id` field. `None` when the target is gone (stale tab
/// click) or isn't a page.
pub async fn page_ws_url(cdp_http_url: &str, target_id: &str) -> Option<String> {
    let list_url = format!("{cdp_http_url}/json");
    let body = reqwest::get(&list_url).await.ok()?.text().await.ok()?;
    let arr = serde_json::from_str::<Value>(&body).ok()?;
    arr.as_array()?.iter().find_map(|t| {
        if t.get("type").and_then(Value::as_str) == Some("page")
            && t.get("id").and_then(Value::as_str) == Some(target_id)
        {
            t.get("webSocketDebuggerUrl")
                .and_then(Value::as_str)
                .map(str::to_string)
        } else {
            None
        }
    })
}

/// Build the initial target list from a `GET /json` body — same listability
/// rules as [`apply_target_event`]. Pure for testability.
pub fn initial_targets_from_json(targets_json: &str) -> Vec<TargetInfo> {
    let Ok(v) = serde_json::from_str::<Value>(targets_json) else {
        return Vec::new();
    };
    let Some(arr) = v.as_array() else {
        return Vec::new();
    };
    arr.iter()
        .filter_map(|t| {
            let info = TargetInfo {
                id: t.get("id").and_then(Value::as_str)?.to_string(),
                title: t
                    .get("title")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
                url: t.get("url").and_then(Value::as_str).unwrap_or("").to_string(),
                kind: t.get("type").and_then(Value::as_str)?.to_string(),
            };
            is_listable_page(&info.kind, &info.url).then_some(info)
        })
        .collect()
}

/// Watches the resident Chrome's target list over the **browser-level** CDP
/// websocket (`Target.setDiscoverTargets {discover:true}`) and pushes the full
/// page-target list to `on_change` on every change — plus once up front with
/// the initial list (built from `GET /json`, so the consumer always gets a
/// deterministic first snapshot even when the browser is idle/empty).
///
/// [`Self::snapshot`] exposes the current list for pull-style reads (e.g.
/// re-sending the list after a stale tab selection).
pub struct TargetWatcher {
    /// The spawned ws read task. Aborted on drop — same lesson as
    /// [`ScreencastSession`]: dropping a `JoinHandle` only DETACHES the task,
    /// which would leak a browser-level CDP connection per detached viewer.
    task: JoinHandle<()>,
    /// Shared snapshot of the current list, updated by the read task.
    targets: Arc<std::sync::Mutex<Vec<TargetInfo>>>,
}

impl Drop for TargetWatcher {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl TargetWatcher {
    /// Connect to the browser-level ws behind `cdp_http_url`, enable target
    /// discovery, and start watching. Sends the initial list (possibly empty)
    /// to `on_change` before any event-driven update.
    pub async fn start(
        cdp_http_url: &str,
        on_change: mpsc::Sender<Vec<TargetInfo>>,
    ) -> Result<Self> {
        let ws_url = browser_ws_url(cdp_http_url).await?;
        let (ws, _) = tokio_tungstenite::connect_async(&ws_url)
            .await
            .map_err(|e| anyhow!("connecting browser CDP ws {ws_url}: {e}"))?;
        let (mut sink, mut stream) = ws.split();

        sink.send(Message::Text(compact(json!({
            "id": 1,
            "method": "Target.setDiscoverTargets",
            "params": { "discover": true }
        }))))
        .await
        .map_err(|e| anyhow!("sending Target.setDiscoverTargets: {e}"))?;

        // Deterministic initial snapshot from /json. The replayed
        // `targetCreated` events Chrome sends right after setDiscoverTargets
        // then dedup against this list in `apply_target_event` (no-op pushes
        // → changed=false → no duplicate notification).
        let list_url = format!("{cdp_http_url}/json");
        let body = reqwest::get(&list_url)
            .await
            .map_err(|e| anyhow!("GET {list_url}: {e}"))?
            .text()
            .await
            .map_err(|e| anyhow!("reading {list_url} body: {e}"))?;
        let initial = initial_targets_from_json(&body);

        let targets = Arc::new(std::sync::Mutex::new(initial.clone()));
        let shared = Arc::clone(&targets);

        let task = tokio::spawn(async move {
            // Always announce the initial list, even when empty — the viewer
            // side uses it to render "browser idle" instead of hanging.
            if on_change.send(initial).await.is_err() {
                return;
            }
            while let Some(msg) = stream.next().await {
                match msg {
                    Ok(Message::Text(text)) => {
                        let Ok(val) = serde_json::from_str::<Value>(&text) else {
                            continue;
                        };
                        let current = shared.lock().expect("targets lock").clone();
                        let (next, changed) = apply_target_event(current, &val);
                        if changed {
                            *shared.lock().expect("targets lock") = next.clone();
                            if on_change.send(next).await.is_err() {
                                // Consumer gone (viewer detached): stop watching.
                                return;
                            }
                        }
                    }
                    Ok(Message::Close(_)) | Err(_) => {
                        tracing::debug!("target watcher ws closed");
                        return;
                    }
                    Ok(_) => {} // ping/pong/binary: ignore.
                }
            }
        });

        Ok(Self { task, targets })
    }

    /// The current target list (clone of the watcher's shared state).
    pub fn snapshot(&self) -> Vec<TargetInfo> {
        self.targets.lock().expect("targets lock").clone()
    }

    /// Stop watching: abort the ws read task (Drop does the same; this is the
    /// explicit form for symmetry with `ScreencastSession::stop`).
    pub fn stop(self) {
        self.task.abort();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- pick_page_for_session: unhinted fallback (active page) -----------
    // These lock the legacy `pick_page_target` behaviour, now reached with
    // `target_hint = None` (the agent's current default-config path).
    // Fixtures carry `id` because picking now also resolves the target id
    // (real Chrome /json always includes it).

    #[test]
    fn pick_prefers_real_page_over_blank() {
        let json = r#"[
            {"type":"background_page","id":"T0","url":"chrome-extension://x","webSocketDebuggerUrl":"ws://bg"},
            {"type":"page","id":"T1","url":"about:blank","webSocketDebuggerUrl":"ws://blank"},
            {"type":"page","id":"T2","url":"https://example.com/","webSocketDebuggerUrl":"ws://real"}
        ]"#;
        assert_eq!(
            pick_page_for_session(json, None),
            Some("ws://real".to_string())
        );
        // The entry variant also reports WHICH target was picked.
        assert_eq!(
            pick_page_entry_for_session(json, None),
            Some(("T2".to_string(), "ws://real".to_string()))
        );
    }

    #[test]
    fn pick_skips_chrome_scheme_pages() {
        let json = r#"[
            {"type":"page","id":"T1","url":"chrome://newtab/","webSocketDebuggerUrl":"ws://newtab"},
            {"type":"page","id":"T2","url":"data:text/html,<h1>hi</h1>","webSocketDebuggerUrl":"ws://data"}
        ]"#;
        assert_eq!(
            pick_page_for_session(json, None),
            Some("ws://data".to_string())
        );
    }

    #[test]
    fn pick_falls_back_to_first_page_when_all_blank() {
        let json = r#"[
            {"type":"webview","id":"T0","url":"x","webSocketDebuggerUrl":"ws://wv"},
            {"type":"page","id":"T1","url":"about:blank","webSocketDebuggerUrl":"ws://first"},
            {"type":"page","id":"T2","url":"chrome://gpu","webSocketDebuggerUrl":"ws://second"}
        ]"#;
        assert_eq!(
            pick_page_for_session(json, None),
            Some("ws://first".to_string())
        );
        assert_eq!(
            pick_page_entry_for_session(json, None),
            Some(("T1".to_string(), "ws://first".to_string()))
        );
    }

    #[test]
    fn pick_none_when_no_page() {
        let json = r#"[
            {"type":"background_page","url":"chrome-extension://x","webSocketDebuggerUrl":"ws://bg"},
            {"type":"service_worker","url":"y","webSocketDebuggerUrl":"ws://sw"}
        ]"#;
        assert_eq!(pick_page_for_session(json, None), None);
    }

    #[test]
    fn pick_none_on_garbage() {
        assert_eq!(pick_page_for_session("not json at all", None), None);
        assert_eq!(pick_page_for_session("{}", None), None);
        assert_eq!(pick_page_for_session("42", None), None);
        assert_eq!(pick_page_for_session("", None), None);
        // Garbage is None regardless of a hint, too.
        assert_eq!(pick_page_for_session("not json", Some("T1")), None);
    }

    #[test]
    fn pick_skips_page_without_ws_url() {
        let json = r#"[
            {"type":"page","id":"T_A","url":"https://a.com/"},
            {"type":"page","id":"T_B","url":"https://b.com/","webSocketDebuggerUrl":"ws://b"}
        ]"#;
        assert_eq!(pick_page_for_session(json, None), Some("ws://b".to_string()));
    }

    // --- pick_page_for_session: hinted (per-session target id) ------------

    #[test]
    fn pick_hint_matches_that_target() {
        // Two real pages; the hint must select its own, not the "active" first.
        let json = r#"[
            {"type":"page","id":"T_A","url":"https://a.com/","webSocketDebuggerUrl":"ws://a"},
            {"type":"page","id":"T_B","url":"https://b.com/","webSocketDebuggerUrl":"ws://b"}
        ]"#;
        assert_eq!(
            pick_page_for_session(json, Some("T_B")),
            Some("ws://b".to_string())
        );
        assert_eq!(
            pick_page_for_session(json, Some("T_A")),
            Some("ws://a".to_string())
        );
    }

    #[test]
    fn pick_hint_no_match_fails_closed() {
        // A hint that matches nothing returns None — it must NOT fall back to
        // the active page (that would hand a hinted viewer a different
        // session's tab — the exact leak per-session targeting closes).
        let json = r#"[
            {"type":"page","id":"T_A","url":"https://a.com/","webSocketDebuggerUrl":"ws://a"}
        ]"#;
        assert_eq!(pick_page_for_session(json, Some("T_MISSING")), None);
    }

    #[test]
    fn pick_hint_ignores_non_page_with_matching_id() {
        // A non-page target carrying the same id must not be selected.
        let json = r#"[
            {"type":"iframe","id":"T_X","url":"https://x/","webSocketDebuggerUrl":"ws://iframe"},
            {"type":"page","id":"T_X","url":"https://x/","webSocketDebuggerUrl":"ws://page"}
        ]"#;
        assert_eq!(
            pick_page_for_session(json, Some("T_X")),
            Some("ws://page".to_string())
        );
    }

    #[test]
    fn pick_hint_matched_page_without_ws_is_none() {
        let json = r#"[
            {"type":"page","id":"T_A","url":"https://a.com/"}
        ]"#;
        assert_eq!(pick_page_for_session(json, Some("T_A")), None);
    }

    // --- CDP command builders --------------------------------------------

    fn parse(s: &str) -> Value {
        serde_json::from_str(s).expect("command must be valid JSON")
    }

    #[test]
    fn page_enable_shape() {
        let v = parse(&cmd_page_enable(7));
        assert_eq!(v["id"], 7);
        assert_eq!(v["method"], "Page.enable");
    }

    #[test]
    fn bring_to_front_shape() {
        let v = parse(&cmd_bring_to_front(5));
        assert_eq!(v["id"], 5);
        assert_eq!(v["method"], "Page.bringToFront");
    }

    #[test]
    fn start_screencast_shape() {
        let v = parse(&cmd_start_screencast(3));
        assert_eq!(v["id"], 3);
        assert_eq!(v["method"], "Page.startScreencast");
        let p = &v["params"];
        assert_eq!(p["format"], "jpeg");
        assert_eq!(p["quality"], 60);
        assert_eq!(p["maxWidth"], 1280);
        assert_eq!(p["maxHeight"], 800);
        assert_eq!(p["everyNthFrame"], 1);
    }

    #[test]
    fn start_screencast_sized_honors_dims() {
        let v = parse(&cmd_start_screencast_sized(4, 800, 600));
        assert_eq!(v["id"], 4);
        assert_eq!(v["method"], "Page.startScreencast");
        let p = &v["params"];
        assert_eq!(p["format"], "jpeg");
        assert_eq!(p["quality"], 60);
        assert_eq!(p["maxWidth"], 800);
        assert_eq!(p["maxHeight"], 600);
        assert_eq!(p["everyNthFrame"], 1);
        // The default builder is exactly the sized one at the default dims.
        let d = parse(&cmd_start_screencast(4));
        assert_eq!(d["params"]["maxWidth"], DEFAULT_SCREENCAST_WIDTH);
        assert_eq!(d["params"]["maxHeight"], DEFAULT_SCREENCAST_HEIGHT);
    }

    #[test]
    fn set_device_metrics_shape() {
        let v = parse(&cmd_set_device_metrics(8, 800, 600));
        assert_eq!(v["id"], 8);
        assert_eq!(v["method"], "Emulation.setDeviceMetricsOverride");
        let p = &v["params"];
        assert_eq!(p["width"], 800);
        assert_eq!(p["height"], 600);
        assert_eq!(p["deviceScaleFactor"], 1);
        assert_eq!(p["mobile"], false);
    }

    #[test]
    fn clear_device_metrics_shape() {
        let v = parse(&cmd_clear_device_metrics(9));
        assert_eq!(v["id"], 9);
        assert_eq!(v["method"], "Emulation.clearDeviceMetricsOverride");
    }

    #[test]
    fn clamp_viewport_bounds() {
        assert_eq!(clamp_viewport(0), MIN_VIEWPORT);
        assert_eq!(clamp_viewport(199), MIN_VIEWPORT);
        assert_eq!(clamp_viewport(200), 200);
        assert_eq!(clamp_viewport(800), 800);
        assert_eq!(clamp_viewport(4096), 4096);
        assert_eq!(clamp_viewport(99999), MAX_VIEWPORT);
        // The clamped constructor applies it to both dims.
        assert_eq!(
            Viewport::clamped(10, 99999),
            Viewport {
                width: MIN_VIEWPORT,
                height: MAX_VIEWPORT
            }
        );
        // Default is the pre-resize fixed size.
        assert_eq!(
            Viewport::default(),
            Viewport {
                width: DEFAULT_SCREENCAST_WIDTH,
                height: DEFAULT_SCREENCAST_HEIGHT
            }
        );
    }

    #[test]
    fn screencast_ack_shape() {
        let v = parse(&cmd_screencast_ack(11, 42));
        assert_eq!(v["id"], 11);
        assert_eq!(v["method"], "Page.screencastFrameAck");
        assert_eq!(v["params"]["sessionId"], 42);
    }

    #[test]
    fn stop_screencast_shape() {
        let v = parse(&cmd_stop_screencast(9));
        assert_eq!(v["id"], 9);
        assert_eq!(v["method"], "Page.stopScreencast");
    }

    // --- input_to_cdp -----------------------------------------------------

    #[test]
    fn mouse_move_maps() {
        let ev = ViewerInputEvent::MouseMove { x: 10.0, y: 20.0 };
        let v = parse(&input_to_cdp(1, &ev));
        assert_eq!(v["method"], "Input.dispatchMouseEvent");
        assert_eq!(v["params"]["type"], "mouseMoved");
        assert_eq!(v["params"]["x"], 10.0);
        assert_eq!(v["params"]["y"], 20.0);
    }

    #[test]
    fn mouse_button_down_maps() {
        let ev = ViewerInputEvent::MouseButton {
            x: 5.0,
            y: 6.0,
            button: "left".to_string(),
            down: true,
            click_count: 2,
        };
        let v = parse(&input_to_cdp(1, &ev));
        assert_eq!(v["method"], "Input.dispatchMouseEvent");
        assert_eq!(v["params"]["type"], "mousePressed");
        assert_eq!(v["params"]["button"], "left");
        assert_eq!(v["params"]["clickCount"], 2);
    }

    #[test]
    fn mouse_button_up_maps() {
        let ev = ViewerInputEvent::MouseButton {
            x: 1.0,
            y: 2.0,
            button: "right".to_string(),
            down: false,
            click_count: 1,
        };
        let v = parse(&input_to_cdp(1, &ev));
        assert_eq!(v["params"]["type"], "mouseReleased");
    }

    #[test]
    fn wheel_maps() {
        let ev = ViewerInputEvent::Wheel {
            x: 3.0,
            y: 4.0,
            dx: -1.0,
            dy: 120.0,
        };
        let v = parse(&input_to_cdp(1, &ev));
        assert_eq!(v["method"], "Input.dispatchMouseEvent");
        assert_eq!(v["params"]["type"], "mouseWheel");
        assert_eq!(v["params"]["deltaX"], -1.0);
        assert_eq!(v["params"]["deltaY"], 120.0);
    }

    #[test]
    fn key_down_maps() {
        let ev = ViewerInputEvent::Key {
            key: "a".to_string(),
            code: "KeyA".to_string(),
            text: "a".to_string(),
            down: true,
            modifiers: 8,
        };
        let v = parse(&input_to_cdp(1, &ev));
        assert_eq!(v["method"], "Input.dispatchKeyEvent");
        assert_eq!(v["params"]["type"], "keyDown");
        assert_eq!(v["params"]["key"], "a");
        assert_eq!(v["params"]["code"], "KeyA");
        assert_eq!(v["params"]["text"], "a");
        assert_eq!(v["params"]["modifiers"], 8);
    }

    #[test]
    fn key_up_maps() {
        let ev = ViewerInputEvent::Key {
            key: "Enter".to_string(),
            code: "Enter".to_string(),
            text: "".to_string(),
            down: false,
            modifiers: 0,
        };
        let v = parse(&input_to_cdp(1, &ev));
        assert_eq!(v["params"]["type"], "keyUp");
    }

    #[test]
    fn insert_text_maps() {
        let ev = ViewerInputEvent::InsertText {
            text: "你好".to_string(),
        };
        let v = parse(&input_to_cdp(1, &ev));
        assert_eq!(v["method"], "Input.insertText");
        assert_eq!(v["params"]["text"], "你好");
    }

    #[test]
    fn viewer_input_event_serde_roundtrip() {
        // The wire shape is shared with Task 2; lock the tag/snake_case form.
        let ev = ViewerInputEvent::MouseButton {
            x: 1.0,
            y: 2.0,
            button: "left".to_string(),
            down: true,
            click_count: 1,
        };
        let s = serde_json::to_string(&ev).unwrap();
        assert!(s.contains("\"kind\":\"mouse_button\""), "got {s}");
        let back: ViewerInputEvent = serde_json::from_str(&s).unwrap();
        assert_eq!(back, ev);

        let it: ViewerInputEvent =
            serde_json::from_str(r#"{"kind":"insert_text","text":"hi"}"#).unwrap();
        assert_eq!(it, ViewerInputEvent::InsertText { text: "hi".into() });
    }

    // --- extract_screencast_frame ----------------------------------------

    #[test]
    fn extract_frame_ok() {
        let msg = json!({
            "method": "Page.screencastFrame",
            "params": { "data": "/9j/abc", "sessionId": 17, "metadata": {} }
        });
        let got = extract_screencast_frame(&msg);
        assert_eq!(got, Some(("/9j/abc".to_string(), 17)));
    }

    #[test]
    fn extract_frame_wrong_method() {
        let msg = json!({ "method": "Page.loadEventFired", "params": {} });
        assert_eq!(extract_screencast_frame(&msg), None);
    }

    #[test]
    fn extract_frame_missing_fields() {
        let msg = json!({ "method": "Page.screencastFrame", "params": { "data": "x" } });
        assert_eq!(extract_screencast_frame(&msg), None);
        let msg2 = json!({ "method": "Page.screencastFrame" });
        assert_eq!(extract_screencast_frame(&msg2), None);
    }

    // --- apply_target_event (P6 target watching) ---------------------------

    fn ti(id: &str, title: &str, url: &str) -> TargetInfo {
        TargetInfo {
            id: id.into(),
            title: title.into(),
            url: url.into(),
            kind: "page".into(),
        }
    }

    fn ev_created(id: &str, kind: &str, title: &str, url: &str) -> Value {
        json!({
            "method": "Target.targetCreated",
            "params": { "targetInfo": {
                "targetId": id, "type": kind, "title": title, "url": url, "attached": false
            }}
        })
    }

    fn ev_changed(id: &str, kind: &str, title: &str, url: &str) -> Value {
        json!({
            "method": "Target.targetInfoChanged",
            "params": { "targetInfo": {
                "targetId": id, "type": kind, "title": title, "url": url, "attached": true
            }}
        })
    }

    fn ev_destroyed(id: &str) -> Value {
        json!({ "method": "Target.targetDestroyed", "params": { "targetId": id } })
    }

    #[test]
    fn target_events_table() {
        // (name, starting list, event, expected list, expected changed)
        let cases: Vec<(&str, Vec<TargetInfo>, Value, Vec<TargetInfo>, bool)> = vec![
            (
                "created page is added",
                vec![],
                ev_created("T1", "page", "Example", "https://example.com/"),
                vec![ti("T1", "Example", "https://example.com/")],
                true,
            ),
            (
                "created about:blank page IS added (fresh tab is real)",
                vec![ti("T1", "A", "https://a/")],
                ev_created("T2", "page", "", "about:blank"),
                vec![ti("T1", "A", "https://a/"), ti("T2", "", "about:blank")],
                true,
            ),
            (
                "created duplicate (setDiscoverTargets replay) is a no-op",
                vec![ti("T1", "A", "https://a/")],
                ev_created("T1", "page", "A", "https://a/"),
                vec![ti("T1", "A", "https://a/")],
                false,
            ),
            (
                "created non-page is ignored",
                vec![],
                ev_created("SW1", "service_worker", "", "https://a/sw.js"),
                vec![],
                false,
            ),
            (
                "created chrome:// page is ignored",
                vec![],
                ev_created("T9", "page", "New Tab", "chrome://newtab/"),
                vec![],
                false,
            ),
            (
                "created devtools:// page is ignored",
                vec![],
                ev_created("T9", "page", "DevTools", "devtools://devtools/bundled/x.html"),
                vec![],
                false,
            ),
            (
                "infoChanged updates title+url in place",
                vec![ti("T1", "", "about:blank"), ti("T2", "B", "https://b/")],
                ev_changed("T1", "page", "百度一下", "https://www.baidu.com/"),
                vec![
                    ti("T1", "百度一下", "https://www.baidu.com/"),
                    ti("T2", "B", "https://b/"),
                ],
                true,
            ),
            (
                "infoChanged with identical info is a no-op",
                vec![ti("T1", "A", "https://a/")],
                ev_changed("T1", "page", "A", "https://a/"),
                vec![ti("T1", "A", "https://a/")],
                false,
            ),
            (
                "infoChanged for an untracked real page adds it",
                vec![],
                ev_changed("T3", "page", "C", "https://c/"),
                vec![ti("T3", "C", "https://c/")],
                true,
            ),
            (
                "infoChanged navigating a tracked page into chrome:// removes it",
                vec![ti("T1", "A", "https://a/")],
                ev_changed("T1", "page", "Settings", "chrome://settings/"),
                vec![],
                true,
            ),
            (
                "destroyed removes the target",
                vec![ti("T1", "A", "https://a/"), ti("T2", "B", "https://b/")],
                ev_destroyed("T1"),
                vec![ti("T2", "B", "https://b/")],
                true,
            ),
            (
                "destroyed unknown id is a no-op",
                vec![ti("T1", "A", "https://a/")],
                ev_destroyed("T_GONE"),
                vec![ti("T1", "A", "https://a/")],
                false,
            ),
            (
                "unrelated method leaves list unchanged",
                vec![ti("T1", "A", "https://a/")],
                json!({ "method": "Page.loadEventFired", "params": {} }),
                vec![ti("T1", "A", "https://a/")],
                false,
            ),
            (
                "command reply (no method) leaves list unchanged",
                vec![ti("T1", "A", "https://a/")],
                json!({ "id": 1, "result": {} }),
                vec![ti("T1", "A", "https://a/")],
                false,
            ),
            (
                "malformed params leave list unchanged",
                vec![ti("T1", "A", "https://a/")],
                json!({ "method": "Target.targetCreated", "params": {} }),
                vec![ti("T1", "A", "https://a/")],
                false,
            ),
        ];

        for (name, start, event, want_list, want_changed) in cases {
            let (got, changed) = apply_target_event(start, &event);
            assert_eq!(got, want_list, "case: {name}");
            assert_eq!(changed, want_changed, "case: {name} (changed flag)");
        }
    }

    #[test]
    fn initial_targets_filter_matches_event_rules() {
        let json = r#"[
            {"type":"page","id":"T1","title":"Blank","url":"about:blank","webSocketDebuggerUrl":"ws://1"},
            {"type":"page","id":"T2","title":"New Tab","url":"chrome://newtab/","webSocketDebuggerUrl":"ws://2"},
            {"type":"page","id":"T3","title":"Real","url":"https://example.com/","webSocketDebuggerUrl":"ws://3"},
            {"type":"service_worker","id":"SW","title":"","url":"https://example.com/sw.js"}
        ]"#;
        let list = initial_targets_from_json(json);
        assert_eq!(
            list,
            vec![
                ti("T1", "Blank", "about:blank"),
                ti("T3", "Real", "https://example.com/"),
            ]
        );
        // Garbage → empty, not panic.
        assert!(initial_targets_from_json("not json").is_empty());
        assert!(initial_targets_from_json("{}").is_empty());
    }

    // --- integration: real Chrome ----------------------------------------

    /// Real-Chrome screencast integration. Run manually:
    /// `cargo test -p cloudcode-agent screencast_streams_real -- --ignored --nocapture`
    ///
    /// Starts a real headless Chrome, opens a non-blank data: page, runs a
    /// screencast, and asserts a JPEG frame (magic bytes FF D8) arrives within
    /// 5s, then injects a MouseMove (must not panic) and stops.
    #[tokio::test]
    #[ignore = "requires a real Chrome install; run manually"]
    async fn screencast_streams_real_jpeg() {
        use crate::browser::chrome::ChromeManager;
        use crate::config::BrowserConfig;
        use std::sync::Arc;
        use std::time::Duration;

        let cfg = BrowserConfig {
            enabled: true,
            chrome_path: None,
            cdp_port: 19244,
            mcp_port: 7110,
            mcp_command: None,
        };
        let tmp = tempfile::tempdir().unwrap();
        let mgr = Arc::new(ChromeManager::new(cfg, tmp.path()));
        mgr.start().await.expect("Chrome should start and become ready");
        let cdp = mgr.cdp_http_url();

        // Open a non-blank page so the screencast has real pixels. Newer Chrome
        // wants PUT on /json/new; older accepts GET. Try PUT first, GET fallback.
        let data_url = "data:text/html,<h1 style=font-size:80px>P2</h1>";
        let new_url = format!("{cdp}/json/new?{data_url}");
        let client = reqwest::Client::new();
        let put = client.put(&new_url).send().await;
        let opened = match put {
            Ok(r) if r.status().is_success() => true,
            _ => {
                let get = client.get(&new_url).send().await;
                matches!(get, Ok(r) if r.status().is_success())
            }
        };
        assert!(opened, "failed to open a data: page target via /json/new");

        // Give Chrome a moment to register + render the new target.
        tokio::time::sleep(Duration::from_millis(800)).await;

        let (frame_tx, mut frame_rx) = mpsc::channel::<Vec<u8>>(8);
        let session = ScreencastSession::start(&cdp, None, frame_tx)
            .await
            .expect("screencast should start");

        // Await the first frame within 5s and assert JPEG magic bytes.
        let frame = tokio::time::timeout(Duration::from_secs(5), frame_rx.recv())
            .await
            .expect("should receive a frame within 5s")
            .expect("frame channel should yield a frame");
        assert!(frame.len() >= 2, "frame too short");
        assert_eq!(
            &frame[0..2],
            &[0xFF, 0xD8],
            "frame must start with JPEG magic bytes FF D8; got {:02X} {:02X}",
            frame[0],
            frame[1]
        );
        eprintln!(
            "got JPEG frame: {} bytes, magic {:02X} {:02X}",
            frame.len(),
            frame[0],
            frame[1]
        );

        // Inject a MouseMove — must not panic.
        session.input(&ViewerInputEvent::MouseMove { x: 10.0, y: 10.0 });

        session.stop().await;
        drop(mgr);
    }

    /// Read the width/height from a baseline/progressive JPEG's first SOF
    /// marker (0xFFC0..=0xFFCF except C4/C8/CC). Returns `(w, h)` or `None`.
    /// Used by the resize integration test to check the post-resize frame
    /// dims without pulling in a decoder dependency.
    fn jpeg_dims(bytes: &[u8]) -> Option<(u16, u16)> {
        let mut i = 2; // skip SOI (FF D8)
        while i + 9 < bytes.len() {
            if bytes[i] != 0xFF {
                i += 1;
                continue;
            }
            let marker = bytes[i + 1];
            // Standalone markers (no length): RSTn, SOI, EOI, TEM, fill.
            if marker == 0xD8 || marker == 0xD9 || (0xD0..=0xD7).contains(&marker) {
                i += 2;
                continue;
            }
            let len = ((bytes[i + 2] as usize) << 8) | bytes[i + 3] as usize;
            let is_sof = (0xC0..=0xCF).contains(&marker)
                && marker != 0xC4
                && marker != 0xC8
                && marker != 0xCC;
            if is_sof {
                let h = ((bytes[i + 5] as u16) << 8) | bytes[i + 6] as u16;
                let w = ((bytes[i + 7] as u16) << 8) | bytes[i + 8] as u16;
                return Some((w, h));
            }
            i += 2 + len;
        }
        None
    }

    /// Real-Chrome resize integration: start a screencast at the default size,
    /// confirm a frame arrives, then `resize(800, 600)` and assert frames keep
    /// flowing AND (best-effort) the decoded JPEG dims track the new viewport
    /// (≤800×600, since `maxWidth`/`maxHeight` bound the frame). Proves the
    /// `Emulation.setDeviceMetricsOverride` + re-issued `startScreencast`
    /// sequence resizes a LIVE screencast.
    ///
    /// Run manually:
    /// `cargo test -p cloudcode-agent screencast_resize_real -- --ignored --nocapture`
    #[tokio::test]
    #[ignore = "requires a real Chrome install; run manually"]
    async fn screencast_resize_real_changes_frame_dims() {
        use crate::browser::chrome::ChromeManager;
        use crate::config::BrowserConfig;
        use std::sync::Arc;
        use std::time::Duration;

        let cfg = BrowserConfig {
            enabled: true,
            chrome_path: None,
            cdp_port: 19246,
            mcp_port: 7112,
            mcp_command: None,
        };
        let tmp = tempfile::tempdir().unwrap();
        let mgr = Arc::new(ChromeManager::new(cfg, tmp.path()));
        mgr.start().await.expect("Chrome should start and become ready");
        let cdp = mgr.cdp_http_url();

        let data_url = "data:text/html,<body style=margin:0><h1 style=font-size:80px>RESIZE</h1>";
        let new_url = format!("{cdp}/json/new?{data_url}");
        let client = reqwest::Client::new();
        let opened = match client.put(&new_url).send().await {
            Ok(r) if r.status().is_success() => true,
            _ => matches!(client.get(&new_url).send().await, Ok(r) if r.status().is_success()),
        };
        assert!(opened, "failed to open a data: page target via /json/new");
        tokio::time::sleep(Duration::from_millis(800)).await;

        let (frame_tx, mut frame_rx) = mpsc::channel::<Vec<u8>>(8);
        let session = ScreencastSession::start(&cdp, None, frame_tx)
            .await
            .expect("screencast should start");

        // First frame at the default viewport.
        let frame = tokio::time::timeout(Duration::from_secs(5), frame_rx.recv())
            .await
            .expect("first frame within 5s")
            .expect("frame channel");
        assert_eq!(&frame[0..2], &[0xFF, 0xD8], "default frame must be JPEG");
        if let Some((w, h)) = jpeg_dims(&frame) {
            eprintln!("default frame dims: {w}×{h}");
        }

        // Resize to 800×600 and drain a few frames; assert frames keep
        // flowing and (once Chrome applies it) shrink to ≤800×600.
        session.resize(800, 600);

        let mut shrunk = false;
        let mut got_post_frame = false;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(6);
        while tokio::time::Instant::now() < deadline {
            match tokio::time::timeout(Duration::from_secs(2), frame_rx.recv()).await {
                Ok(Some(f)) => {
                    got_post_frame = true;
                    assert_eq!(&f[0..2], &[0xFF, 0xD8], "post-resize frame must be JPEG");
                    if let Some((w, h)) = jpeg_dims(&f) {
                        eprintln!("post-resize frame dims: {w}×{h}");
                        if w <= 800 && h <= 600 && (w >= 700 || h >= 500) {
                            shrunk = true;
                            break;
                        }
                    }
                }
                _ => break,
            }
        }
        assert!(got_post_frame, "frames must keep flowing after resize");
        eprintln!("frames kept flowing after resize; dims tracked new size: {shrunk}");

        session.stop().await;
        drop(mgr);
    }

    /// P4 Task 1 per-session page targeting — real Chrome + TWO playwright-mcp
    /// (`--isolated`, distinct contexts per the page-mapping investigation).
    ///
    /// Two "sessions" each navigate to a distinct `data:` URL with a unique
    /// marker. We learn each session's page **target id** by matching its marker
    /// in `GET /json`, record it as the session's page hint, and assert that
    /// [`pick_page_for_session`] with session A's hint selects A's page (and B's
    /// hint selects B's) — i.e. the screencast would target THE RIGHT page, not
    /// "the active page". We also drive a CDP `Page.captureScreenshot`-free
    /// check by re-reading `/json` titles to confirm the chosen ws belongs to the
    /// marker we expect. Proves the targeting logic end-to-end against the real
    /// browser; the cross-session isolation here is the `--isolated` path that
    /// actually closes the P2 leak.
    ///
    /// Run manually:
    /// `cargo test -p cloudcode-agent per_session_page_targeting -- --ignored --nocapture`
    /// Prereqs: real Chrome + `node`/`npx` on PATH (no internet; data: URLs).
    #[tokio::test]
    #[ignore = "requires real Chrome + npx playwright-mcp; run manually"]
    async fn per_session_page_targeting_picks_the_owning_sessions_page() {
        use crate::browser::chrome::ChromeManager;
        use crate::browser::mcp_endpoint::{handle_post, EndpointState, PostOutcome};
        use crate::config::BrowserConfig;
        use serde_json::Value;
        use std::sync::Arc;
        use std::time::Duration;
        use uuid::Uuid;

        // --- Real resident Chrome. ---
        let cdp_port = {
            let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            l.local_addr().unwrap().port()
        };
        // `--isolated` so each playwright-mcp owns a distinct context/target
        // (the only mode in which per-session correlation is possible — see the
        // P4 page-mapping notes). We drive the endpoint with an explicit
        // mcp_command carrying the flag.
        let cfg = BrowserConfig {
            enabled: true,
            chrome_path: None,
            cdp_port,
            mcp_port: 0,
            mcp_command: None,
        };
        let tmp = tempfile::tempdir().unwrap();
        let chrome = Arc::new(ChromeManager::new(cfg.clone(), tmp.path()));
        chrome.start().await.expect("Chrome should start");
        let cdp = chrome.cdp_http_url();

        let isolated_cmd = format!(
            "npx -y @playwright/mcp@0.0.76 --cdp-endpoint {cdp} --isolated"
        );
        let cfg_iso = BrowserConfig {
            mcp_command: Some(isolated_cmd),
            ..cfg.clone()
        };
        let state = EndpointState::new(Arc::clone(&chrome), cfg_iso);

        // --- Two sessions, A and B. ---
        let sid_a = Uuid::new_v4();
        let sid_b = Uuid::new_v4();
        state.register("tok-a".into(), sid_a);
        state.register("tok-b".into(), sid_b);

        let marker_a = "P4MARKER_AAA";
        let marker_b = "P4MARKER_BBB";

        async fn drive(state: &EndpointState, token: &str, marker: &str) {
            let init = format!(
                r#"{{"jsonrpc":"2.0","id":1,"method":"initialize","params":{{"protocolVersion":"2025-06-18","capabilities":{{}},"clientInfo":{{"name":"p4","version":"0"}}}}}}"#
            );
            assert!(matches!(
                handle_post(token, init, state).await,
                PostOutcome::Response(_)
            ));
            let _ = handle_post(
                token,
                r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#.to_string(),
                state,
            )
            .await;
            let nav = format!(
                r#"{{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{{"name":"browser_navigate","arguments":{{"url":"data:text/html,<title>{marker}</title><h1>{marker}</h1>"}}}}}}"#
            );
            match handle_post(token, nav, state).await {
                PostOutcome::Response(b) => {
                    assert!(!b.contains("\"error\""), "navigate failed: {b}")
                }
                PostOutcome::Accepted => panic!("navigate should return a response"),
            }
        }

        drive(&state, "tok-a", marker_a).await;
        drive(&state, "tok-b", marker_b).await;
        tokio::time::sleep(Duration::from_millis(800)).await;

        // --- Learn each session's page target id from /json (match by marker in
        //     the page url), and record it as the session's hint. ---
        let body = reqwest::get(format!("{cdp}/json"))
            .await
            .unwrap()
            .text()
            .await
            .unwrap();
        let arr: Vec<Value> = serde_json::from_str(&body).unwrap();
        let id_for = |marker: &str| -> String {
            arr.iter()
                .find(|t| {
                    t.get("type").and_then(Value::as_str) == Some("page")
                        && t.get("url")
                            .and_then(Value::as_str)
                            .map(|u| u.contains(marker))
                            .unwrap_or(false)
                })
                .and_then(|t| t.get("id").and_then(Value::as_str))
                .unwrap_or_else(|| panic!("no page target carrying {marker}: {body}"))
                .to_string()
        };
        let target_a = id_for(marker_a);
        let target_b = id_for(marker_b);
        assert_ne!(
            target_a, target_b,
            "--isolated must give A and B distinct page targets; got the same id ({target_a})"
        );
        state.record_page_hint(sid_a, target_a.clone());
        state.record_page_hint(sid_b, target_b.clone());

        // --- The actual assertion: picking with A's hint selects A's page ws,
        //     with B's hint selects B's — and they differ. ---
        let hint_a = state.page_hint_for(sid_a);
        let hint_b = state.page_hint_for(sid_b);
        let ws_a = pick_page_for_session(&body, hint_a.as_deref())
            .expect("A's hint should resolve to A's page ws");
        let ws_b = pick_page_for_session(&body, hint_b.as_deref())
            .expect("B's hint should resolve to B's page ws");
        assert_ne!(ws_a, ws_b, "A and B must screencast DIFFERENT pages");

        // Confirm each chosen ws belongs to the expected marker by joining back
        // through /json: the entry whose webSocketDebuggerUrl == ws_a must carry
        // marker_a in its url, and likewise for B.
        let url_of_ws = |ws: &str| -> String {
            arr.iter()
                .find(|t| t.get("webSocketDebuggerUrl").and_then(Value::as_str) == Some(ws))
                .and_then(|t| t.get("url").and_then(Value::as_str))
                .unwrap_or("")
                .to_string()
        };
        assert!(
            url_of_ws(&ws_a).contains(marker_a),
            "A's chosen ws must be A's page (url={})",
            url_of_ws(&ws_a)
        );
        assert!(
            url_of_ws(&ws_b).contains(marker_b),
            "B's chosen ws must be B's page (url={})",
            url_of_ws(&ws_b)
        );
        eprintln!(
            "per-session targeting OK: A→{} (target {target_a}), B→{} (target {target_b})",
            url_of_ws(&ws_a),
            url_of_ws(&ws_b)
        );

        state.end_session(sid_a);
        state.end_session(sid_b);
        drop(state);
        drop(chrome);
    }

    /// P6 Task 1 multi-target integration — real Chrome, two pages:
    /// the TargetWatcher sees both pages, `start_on_target` streams a JPEG
    /// from page A, switching to page B (stop A → `page_ws_url(B)` →
    /// `start_on_target`) streams a JPEG from B, and destroying B shrinks the
    /// watcher's list back to one.
    ///
    /// Run manually:
    /// `cargo test -p cloudcode-agent multi_target_watcher_and_switch -- --ignored --nocapture`
    /// Prereqs: a real Chrome/Chromium install (no internet; data: URLs).
    #[tokio::test]
    #[ignore = "requires a real Chrome install; run manually"]
    async fn multi_target_watcher_and_switch() {
        use crate::browser::chrome::ChromeManager;
        use crate::config::BrowserConfig;
        use std::sync::Arc;
        use std::time::Duration;

        let cdp_port = {
            let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            l.local_addr().unwrap().port()
        };
        let cfg = BrowserConfig {
            enabled: true,
            chrome_path: None,
            cdp_port,
            mcp_port: 0,
            mcp_command: None,
        };
        let tmp = tempfile::tempdir().unwrap();
        let mgr = Arc::new(ChromeManager::new(cfg, tmp.path()));
        mgr.start().await.expect("Chrome should start and become ready");
        let cdp = mgr.cdp_http_url();

        // Open TWO distinct data: pages via /json/new (PUT first, GET fallback
        // for older Chrome).
        let client = reqwest::Client::new();
        let open = |client: reqwest::Client, cdp: String, data_url: &'static str| async move {
            let new_url = format!("{cdp}/json/new?{data_url}");
            match client.put(&new_url).send().await {
                Ok(r) if r.status().is_success() => true,
                _ => matches!(client.get(&new_url).send().await, Ok(r) if r.status().is_success()),
            }
        };
        let url_a = "data:text/html,<title>P6A</title><h1 style=font-size:80px>P6-A</h1>";
        let url_b = "data:text/html,<title>P6B</title><h1 style=font-size:80px>P6-B</h1>";
        assert!(open(client.clone(), cdp.clone(), url_a).await, "open page A");
        assert!(open(client.clone(), cdp.clone(), url_b).await, "open page B");

        // Watch targets: wait on the change channel until both pages appear.
        let (chg_tx, mut chg_rx) = mpsc::channel::<Vec<TargetInfo>>(16);
        let watcher = TargetWatcher::start(&cdp, chg_tx)
            .await
            .expect("watcher should start");
        let both = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let list = chg_rx.recv().await.expect("change channel open");
                eprintln!(
                    "targets update: {:?}",
                    list.iter().map(|t| (&t.id, &t.url)).collect::<Vec<_>>()
                );
                let has = |m: &str| list.iter().any(|t| t.url.contains(m));
                if has("P6-A") && has("P6-B") {
                    break list;
                }
            }
        })
        .await
        .expect("watcher should report both pages within 5s");
        assert!(both.iter().all(|t| t.kind == "page"));
        let id_of = |m: &str| {
            both.iter()
                .find(|t| t.url.contains(m))
                .map(|t| t.id.clone())
                .unwrap()
        };
        let (target_a, target_b) = (id_of("P6-A"), id_of("P6-B"));
        assert_ne!(target_a, target_b);

        // Stream page A directly via start_on_target.
        let ws_a = page_ws_url(&cdp, &target_a)
            .await
            .expect("page A ws url resolves");
        let (ftx_a, mut frx_a) = mpsc::channel::<Vec<u8>>(8);
        let sess_a = ScreencastSession::start_on_target(&ws_a, ftx_a)
            .await
            .expect("screencast A starts");
        let frame = tokio::time::timeout(Duration::from_secs(5), frx_a.recv())
            .await
            .expect("frame from A within 5s")
            .expect("frame channel A");
        assert_eq!(&frame[0..2], &[0xFF, 0xD8], "A frame must be JPEG");
        eprintln!("page A JPEG: {} bytes", frame.len());

        // Switch: stop A, start B (the exact select_target flow).
        sess_a.stop().await;
        let ws_b = page_ws_url(&cdp, &target_b)
            .await
            .expect("page B ws url resolves");
        let (ftx_b, mut frx_b) = mpsc::channel::<Vec<u8>>(8);
        let sess_b = ScreencastSession::start_on_target(&ws_b, ftx_b)
            .await
            .expect("screencast B starts");
        let frame = tokio::time::timeout(Duration::from_secs(5), frx_b.recv())
            .await
            .expect("frame from B within 5s")
            .expect("frame channel B");
        assert_eq!(&frame[0..2], &[0xFF, 0xD8], "B frame must be JPEG");
        eprintln!("page B JPEG: {} bytes", frame.len());
        sess_b.stop().await;

        // A stale id resolves to None (page_ws_url contract for stale tabs).
        assert_eq!(page_ws_url(&cdp, "NOT_A_REAL_TARGET").await, None);

        // Destroy page B via /json/close and watch the list shrink.
        let close = client
            .get(format!("{cdp}/json/close/{target_b}"))
            .send()
            .await
            .expect("close request");
        assert!(close.status().is_success(), "close B: {}", close.status());
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let list = chg_rx.recv().await.expect("change channel open");
                eprintln!(
                    "targets update after close: {:?}",
                    list.iter().map(|t| (&t.id, &t.url)).collect::<Vec<_>>()
                );
                if !list.iter().any(|t| t.id == target_b) {
                    assert!(
                        list.iter().any(|t| t.id == target_a),
                        "A must survive B's destruction"
                    );
                    break;
                }
            }
        })
        .await
        .expect("watcher should drop B within 5s of close");
        // Snapshot agrees with the last pushed list.
        assert!(watcher.snapshot().iter().all(|t| t.id != target_b));

        watcher.stop();
        drop(mgr);
    }
}
