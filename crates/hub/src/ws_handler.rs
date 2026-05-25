use crate::registry::OutgoingFrame;
use crate::tunnel::{ClientMsg, RejectReason, ServerMsg, PROTOCOL_VERSION};
use crate::AppState;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::State;
use axum::response::Response;
use futures::{SinkExt, StreamExt};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;

const HELLO_TIMEOUT: Duration = Duration::from_secs(10);
/// How often the hub probes each agent connection with a PING frame.
/// Kept short (5s) so the agent's read loop, which trips a reconnect
/// after ~10s of silence, surfaces a dead hub (e.g. mid-upgrade,
/// crashed, or stale TCP after a network blip) within ~10s instead
/// of waiting on the OS-level keepalive timer. The opposite tradeoff
/// is bandwidth — but a ping is a few bytes per agent every 5s, so
/// even hundreds of agents are < 1 KB/s.
const PING_INTERVAL: Duration = Duration::from_secs(5);
/// Maximum gap between frames from the agent before the hub declares
/// the connection dead and unregisters. Agents Pong every Ping (so we
/// expect a frame every ~5s) and otherwise stream PTY output. 10s
/// gives a 2x margin against jitter. Without this the OS-level TCP
/// keepalive (default ~2h on macOS) is the only timeout, so a silent
/// TCP drop pins the AgentConn in the registry for hours; every
/// reconnect attempt by the same agent then gets NameTaken and the
/// supervisor pumps the agent process in a loop until TCP finally
/// gives up.
const READ_IDLE_TIMEOUT: Duration = Duration::from_secs(10);
const SEND_QUEUE: usize = 256;

pub async fn upgrade(State(state): State<Arc<AppState>>, ws: WebSocketUpgrade) -> Response {
    ws.on_upgrade(move |socket| handle_socket(socket, state))
}

async fn handle_socket(socket: WebSocket, state: Arc<AppState>) {
    let (mut sink, mut stream) = socket.split();

    let hello = match tokio::time::timeout(HELLO_TIMEOUT, stream.next()).await {
        Ok(Some(Ok(Message::Text(s)))) => match serde_json::from_str::<ClientMsg>(&s) {
            Ok(ClientMsg::Hello {
                name,
                secret,
                version,
                agent_version,
                target_triple,
                workspaces,
            }) => (name, secret, version, agent_version, target_triple, workspaces),
            _ => {
                let _ = send_rejected(&mut sink, RejectReason::AuthFailed).await;
                return;
            }
        },
        _ => return,
    };

    let (name, secret, version, agent_version, target_triple, agent_workspaces) = hello;

    let agent_proto: u32 = version.parse().unwrap_or(0);
    let hub_proto: u32 = PROTOCOL_VERSION.parse().unwrap_or(0);
    if agent_proto > hub_proto {
        let _ = send_rejected(&mut sink, RejectReason::VersionMismatch).await;
        return;
    }

    if !crate::auth::verify_token(&secret, &state.config.agents.registration_token_hash) {
        let _ = send_rejected(&mut sink, RejectReason::AuthFailed).await;
        return;
    }

    let (tx, mut rx) = mpsc::channel::<OutgoingFrame>(SEND_QUEUE);
    let Some(conn) = state
        .registry
        .try_register(name.clone(), agent_version, target_triple, tx)
    else {
        let _ = send_rejected(&mut sink, RejectReason::NameTaken).await;
        return;
    };

    if send_text(&mut sink, &ServerMsg::Welcome { name: name.clone() })
        .await
        .is_err()
    {
        state.registry.unregister(&conn);
        return;
    }

    tracing::info!(agent = %name, "agent connected");

    // One-time migration: seed hub's `workspaces` table with anything
    // this agent already has on local disk. Each entry comes in as
    // "<account>/<name>"; we INSERT OR IGNORE so a name already
    // owned by another agent (or by a previous run of this agent)
    // is left alone. First-come-first-served if two agents happen
    // to report the same `(account, name)`.
    let mut seeded = 0usize;
    for slash in &agent_workspaces {
        let Some((account, ws_name)) = slash.split_once('/') else {
            continue;
        };
        if account.is_empty() || ws_name.is_empty() {
            continue;
        }
        match state
            .db
            .upsert_workspace_binding(account, &name, ws_name)
            .await
        {
            Ok(true) => seeded += 1,
            Ok(false) => {}
            Err(e) => tracing::warn!(
                agent = %name,
                account = %account,
                workspace = %ws_name,
                error = %e,
                "could not seed workspace binding"
            ),
        }
    }
    if seeded > 0 {
        tracing::info!(
            agent = %name,
            count = seeded,
            "seeded workspace bindings from agent Hello"
        );
    }

    let writer = tokio::spawn(async move {
        let mut ping = tokio::time::interval(PING_INTERVAL);
        ping.tick().await;
        loop {
            tokio::select! {
                msg = rx.recv() => {
                    let Some(out) = msg else { break; };
                    let r = match out {
                        OutgoingFrame::Text(m) => match serde_json::to_string(&m) {
                            Ok(t) => sink.send(Message::Text(t)).await,
                            Err(e) => {
                                tracing::warn!(error = %e, "encode hub→agent text");
                                continue;
                            }
                        },
                        OutgoingFrame::Binary(b) => sink.send(Message::Binary(b)).await,
                    };
                    if r.is_err() {
                        break;
                    }
                }
                _ = ping.tick() => {
                    if send_text(&mut sink, &ServerMsg::Ping).await.is_err() {
                        break;
                    }
                }
            }
        }
        let _ = sink.close().await;
    });

    loop {
        let next = match tokio::time::timeout(READ_IDLE_TIMEOUT, stream.next()).await {
            Ok(opt) => opt,
            Err(_) => {
                // Silent TCP drop: agent stopped pong-ing but the OS
                // hasn't reported the connection closed yet. Tear it
                // down so the agent's reconnect attempt can claim the
                // registry slot.
                tracing::info!(
                    agent = %name,
                    "no frame from agent for {}s; declaring dead",
                    READ_IDLE_TIMEOUT.as_secs()
                );
                break;
            }
        };
        let Some(item) = next else { break };
        let msg = match item {
            Ok(m) => m,
            Err(e) => {
                tracing::debug!(agent = %name, error = %e, "ws read error");
                break;
            }
        };
        match msg {
            Message::Text(s) => match serde_json::from_str::<ClientMsg>(&s) {
                Ok(ClientMsg::Pong) => {}
                Ok(ClientMsg::Message {
                    session_id,
                    claude_session_id,
                    ts,
                    kind,
                    body,
                }) => {
                    let db = state.db.clone();
                    let row = crate::db::MessageRow {
                        cc_session_id: session_id.to_string(),
                        claude_session_id,
                        ts,
                        kind,
                        body,
                    };
                    tokio::spawn(async move { db.insert_message(&row).await });
                }
                Ok(ClientMsg::UserInteraction {
                    account,
                    workspace,
                    claude_session_id,
                    prompt_id,
                    parent_uuid,
                    cwd,
                    git_branch,
                    ts_ms,
                    kind,
                    content,
                }) => {
                    let db = state.db.clone();
                    let agent_name = name.clone();
                    tokio::spawn(async move {
                        db.insert_user_interaction(&crate::db::UserInteractionRow {
                            account,
                            agent: agent_name,
                            workspace,
                            claude_session_id,
                            prompt_id,
                            parent_uuid,
                            cwd: Some(cwd),
                            git_branch,
                            ts_ms,
                            kind,
                            content,
                        })
                        .await;
                    });
                }
                Ok(frame) => conn.handle_text_frame(frame),
                Err(e) => tracing::warn!(agent = %name, error = %e, "bad frame"),
            },
            Message::Binary(b) => conn.handle_binary_frame(&b),
            Message::Close(_) => break,
            Message::Ping(_) | Message::Pong(_) => {}
        }
    }

    tracing::info!(agent = %name, "agent disconnected");
    state.registry.unregister(&conn);
    writer.abort();
}

async fn send_text<S>(sink: &mut S, msg: &ServerMsg) -> Result<(), ()>
where
    S: SinkExt<Message, Error = axum::Error> + Unpin,
{
    let text = serde_json::to_string(msg).map_err(|_| ())?;
    sink.send(Message::Text(text)).await.map_err(|_| ())
}

async fn send_rejected<S>(sink: &mut S, reason: RejectReason) -> Result<(), ()>
where
    S: SinkExt<Message, Error = axum::Error> + Unpin,
{
    let _ = send_text(sink, &ServerMsg::Rejected { reason }).await;
    Ok(())
}
