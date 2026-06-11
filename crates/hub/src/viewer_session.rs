//! Viewer ↔ hub WS endpoint at `/v1/viewer/ws?session=<session_id>`.
//!
//! A browser viewer page (served at `/viewer`, see `app::viewer`) opens this
//! socket to watch a running PTY session's headless-Chrome screencast and to
//! push human mouse/keyboard/IME input back into that browser. The structure
//! mirrors `pty_session.rs` — cookie auth resolved before the upgrade, then a
//! `select!` loop bridging the agent's frame channel and the browser ws — but
//! is much simpler: there is no menu phase, no PTY, and no workspace ops.
//!
//! Wire shapes:
//!   - **down (agent → viewer)**: binary `Message::Binary(jpeg)` — one JPEG
//!     per screencast frame, forwarded verbatim from the agent's
//!     `TAG_SCREENCAST_FRAME` channel — plus text `Message::Text(json)` of
//!     the shape `{"kind":"targets","targets":[…]}` whenever the agent's
//!     target list changes (see [`targets_wire_json`]; P6 multi-target).
//!   - **up (viewer → agent)**: text `Message::Text(json)` — each frame is
//!     either a `ViewerInputEvent` in its `{"kind":...}` serde form, relayed
//!     as `ServerMsg::ViewerInput`, or `{"kind":"select_target",
//!     "target_id":…}`, relayed as `ServerMsg::ViewerSelectTarget` (see
//!     [`parse_viewer_uplink`]).
//!
//! Lifecycle: on connect we mint a `viewer_session_id`, register a
//! [`ViewerOut`] channel on the owning `AgentConn`, and send
//! `ServerMsg::ViewerAttach`. On disconnect (ws close, agent channel closed,
//! `ViewerOut::Closed`, or agent gone) we unregister and send
//! `ServerMsg::ViewerDetach`.

use crate::app::{self, USER_SESSION_COOKIE};
use crate::auth;
use crate::registry::{AgentConn, ViewerOut};
use crate::tunnel::{ServerMsg, TargetInfo, ViewerInputEvent};
use crate::AppState;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Query, State};
use axum::http::HeaderMap;
use axum::response::{IntoResponse, Response};
use futures::{SinkExt, StreamExt};
use serde::Deserialize;
use std::sync::Arc;
use uuid::Uuid;

/// Buffer for the per-viewer JPEG frame channel. Screencast frames arrive at
/// ~30fps; a slow browser consumer drops frames (the agent's `try_send` in
/// `handle_binary_frame` just warns and moves on) rather than stalling the
/// agent read loop. A modest buffer absorbs short hiccups.
const VIEWER_FRAME_QUEUE: usize = 64;

#[derive(Debug, Deserialize)]
pub struct ViewerQuery {
    /// The PTY session_id whose browser the viewer wants to watch.
    pub session: Uuid,
    /// Optional account token, used by the native desktop app (which has
    /// no browser cookie). When present it is validated EXACTLY as the
    /// PTY ws Hello token is — `auth::authenticate` over a synthesized
    /// `Authorization: Bearer <token>` header — and the resolved account
    /// is subject to the same ownership guard as the cookie account. The
    /// browser viewer page never sends this; it relies on the cookie.
    #[serde(default)]
    pub token: Option<String>,
}

/// Resolve the account name for a viewer connection, preferring the
/// browser session cookie and falling back to a query-param token.
///
/// Both paths reuse the hub's existing, audited credential checks:
///   - **cookie** → `state.user_auth.lookup` (same as the web verify
///     page and `pty_session::upgrade`'s cookie pre-auth).
///   - **token**  → `auth::authenticate` over a `Bearer <token>` header,
///     the IDENTICAL validation `pty_session::authenticate` runs for the
///     CLI client's Hello token. No weaker check is introduced.
///
/// Returns the authenticated account name, or `None` if neither
/// credential resolves to a live account. The account-ownership guard in
/// `handle_socket` still applies on top of this — authentication here
/// only proves *who* the viewer is, not *what* they may watch.
async fn resolve_viewer_account(
    state: &AppState,
    headers: &HeaderMap,
    token: Option<&str>,
) -> Option<String> {
    // Cookie first: this is the browser viewer page's sole credential and
    // matches the existing P2 behavior exactly.
    if let Some(sid) = app::parse_cookie(headers, USER_SESSION_COOKIE) {
        if let Some(account) = state.user_auth.lookup(&sid).await {
            return Some(account);
        }
    }
    // Token fallback (native desktop app). Validate it the same way
    // `pty_session::authenticate` does: synthesize the `Authorization`
    // header `auth::authenticate` expects and reuse that function verbatim.
    if let Some(token) = token {
        let mut hdrs = HeaderMap::new();
        if let Ok(v) = axum::http::HeaderValue::from_str(&format!("Bearer {}", token)) {
            hdrs.insert(axum::http::header::AUTHORIZATION, v);
            if let Ok(account) = auth::authenticate(&state.db, &hdrs).await {
                return Some(account.name);
            }
        }
    }
    None
}

/// Parse a browser-sent viewer-input JSON line into a `ViewerInputEvent`.
///
/// The browser sends exactly the `ViewerInputEvent` serde form (flat
/// `#[serde(tag = "kind")]` shape, e.g. `{"kind":"mouse_move","x":10,"y":20}`),
/// so this is just a tolerant `serde_json::from_str`: any malformed / unknown
/// frame yields `None` and the relay loop skips it instead of tearing down the
/// connection. Factored out so the wire contract is unit-testable without a
/// live socket.
pub fn parse_viewer_input(text: &str) -> Option<ViewerInputEvent> {
    serde_json::from_str::<ViewerInputEvent>(text).ok()
}

/// One parsed uplink (viewer → hub) text frame.
///
/// The uplink shares one `{"kind":…}` tag space: every `ViewerInputEvent`
/// kind passes through untouched as [`ViewerUplink::Input`], and the P6
/// multi-target addition `{"kind":"select_target","target_id":"…"}` becomes
/// [`ViewerUplink::SelectTarget`] (relayed as `ServerMsg::ViewerSelectTarget`).
#[derive(Debug, PartialEq)]
pub enum ViewerUplink {
    Input(ViewerInputEvent),
    SelectTarget { target_id: String },
}

/// The non-input uplink kinds, parsed with the same serde conventions as
/// `ViewerInputEvent` (flat `kind` tag, snake_case).
#[derive(Debug, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum ControlUplink {
    SelectTarget { target_id: String },
}

/// Parse any uplink text frame. Input kinds keep their exact pre-P6
/// semantics (`parse_viewer_input` passthrough); unknown/malformed frames
/// yield `None` and are skipped by the relay loop.
pub fn parse_viewer_uplink(text: &str) -> Option<ViewerUplink> {
    if let Some(ev) = parse_viewer_input(text) {
        return Some(ViewerUplink::Input(ev));
    }
    match serde_json::from_str::<ControlUplink>(text).ok()? {
        ControlUplink::SelectTarget { target_id } => Some(ViewerUplink::SelectTarget { target_id }),
    }
}

/// Serialize a targets list into the downlink Text-frame shape the viewer
/// app consumes: `{"kind":"targets","targets":[…]}`. Wrapped in a `kind`
/// envelope so future downlink text messages can multiplex on the same
/// socket. This is THE wire contract for the app's tab bar (the app mirrors
/// `TargetInfo` from this JSON, same pattern as `ViewerInputEvent`).
pub fn targets_wire_json(targets: &[TargetInfo]) -> String {
    serde_json::to_string(&serde_json::json!({
        "kind": "targets",
        "targets": targets,
    }))
    .unwrap_or_else(|_| r#"{"kind":"targets","targets":[]}"#.to_string())
}

/// Find the agent that currently owns `session_id`, and the account that
/// session belongs to.
///
/// Mirrors how `pty_session` keys live sessions: `state.workspaces` is the
/// global `(agent, account, workspace) -> session_id` map (set at OpenSession,
/// cleared on close). We reverse-scan it for the entry whose value matches the
/// requested session, returning `(agent_name, account_name)`. `None` means the
/// session isn't live (never opened, already closed, or the agent dropped it).
fn resolve_session_owner(state: &AppState, session_id: Uuid) -> Option<(String, String)> {
    state.workspaces.iter().find_map(|e| {
        if *e.value() == session_id {
            let (agent, account, _workspace) = e.key();
            Some((agent.clone(), account.clone()))
        } else {
            None
        }
    })
}

/// `GET /v1/viewer/ws?session=<id>[&token=<tok>]` — resolve auth *before*
/// the upgrade (the request headers are gone once the socket is open),
/// exactly as `pty_session::upgrade` does. Two credentials are accepted:
///   - a `cc_user_session` cookie (the browser viewer page), and
///   - a `?token=` query param (the native desktop app, which has no
///     cookie) — validated with the SAME `auth::authenticate` the PTY ws
///     uses for the CLI client's Hello token.
/// Cookie takes precedence when both are present. No valid credential →
/// reject the upgrade with `401` (never open the socket for an
/// unauthenticated viewer). The account-ownership guard in `handle_socket`
/// applies regardless of which credential authenticated the viewer.
pub async fn upgrade(
    State(state): State<Arc<AppState>>,
    Query(q): Query<ViewerQuery>,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> Response {
    let Some(account) = resolve_viewer_account(&state, &headers, q.token.as_deref()).await else {
        return (axum::http::StatusCode::UNAUTHORIZED, "login required").into_response();
    };
    ws.on_upgrade(move |socket| handle_socket(socket, state, account, q.session))
}

async fn handle_socket(
    socket: WebSocket,
    state: Arc<AppState>,
    account: String,
    session_id: Uuid,
) {
    let (mut sink, mut stream) = socket.split();

    // Resolve which agent owns this session, and the account that owns it.
    let Some((agent_name, owner_account)) = resolve_session_owner(&state, session_id) else {
        let _ = sink
            .send(Message::Close(Some(axum::extract::ws::CloseFrame {
                code: axum::extract::ws::close_code::NORMAL,
                reason: "session not found".into(),
            })))
            .await;
        return;
    };

    // Authorization: the cookie account must own the session it's asking to
    // watch. Without this any logged-in account could spy on any other
    // account's browser by guessing a session_id.
    if owner_account != account {
        let _ = sink
            .send(Message::Close(Some(axum::extract::ws::CloseFrame {
                code: axum::extract::ws::close_code::POLICY,
                reason: "not your session".into(),
            })))
            .await;
        return;
    }

    let Some(agent_conn) = state.registry.get(&agent_name) else {
        let _ = sink
            .send(Message::Close(Some(axum::extract::ws::CloseFrame {
                code: axum::extract::ws::close_code::NORMAL,
                reason: "agent offline".into(),
            })))
            .await;
        return;
    };

    let viewer_session_id = Uuid::new_v4();
    let (frame_tx, mut frame_rx) = tokio::sync::mpsc::channel::<ViewerOut>(VIEWER_FRAME_QUEUE);
    agent_conn.register_viewer(viewer_session_id, frame_tx);

    // Tell the agent to start screencasting this session's browser to us.
    if agent_conn
        .send(ServerMsg::ViewerAttach {
            viewer_session_id,
            session_id,
        })
        .await
        .is_err()
    {
        agent_conn.unregister_viewer(viewer_session_id);
        let _ = sink
            .send(Message::Close(Some(axum::extract::ws::CloseFrame {
                code: axum::extract::ws::close_code::NORMAL,
                reason: "agent disconnected".into(),
            })))
            .await;
        return;
    }

    tracing::info!(
        viewer = %viewer_session_id,
        session = %session_id,
        agent = %agent_name,
        "viewer attached"
    );

    relay_loop(&mut sink, &mut stream, &mut frame_rx, &agent_conn, viewer_session_id).await;

    // Teardown — unregister the frame channel and tell the agent to stop the
    // screencast. Both are best-effort: if the agent already vanished its
    // viewer map is gone too, and the Detach send just fails harmlessly.
    agent_conn.unregister_viewer(viewer_session_id);
    let _ = agent_conn
        .send(ServerMsg::ViewerDetach { viewer_session_id })
        .await;
    tracing::info!(viewer = %viewer_session_id, "viewer detached");
    let _ = sink.close().await;
}

/// The core bridge: agent [`ViewerOut`]s → browser ws (Binary frames / Text
/// targets), browser text → agent input or target selection. Returns when
/// either side closes. Generic over the sink/stream halves so the pure
/// routing shape stays obvious (and so a future test could drive it with
/// in-memory channels — the live ws path is P4/P5 manual smoke).
async fn relay_loop<Si, St>(
    sink: &mut Si,
    stream: &mut St,
    frame_rx: &mut tokio::sync::mpsc::Receiver<ViewerOut>,
    agent_conn: &Arc<AgentConn>,
    viewer_session_id: Uuid,
) where
    Si: SinkExt<Message, Error = axum::Error> + Unpin,
    St: futures::Stream<Item = Result<Message, axum::Error>> + Unpin,
{
    loop {
        tokio::select! {
            frame = frame_rx.recv() => {
                match frame {
                    // A JPEG from the agent → straight down to the browser.
                    Some(ViewerOut::Frame(jpeg)) => {
                        if sink.send(Message::Binary(jpeg.to_vec())).await.is_err() {
                            break;
                        }
                    }
                    // Targets list (pre-serialized JSON) → Text frame.
                    Some(ViewerOut::Targets(json)) => {
                        if sink.send(Message::Text(json)).await.is_err() {
                            break;
                        }
                    }
                    // Agent ended this viewer's screencast. End the relay.
                    Some(ViewerOut::Closed(reason)) => {
                        tracing::debug!(viewer = %viewer_session_id, reason = ?reason, "agent closed viewer");
                        break;
                    }
                    // Channel closed (agent disconnected). End the relay.
                    None => break,
                }
            }
            incoming = stream.next() => {
                let msg = match incoming {
                    Some(Ok(m)) => m,
                    _ => break,
                };
                match msg {
                    Message::Text(text) => {
                        let out = match parse_viewer_uplink(&text) {
                            Some(ViewerUplink::Input(event)) => ServerMsg::ViewerInput {
                                viewer_session_id,
                                event,
                            },
                            Some(ViewerUplink::SelectTarget { target_id }) => {
                                ServerMsg::ViewerSelectTarget {
                                    viewer_session_id,
                                    target_id,
                                }
                            }
                            None => {
                                tracing::debug!(viewer = %viewer_session_id, "unparseable viewer uplink frame; ignoring");
                                continue;
                            }
                        };
                        // Fire-and-forget; a dead agent ends the loop on
                        // the next frame poll anyway.
                        if agent_conn.send(out).await.is_err() {
                            break;
                        }
                    }
                    Message::Close(_) => break,
                    // Viewers don't send binary; pings/pongs handled by axum.
                    _ => {}
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_mouse_move() {
        let ev = parse_viewer_input(r#"{"kind":"mouse_move","x":10.5,"y":20.0}"#).unwrap();
        assert_eq!(ev, ViewerInputEvent::MouseMove { x: 10.5, y: 20.0 });
    }

    #[test]
    fn parse_mouse_button() {
        let ev = parse_viewer_input(
            r#"{"kind":"mouse_button","x":1.0,"y":2.0,"button":"left","down":true,"click_count":1}"#,
        )
        .unwrap();
        assert_eq!(
            ev,
            ViewerInputEvent::MouseButton {
                x: 1.0,
                y: 2.0,
                button: "left".into(),
                down: true,
                click_count: 1,
            }
        );
    }

    #[test]
    fn parse_wheel() {
        let ev =
            parse_viewer_input(r#"{"kind":"wheel","x":3.0,"y":4.0,"dx":-1.0,"dy":2.5}"#).unwrap();
        assert_eq!(
            ev,
            ViewerInputEvent::Wheel {
                x: 3.0,
                y: 4.0,
                dx: -1.0,
                dy: 2.5,
            }
        );
    }

    #[test]
    fn parse_key() {
        let ev = parse_viewer_input(
            r#"{"kind":"key","key":"a","code":"KeyA","text":"a","down":true,"modifiers":2}"#,
        )
        .unwrap();
        assert_eq!(
            ev,
            ViewerInputEvent::Key {
                key: "a".into(),
                code: "KeyA".into(),
                text: "a".into(),
                down: true,
                modifiers: 2,
            }
        );
    }

    #[test]
    fn parse_insert_text() {
        let ev = parse_viewer_input(r#"{"kind":"insert_text","text":"你好"}"#).unwrap();
        assert_eq!(ev, ViewerInputEvent::InsertText { text: "你好".into() });
    }

    #[test]
    fn garbage_is_none() {
        assert!(parse_viewer_input("not json").is_none());
        assert!(parse_viewer_input("{}").is_none());
        assert!(parse_viewer_input(r#"{"kind":"nope"}"#).is_none());
        // Right kind, missing required field.
        assert!(parse_viewer_input(r#"{"kind":"mouse_move","x":1.0}"#).is_none());
        // Empty.
        assert!(parse_viewer_input("").is_none());
    }

    // --- parse_viewer_uplink (P6 multi-target) -----------------------------

    #[test]
    fn uplink_passes_input_kinds_through() {
        let got = parse_viewer_uplink(r#"{"kind":"mouse_move","x":10.5,"y":20.0}"#).unwrap();
        assert_eq!(
            got,
            ViewerUplink::Input(ViewerInputEvent::MouseMove { x: 10.5, y: 20.0 })
        );
        let got = parse_viewer_uplink(r#"{"kind":"insert_text","text":"你好"}"#).unwrap();
        assert_eq!(
            got,
            ViewerUplink::Input(ViewerInputEvent::InsertText { text: "你好".into() })
        );
    }

    #[test]
    fn uplink_parses_select_target() {
        let got = parse_viewer_uplink(r#"{"kind":"select_target","target_id":"ABC123"}"#).unwrap();
        assert_eq!(
            got,
            ViewerUplink::SelectTarget {
                target_id: "ABC123".into()
            }
        );
    }

    #[test]
    fn uplink_garbage_is_none() {
        assert!(parse_viewer_uplink("not json").is_none());
        assert!(parse_viewer_uplink("{}").is_none());
        assert!(parse_viewer_uplink(r#"{"kind":"nope"}"#).is_none());
        // select_target missing its required field.
        assert!(parse_viewer_uplink(r#"{"kind":"select_target"}"#).is_none());
        assert!(parse_viewer_uplink("").is_none());
    }

    // --- targets_wire_json: the app-facing downlink Text shape -------------

    #[test]
    fn targets_wire_shape_is_pinned() {
        let targets = vec![
            TargetInfo {
                id: "T_A".into(),
                title: "百度一下".into(),
                url: "https://www.baidu.com/".into(),
                kind: "page".into(),
            },
            TargetInfo {
                id: "T_B".into(),
                title: "".into(),
                url: "about:blank".into(),
                kind: "page".into(),
            },
        ];
        let json = targets_wire_json(&targets);
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["kind"], "targets");
        let arr = v["targets"].as_array().unwrap();
        assert_eq!(arr.len(), 2);
        assert_eq!(arr[0]["id"], "T_A");
        assert_eq!(arr[0]["title"], "百度一下");
        assert_eq!(arr[0]["url"], "https://www.baidu.com/");
        assert_eq!(arr[0]["kind"], "page");
        assert_eq!(arr[1]["id"], "T_B");

        // Empty list (browser idle) keeps the envelope.
        let v: serde_json::Value = serde_json::from_str(&targets_wire_json(&[])).unwrap();
        assert_eq!(v["kind"], "targets");
        assert_eq!(v["targets"].as_array().unwrap().len(), 0);
    }

    /// The token branch of `resolve_viewer_account` is a thin wrapper over
    /// `auth::authenticate(&db, Bearer <token>)` — the IDENTICAL primitive
    /// `pty_session::authenticate` uses for the CLI Hello token. This test
    /// pins that the reused validation maps a token to its owning account
    /// (and ONLY that account), which is the security-load-bearing bit: it
    /// is what the downstream ownership guard (`owner_account != account`)
    /// then compares against. A token for account A must resolve to A, so a
    /// token for A can never satisfy the guard for B's session.
    #[tokio::test]
    async fn token_resolves_to_its_owning_account_only() {
        use crate::auth;
        use crate::db::Db;

        // Temp-file sqlite (the hub crate has no in-memory test harness;
        // a unique temp path keeps this self-contained and parallel-safe).
        let dir = std::env::temp_dir();
        let path = dir.join(format!("cc-viewer-auth-{}.sqlite", Uuid::new_v4()));
        let db = Db::open(&path).await.expect("open temp db");

        let token_a = auth::generate_token();
        let token_b = auth::generate_token();
        let hash_a = auth::hash_token(&token_a).unwrap();
        let hash_b = auth::hash_token(&token_b).unwrap();
        db.insert_account("alice", &hash_a, None, None).await.unwrap();
        db.insert_account("bob", &hash_b, None, None).await.unwrap();

        let resolve = |tok: &str| {
            let mut hdrs = HeaderMap::new();
            hdrs.insert(
                axum::http::header::AUTHORIZATION,
                axum::http::HeaderValue::from_str(&format!("Bearer {tok}")).unwrap(),
            );
            hdrs
        };

        // alice's token → alice (never bob).
        let acct = auth::authenticate(&db, &resolve(&token_a)).await.unwrap();
        assert_eq!(acct.name, "alice");
        // bob's token → bob.
        let acct = auth::authenticate(&db, &resolve(&token_b)).await.unwrap();
        assert_eq!(acct.name, "bob");
        // A garbage token authenticates as nobody.
        assert!(auth::authenticate(&db, &resolve("cc_not_a_real_token"))
            .await
            .is_err());

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn resolve_owner_reverse_scan() {
        // `resolve_session_owner` reverse-scans the `(agent, account,
        // workspace) -> session_id` map. We exercise that exact logic against
        // a standalone DashMap so we don't have to stand up a full AppState
        // (Db / audit / config) just to test the scan.
        let workspaces: dashmap::DashMap<(String, String, String), Uuid> = dashmap::DashMap::new();
        let sid = Uuid::new_v4();
        workspaces.insert(("agentX".into(), "alice".into(), "proj".into()), sid);
        workspaces.insert(("agentY".into(), "bob".into(), "other".into()), Uuid::new_v4());

        let scan = |target: Uuid| -> Option<(String, String)> {
            workspaces.iter().find_map(|e| {
                if *e.value() == target {
                    let (agent, account, _ws) = e.key();
                    Some((agent.clone(), account.clone()))
                } else {
                    None
                }
            })
        };

        assert_eq!(scan(sid), Some(("agentX".into(), "alice".into())));
        assert!(scan(Uuid::new_v4()).is_none());
    }
}
