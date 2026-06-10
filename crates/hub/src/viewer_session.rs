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
//!     `TAG_SCREENCAST_FRAME` channel.
//!   - **up (viewer → agent)**: text `Message::Text(json)` — each frame is a
//!     `ViewerInputEvent` in its `{"kind":...}` serde form (see `parse_viewer_input`),
//!     relayed to the agent as `ServerMsg::ViewerInput`.
//!
//! Lifecycle: on connect we mint a `viewer_session_id`, register a frame
//! channel on the owning `AgentConn`, and send `ServerMsg::ViewerAttach`. On
//! disconnect (ws close, agent frame channel closed, or agent gone) we
//! unregister and send `ServerMsg::ViewerDetach`.

use crate::app::{self, USER_SESSION_COOKIE};
use crate::registry::AgentConn;
use crate::tunnel::{ServerMsg, ViewerInputEvent};
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

/// `GET /v1/viewer/ws?session=<id>` — resolve cookie auth *before* the upgrade
/// (the request headers are gone once the socket is open), exactly as
/// `pty_session::upgrade` does. Unlike the PTY endpoint there is no CLI / Hello
/// token path: the viewer page is browser-only, so a valid `cc_user_session`
/// cookie is the sole credential. No cookie / unknown session → reject the
/// upgrade with `401` (never open the socket for an unauthenticated viewer).
pub async fn upgrade(
    State(state): State<Arc<AppState>>,
    Query(q): Query<ViewerQuery>,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> Response {
    let account = match app::parse_cookie(&headers, USER_SESSION_COOKIE) {
        Some(sid) => state.user_auth.lookup(&sid).await,
        None => None,
    };
    let Some(account) = account else {
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
    let (frame_tx, mut frame_rx) = tokio::sync::mpsc::channel::<bytes::Bytes>(VIEWER_FRAME_QUEUE);
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

/// The core bridge: agent frames → browser binary, browser text → agent input.
/// Returns when either side closes. Generic over the sink/stream halves so the
/// pure routing shape stays obvious (and so a future test could drive it with
/// in-memory channels — the live ws path is P4/P5 manual smoke).
async fn relay_loop<Si, St>(
    sink: &mut Si,
    stream: &mut St,
    frame_rx: &mut tokio::sync::mpsc::Receiver<bytes::Bytes>,
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
                    Some(jpeg) => {
                        if sink.send(Message::Binary(jpeg.to_vec())).await.is_err() {
                            break;
                        }
                    }
                    // Channel closed: agent dropped the screencast (page
                    // closed / agent disconnected). End the relay.
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
                        if let Some(event) = parse_viewer_input(&text) {
                            // Fire-and-forget; a dead agent ends the loop on
                            // the next frame poll anyway.
                            if agent_conn
                                .send(ServerMsg::ViewerInput {
                                    viewer_session_id,
                                    event,
                                })
                                .await
                                .is_err()
                            {
                                break;
                            }
                        } else {
                            tracing::debug!(viewer = %viewer_session_id, "unparseable viewer input frame; ignoring");
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
