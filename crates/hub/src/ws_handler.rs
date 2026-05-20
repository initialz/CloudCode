use crate::registry::OutgoingFrame;
use crate::tunnel::{ClientMsg, RejectReason, ServerMsg, PROTOCOL_VERSION};
use crate::AppState;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::State;
use axum::response::Response;
use futures::{SinkExt, StreamExt};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;

const HELLO_TIMEOUT: Duration = Duration::from_secs(10);
const PING_INTERVAL: Duration = Duration::from_secs(30);
const SEND_QUEUE: usize = 256;
/// Minimum time between two `update_workspace_sync_meta` calls for
/// the same `(account, workspace)`. A busy editor save burst will
/// fire dozens of WorkspacePushFile frames in quick succession; we
/// don't want to size-scan + UPDATE the DB on every one. The
/// staleness this introduces (≤5s) only affects the admin UI's
/// "last sync" column, not any correctness path.
const SYNC_META_DEBOUNCE: Duration = Duration::from_secs(5);

pub async fn upgrade(State(state): State<Arc<AppState>>, ws: WebSocketUpgrade) -> Response {
    // Bump the WS frame / message ceilings. Tungstenite's default
    // `max_frame_size` is 16 MiB; once a single `WorkspacePushFile`
    // serializes the file content via `serde_bytes` into JSON (each
    // byte → 1-3 chars + comma) we can easily blow past that on
    // multi-MB JSON / data files and the kernel hands us a RST
    // instead of a proper close. 64 MiB gives plenty of headroom
    // without making it trivial for a misbehaving agent to OOM us.
    let ws = ws
        .max_frame_size(256 * 1024 * 1024)
        .max_message_size(256 * 1024 * 1024);
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
                tools,
            }) => (name, secret, version, agent_version, target_triple, tools),
            _ => {
                let _ = send_rejected(&mut sink, RejectReason::AuthFailed).await;
                return;
            }
        },
        _ => return,
    };

    let (name, secret, version, agent_version, target_triple, tools) = hello;

    if version != PROTOCOL_VERSION {
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
        .try_register(name.clone(), agent_version, target_triple, tools, tx)
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

    // v1.13: drain any pending "your lock was force-taken" instructions
    // for this agent. If there are any, push a WorkspaceCleanup frame
    // *before* any other ServerMsg so the agent can rm -rf its stale
    // copies before the next OpenSession races. We deliberately drain
    // (not just peek) — if the WS dies before the agent acts on the
    // frame the worst case is the agent re-uploads a stale copy on the
    // next sync, which the hub overwrites on the next pull. We re-queue
    // only when the hub itself force-takes a new lock.
    match state.db.take_pending_cleanups(&name).await {
        Ok(items) if !items.is_empty() => {
            tracing::info!(
                agent = %name,
                count = items.len(),
                "draining pending workspace cleanups"
            );
            if send_text(&mut sink, &ServerMsg::WorkspaceCleanup { items })
                .await
                .is_err()
            {
                state.registry.unregister(&conn);
                return;
            }
        }
        Ok(_) => {}
        Err(e) => tracing::warn!(agent = %name, error = %e, "take_pending_cleanups failed"),
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

    // Per-(account, workspace) "last meta refresh" timestamps for the
    // sync-meta debounce. Lives for the lifetime of the agent
    // connection — cleared when this handler returns.
    let mut last_meta_refresh: std::collections::HashMap<(String, String), Instant> =
        std::collections::HashMap::new();

    while let Some(item) = stream.next().await {
        let msg = match item {
            Ok(m) => m,
            Err(e) => {
                tracing::debug!(agent = %name, error = %e, "ws read error");
                break;
            }
        };
        match msg {
            Message::Text(s) => match serde_json::from_str::<ClientMsg>(&s) {
                Ok(ClientMsg::Message {
                    session_id,
                    claude_session_id,
                    ts,
                    kind,
                    body,
                }) => {
                    // Conversation event from the agent's jsonl tail.
                    // Persisted straight to the admin db; no client
                    // forwarding needed.
                    state
                        .db
                        .insert_message(&crate::db::MessageRow {
                            cc_session_id: session_id.to_string(),
                            claude_session_id,
                            ts,
                            kind,
                            body,
                        })
                        .await;
                }
                Ok(ClientMsg::WorkspacePushFile {
                    session_id,
                    path,
                    content,
                }) => {
                    handle_workspace_push(
                        &state,
                        &conn,
                        session_id,
                        path,
                        content,
                        &mut last_meta_refresh,
                    )
                    .await;
                }
                Ok(ClientMsg::WorkspaceDeleteFile { session_id, path }) => {
                    handle_workspace_delete(
                        &state,
                        &conn,
                        session_id,
                        path,
                        &mut last_meta_refresh,
                    )
                    .await;
                }
                Ok(frame) => conn.handle_text_frame(frame).await,
                Err(e) => tracing::warn!(agent = %name, error = %e, "bad frame"),
            },
            Message::Binary(b) => conn.handle_binary_frame(&b).await,
            Message::Close(_) => break,
            Message::Ping(_) | Message::Pong(_) => {}
        }
    }

    tracing::info!(agent = %name, "agent disconnected");
    // Release any workspace locks this agent still held so they don't
    // stay "owned" by a dead connection. The next OpenSession for the
    // workspace will see it as free and take it without needing
    // `force=true`.
    match state.db.release_all_workspace_locks_for_agent(&name).await {
        Ok(n) if n > 0 => {
            tracing::info!(agent = %name, n, "released workspace locks on disconnect");
        }
        Ok(_) => {}
        Err(e) => tracing::warn!(agent = %name, error = %e, "lock release on disconnect failed"),
    }
    state.registry.unregister(&conn);
    writer.abort();
}

/// Hub-side handler for `ClientMsg::WorkspacePushFile`. Looks the
/// session up in the reverse index, writes the file into the canonical
/// store, and acks the agent. Failures ack with `ok=false`; the agent
/// keeps the row in its push queue and retries.
async fn handle_workspace_push(
    state: &Arc<AppState>,
    conn: &Arc<crate::registry::AgentConn>,
    session_id: uuid::Uuid,
    path: String,
    content: Vec<u8>,
    last_meta_refresh: &mut std::collections::HashMap<(String, String), Instant>,
) {
    let Some((account, workspace, _agent)) = state
        .session_workspaces
        .get(&session_id)
        .map(|e| e.value().clone())
    else {
        tracing::warn!(
            session = %session_id,
            path = %path,
            "WorkspacePushFile for unknown session; acking with error"
        );
        let _ = conn
            .send(ServerMsg::WorkspaceFileAck {
                session_id,
                path,
                ok: false,
                error: Some("unknown session".into()),
            })
            .await;
        return;
    };

    // Push the sync filesystem write off the axum runtime — under a
    // 5000-file burst (git clone of an angr-sized repo) this used to
    // block enough worker threads that the WS writer task couldn't
    // drain its outbound buffer, no acks came back, the agent
    // accumulated 50 in-flight pending paths and stalled, OS-level
    // TCP buffers filled both directions and tungstenite eventually
    // RST'd.
    let storage = state.workspaces.clone();
    let acct = account.clone();
    let ws = workspace.clone();
    let p = path.clone();
    let (ok, error) = match tokio::task::spawn_blocking(move || {
        storage.write_file(&acct, &ws, &p, &content)
    })
    .await
    {
        Ok(Ok(())) => (true, None),
        Ok(Err(e)) => (false, Some(e.to_string())),
        Err(e) => (false, Some(format!("join blocking write: {e}"))),
    };
    if ok {
        maybe_refresh_sync_meta(state, &account, &workspace, last_meta_refresh).await;
    }
    let _ = conn
        .send(ServerMsg::WorkspaceFileAck {
            session_id,
            path,
            ok,
            error,
        })
        .await;
}

/// Mirror of `handle_workspace_push` for the delete path.
async fn handle_workspace_delete(
    state: &Arc<AppState>,
    conn: &Arc<crate::registry::AgentConn>,
    session_id: uuid::Uuid,
    path: String,
    last_meta_refresh: &mut std::collections::HashMap<(String, String), Instant>,
) {
    let Some((account, workspace, _agent)) = state
        .session_workspaces
        .get(&session_id)
        .map(|e| e.value().clone())
    else {
        tracing::warn!(
            session = %session_id,
            path = %path,
            "WorkspaceDeleteFile for unknown session; acking with error"
        );
        let _ = conn
            .send(ServerMsg::WorkspaceFileAck {
                session_id,
                path,
                ok: false,
                error: Some("unknown session".into()),
            })
            .await;
        return;
    };

    let storage = state.workspaces.clone();
    let acct = account.clone();
    let ws = workspace.clone();
    let p = path.clone();
    let (ok, error) = match tokio::task::spawn_blocking(move || {
        storage.delete_file(&acct, &ws, &p)
    })
    .await
    {
        Ok(Ok(())) => (true, None),
        Ok(Err(e)) => (false, Some(e.to_string())),
        Err(e) => (false, Some(format!("join blocking delete: {e}"))),
    };
    if ok {
        maybe_refresh_sync_meta(state, &account, &workspace, last_meta_refresh).await;
    }
    let _ = conn
        .send(ServerMsg::WorkspaceFileAck {
            session_id,
            path,
            ok,
            error,
        })
        .await;
}

/// Throttled `last_sync_at` + `size_bytes` refresh. Each (account,
/// workspace) is updated at most once per `SYNC_META_DEBOUNCE`, so a
/// 200-file editor save burst hits the DB once instead of 200 times.
async fn maybe_refresh_sync_meta(
    state: &Arc<AppState>,
    account: &str,
    workspace: &str,
    last_meta_refresh: &mut std::collections::HashMap<(String, String), Instant>,
) {
    let key = (account.to_string(), workspace.to_string());
    let now = Instant::now();
    let due = last_meta_refresh
        .get(&key)
        .map(|prev| now.duration_since(*prev) >= SYNC_META_DEBOUNCE)
        .unwrap_or(true);
    if !due {
        return;
    }
    last_meta_refresh.insert(key, now);
    let size = match state.workspaces.total_size(account, workspace) {
        Ok(s) => s as i64,
        Err(e) => {
            tracing::debug!(error = %e, "total_size failed; skipping sync meta refresh");
            return;
        }
    };
    if let Err(e) = state
        .db
        .update_workspace_sync_meta(account, workspace, size)
        .await
    {
        tracing::debug!(error = %e, "update_workspace_sync_meta failed");
    }
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
