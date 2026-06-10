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

use crate::tunnel::ViewerInputEvent;
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
    let arr = serde_json::from_str::<Value>(targets_json).ok()?;
    let arr = arr.as_array()?;

    let is_page = |t: &Value| t.get("type").and_then(Value::as_str) == Some("page");
    let ws_of = |t: &Value| {
        t.get("webSocketDebuggerUrl")
            .and_then(Value::as_str)
            .map(str::to_string)
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
                return ws_of(t);
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
        if let Some(ws) = ws_of(t) {
            return Some(ws);
        }
    }

    // Unhinted fallback — Pass 2: the first page of any kind that has a ws url.
    for t in arr {
        if is_page(t) {
            if let Some(ws) = ws_of(t) {
                return Some(ws);
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

fn cmd_start_screencast(id: i64) -> String {
    compact(json!({
        "id": id,
        "method": "Page.startScreencast",
        "params": {
            "format": "jpeg",
            "quality": 60,
            "maxWidth": 1280,
            "maxHeight": 800,
            "everyNthFrame": 1,
        }
    }))
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

impl ScreencastSession {
    /// Connect to the page target behind `cdp_http_url`, start a JPEG
    /// screencast, and stream decoded frames to `frame_tx`.
    ///
    /// `target_hint` is the per-session page target id (when known); see
    /// [`pick_page_for_session`]. `None` falls back to the active page.
    ///
    /// Steps:
    ///   1. `GET <cdp_http_url>/json` → [`pick_page_for_session`] → ws url (bail
    ///      if none);
    ///   2. `connect_async` the page debugger ws;
    ///   3. send `Page.enable` + `Page.startScreencast`;
    ///   4. spawn a task that selects between the ws read stream (decode frames
    ///      → `frame_tx`, ack each) and a command receiver (forward queued
    ///      outgoing commands to the ws).
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

        let (ws, _) = tokio_tungstenite::connect_async(&ws_url)
            .await
            .map_err(|e| anyhow!("connecting CDP ws {ws_url}: {e}"))?;
        let (mut sink, mut stream) = ws.split();

        let next_id = Arc::new(AtomicI64::new(1));
        let mint = |id: &AtomicI64| id.fetch_add(1, Ordering::Relaxed);

        // Bring the page up and start the screencast.
        sink.send(Message::Text(cmd_page_enable(mint(&next_id))))
            .await
            .map_err(|e| anyhow!("sending Page.enable: {e}"))?;
        sink.send(Message::Text(cmd_start_screencast(mint(&next_id))))
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

    /// Stop the screencast and tear the session down: enqueue
    /// `Page.stopScreencast` (best-effort), then abort the ws task so the
    /// websocket closes.
    pub async fn stop(self) {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let _ = self.cmd_tx.try_send(cmd_stop_screencast(id));
        // Give the task a brief moment to flush the stop command before abort.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        self.task.abort();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- pick_page_for_session: unhinted fallback (active page) -----------
    // These lock the legacy `pick_page_target` behaviour, now reached with
    // `target_hint = None` (the agent's current default-config path).

    #[test]
    fn pick_prefers_real_page_over_blank() {
        let json = r#"[
            {"type":"background_page","url":"chrome-extension://x","webSocketDebuggerUrl":"ws://bg"},
            {"type":"page","url":"about:blank","webSocketDebuggerUrl":"ws://blank"},
            {"type":"page","url":"https://example.com/","webSocketDebuggerUrl":"ws://real"}
        ]"#;
        assert_eq!(
            pick_page_for_session(json, None),
            Some("ws://real".to_string())
        );
    }

    #[test]
    fn pick_skips_chrome_scheme_pages() {
        let json = r#"[
            {"type":"page","url":"chrome://newtab/","webSocketDebuggerUrl":"ws://newtab"},
            {"type":"page","url":"data:text/html,<h1>hi</h1>","webSocketDebuggerUrl":"ws://data"}
        ]"#;
        assert_eq!(
            pick_page_for_session(json, None),
            Some("ws://data".to_string())
        );
    }

    #[test]
    fn pick_falls_back_to_first_page_when_all_blank() {
        let json = r#"[
            {"type":"webview","url":"x","webSocketDebuggerUrl":"ws://wv"},
            {"type":"page","url":"about:blank","webSocketDebuggerUrl":"ws://first"},
            {"type":"page","url":"chrome://gpu","webSocketDebuggerUrl":"ws://second"}
        ]"#;
        assert_eq!(
            pick_page_for_session(json, None),
            Some("ws://first".to_string())
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
            {"type":"page","url":"https://a.com/"},
            {"type":"page","url":"https://b.com/","webSocketDebuggerUrl":"ws://b"}
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
}
