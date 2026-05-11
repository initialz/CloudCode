use crate::audit::AuditEvent;
use crate::auth;
use crate::registry::AgentConn;
use crate::session_proto::{ClientToHub, HubToClient, SESSION_PROTOCOL_VERSION};
use crate::tunnel::{ClientMsg, ServerMsg};
use crate::AppState;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::State;
use axum::response::Response;
use futures::{SinkExt, StreamExt};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{mpsc, oneshot};
use uuid::Uuid;

const HELLO_TIMEOUT: Duration = Duration::from_secs(10);
const OPEN_TIMEOUT: Duration = Duration::from_secs(20);
const WORKSPACE_REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
const FROM_AGENT_QUEUE: usize = 256;

pub async fn upgrade(State(state): State<Arc<AppState>>, ws: WebSocketUpgrade) -> Response {
    ws.on_upgrade(move |socket| handle_socket(socket, state))
}

async fn handle_socket(socket: WebSocket, state: Arc<AppState>) {
    let (mut sink, mut stream) = socket.split();

    // ---- Hello (auth) ----
    let hello = tokio::time::timeout(HELLO_TIMEOUT, stream.next()).await;
    let (token, _version) = match hello {
        Ok(Some(Ok(Message::Text(s)))) => match serde_json::from_str::<ClientToHub>(&s) {
            Ok(ClientToHub::Hello { token, version }) => (token, version),
            _ => {
                let _ = send_client(
                    &mut sink,
                    &HubToClient::Rejected {
                        reason: "expected hello".into(),
                    },
                )
                .await;
                return;
            }
        },
        _ => return,
    };
    let mut headers = axum::http::HeaderMap::new();
    headers.insert(
        axum::http::header::AUTHORIZATION,
        match axum::http::HeaderValue::from_str(&format!("Bearer {}", token)) {
            Ok(v) => v,
            Err(_) => {
                let _ = send_client(
                    &mut sink,
                    &HubToClient::Rejected {
                        reason: "bad token".into(),
                    },
                )
                .await;
                return;
            }
        },
    );
    let account_name = match auth::authenticate(&state.config.accounts, &headers) {
        Ok(a) => a.name.clone(),
        Err(reason) => {
            state.audit.write(AuditEvent {
                status: Some(401),
                reason: Some(reason.into()),
                ..AuditEvent::new("session_auth_denied")
            });
            let _ = send_client(
                &mut sink,
                &HubToClient::Rejected {
                    reason: reason.into(),
                },
            )
            .await;
            return;
        }
    };

    if send_client(
        &mut sink,
        &HubToClient::Welcome {
            account: account_name.clone(),
        },
    )
    .await
    .is_err()
    {
        return;
    }

    // ---- Wait for OpenSession ----
    let open = tokio::time::timeout(OPEN_TIMEOUT, stream.next()).await;
    let (agent_filter, workspace) = match open {
        Ok(Some(Ok(Message::Text(s)))) => match serde_json::from_str::<ClientToHub>(&s) {
            Ok(ClientToHub::OpenSession { agent, workspace }) => (agent, workspace),
            _ => {
                let _ = send_client(
                    &mut sink,
                    &HubToClient::Rejected {
                        reason: "expected open_session".into(),
                    },
                )
                .await;
                return;
            }
        },
        _ => return,
    };

    // Re-resolve account by name (auth borrowed earlier; now grab account ref freshly).
    let account = match state
        .config
        .accounts
        .iter()
        .find(|a| a.name == account_name)
    {
        Some(a) => a.clone(),
        None => {
            let _ = send_client(
                &mut sink,
                &HubToClient::Rejected {
                    reason: "account gone".into(),
                },
            )
            .await;
            return;
        }
    };

    // Pick agent.
    let candidates: Vec<&String> = match &agent_filter {
        Some(name) => {
            if !account.allowed_agents.iter().any(|a| a == name) {
                let _ = send_client(
                    &mut sink,
                    &HubToClient::Rejected {
                        reason: format!("agent '{}' not in your allowed_agents", name),
                    },
                )
                .await;
                return;
            }
            vec![name]
        }
        None => account.allowed_agents.iter().collect(),
    };
    if candidates.is_empty() {
        let _ = send_client(
            &mut sink,
            &HubToClient::Rejected {
                reason: "no allowed agents".into(),
            },
        )
        .await;
        return;
    }
    let conn = candidates.iter().find_map(|name| state.registry.get(name));
    let Some(conn) = conn else {
        let _ = send_client(
            &mut sink,
            &HubToClient::Rejected {
                reason: "no agent online".into(),
            },
        )
        .await;
        return;
    };
    let agent_name = conn.name.clone();

    // Workspace mutex (try-insert).
    if !try_claim_workspace(&state, &agent_name, &workspace, Uuid::nil()).0 {
        let _ = send_client(
            &mut sink,
            &HubToClient::Rejected {
                reason: format!(
                    "workspace '{}' is busy on agent '{}'",
                    workspace, agent_name
                ),
            },
        )
        .await;
        return;
    }
    let session_id = Uuid::new_v4();
    // Swap nil placeholder for real session id under the same key.
    state
        .workspaces
        .insert((agent_name.clone(), workspace.clone()), session_id);

    // Channel for ClientMsg forwarded by AgentConn::handle_frame for this session.
    let (from_agent_tx, mut from_agent_rx) = mpsc::channel::<ClientMsg>(FROM_AGENT_QUEUE);
    conn.register_session(session_id, from_agent_tx);

    // Tell agent to open the session.
    if conn
        .send(ServerMsg::SessionStart {
            session_id,
            workspace: workspace.clone(),
        })
        .await
        .is_err()
    {
        cleanup(&state, &conn, session_id, &agent_name, &workspace).await;
        let _ = send_client(
            &mut sink,
            &HubToClient::SessionError {
                message: "agent disconnected".into(),
            },
        )
        .await;
        return;
    }

    // Wait for SessionOpened.
    let opened = tokio::time::timeout(OPEN_TIMEOUT, from_agent_rx.recv()).await;
    let cwd = match opened {
        Ok(Some(ClientMsg::SessionOpened { cwd, .. })) => cwd,
        Ok(Some(ClientMsg::SessionError { message, .. })) => {
            cleanup(&state, &conn, session_id, &agent_name, &workspace).await;
            let _ = send_client(&mut sink, &HubToClient::Rejected { reason: message }).await;
            return;
        }
        _ => {
            cleanup(&state, &conn, session_id, &agent_name, &workspace).await;
            let _ = send_client(
                &mut sink,
                &HubToClient::Rejected {
                    reason: "session open timeout".into(),
                },
            )
            .await;
            return;
        }
    };

    state.audit.write(AuditEvent {
        account: Some(account_name.clone()),
        agent: Some(agent_name.clone()),
        session_id: Some(session_id.to_string()),
        workspace: Some(workspace.clone()),
        status: Some(200),
        ..AuditEvent::new("session_opened")
    });

    if send_client(
        &mut sink,
        &HubToClient::SessionOpened {
            agent: agent_name.clone(),
            workspace: workspace.clone(),
            cwd,
        },
    )
    .await
    .is_err()
    {
        cleanup(&state, &conn, session_id, &agent_name, &workspace).await;
        return;
    }

    // Mutable session state held by this handler.
    let mut current_workspace = workspace.clone();

    // ---- Main loop: multiplex client incoming + agent incoming ----
    loop {
        tokio::select! {
            client_msg = stream.next() => {
                let msg = match client_msg {
                    Some(Ok(m)) => m,
                    Some(Err(_)) | None => break,
                };
                match msg {
                    Message::Text(s) => {
                        let frame: ClientToHub = match serde_json::from_str(&s) {
                            Ok(f) => f,
                            Err(e) => { tracing::warn!(error = %e, "bad client frame"); continue; }
                        };
                        if !handle_client_frame(
                            &state,
                            &conn,
                            &agent_name,
                            session_id,
                            &mut current_workspace,
                            frame,
                            &mut sink,
                        ).await {
                            break;
                        }
                    }
                    Message::Close(_) => break,
                    _ => {}
                }
            }
            agent_msg = from_agent_rx.recv() => {
                let Some(frame) = agent_msg else { break; };
                if !handle_agent_frame(
                    &state,
                    &agent_name,
                    &account_name,
                    session_id,
                    &mut current_workspace,
                    frame,
                    &mut sink,
                ).await {
                    break;
                }
            }
        }
    }

    // ---- Cleanup ----
    let _ = conn.send(ServerMsg::SessionStop { session_id }).await;
    cleanup(&state, &conn, session_id, &agent_name, &current_workspace).await;
    state.audit.write(AuditEvent {
        account: Some(account_name),
        agent: Some(agent_name),
        session_id: Some(session_id.to_string()),
        workspace: Some(current_workspace),
        status: Some(200),
        ..AuditEvent::new("session_closed")
    });
    let _ = sink.close().await;
}

async fn handle_client_frame<S>(
    state: &Arc<AppState>,
    conn: &Arc<AgentConn>,
    agent_name: &str,
    session_id: Uuid,
    current_workspace: &mut String,
    frame: ClientToHub,
    sink: &mut S,
) -> bool
where
    S: SinkExt<Message, Error = axum::Error> + Unpin,
{
    match frame {
        ClientToHub::Input { content } => {
            if conn
                .send(ServerMsg::SessionInput {
                    session_id,
                    content,
                    resume: None,
                })
                .await
                .is_err()
            {
                return false;
            }
            let _ = send_client(sink, &HubToClient::TurnStarted).await;
            true
        }
        ClientToHub::Interrupt => {
            let _ = conn.send(ServerMsg::SessionInterrupt { session_id }).await;
            true
        }
        ClientToHub::SwitchWorkspace { workspace } => {
            // Acquire new mutex slot before releasing old. If acquire fails,
            // tell the client and keep the current workspace.
            if workspace == *current_workspace {
                let _ = send_client(
                    sink,
                    &HubToClient::SessionError {
                        message: "already in this workspace; use `:reset` to start fresh".into(),
                    },
                )
                .await;
                return true;
            }
            if !try_claim_workspace(state, agent_name, &workspace, session_id).0 {
                let _ = send_client(
                    sink,
                    &HubToClient::SessionError {
                        message: format!("workspace '{}' is busy", workspace),
                    },
                )
                .await;
                return true;
            }
            if conn
                .send(ServerMsg::SessionSwitchWorkspace {
                    session_id,
                    workspace: workspace.clone(),
                })
                .await
                .is_err()
            {
                // Roll back claim.
                state
                    .workspaces
                    .remove_if(&(agent_name.to_string(), workspace.clone()), |_, sid| {
                        *sid == session_id
                    });
                return false;
            }
            // Wait for SessionWorkspaceSwitched from the agent (handled in
            // handle_agent_frame, which will release the old slot and emit
            // HubToClient::WorkspaceSwitched).
            true
        }
        ClientToHub::ListWorkspaces => {
            let request_id = Uuid::new_v4();
            let (tx, rx) = oneshot::channel();
            conn.register_workspace_request(request_id, tx);
            if conn
                .send(ServerMsg::WorkspaceList { request_id })
                .await
                .is_err()
            {
                return false;
            }
            let resp = tokio::time::timeout(WORKSPACE_REQUEST_TIMEOUT, rx).await;
            match resp {
                Ok(Ok(ClientMsg::WorkspaceListResult { items, error, .. })) => {
                    if let Some(e) = error {
                        let _ = send_client(sink, &HubToClient::SessionError { message: e }).await;
                    } else {
                        let _ = send_client(sink, &HubToClient::WorkspaceList { items }).await;
                    }
                }
                _ => {
                    let _ = send_client(
                        sink,
                        &HubToClient::SessionError {
                            message: "workspace list timed out".into(),
                        },
                    )
                    .await;
                }
            }
            true
        }
        ClientToHub::CreateWorkspace { name } => {
            let request_id = Uuid::new_v4();
            let (tx, rx) = oneshot::channel();
            conn.register_workspace_request(request_id, tx);
            if conn
                .send(ServerMsg::WorkspaceCreate {
                    request_id,
                    name: name.clone(),
                })
                .await
                .is_err()
            {
                return false;
            }
            match tokio::time::timeout(WORKSPACE_REQUEST_TIMEOUT, rx).await {
                Ok(Ok(ClientMsg::WorkspaceCreateResult { error, .. })) => {
                    if let Some(e) = error {
                        let _ = send_client(sink, &HubToClient::SessionError { message: e }).await;
                    } else {
                        let _ = send_client(sink, &HubToClient::WorkspaceCreated { name }).await;
                    }
                }
                _ => {
                    let _ = send_client(
                        sink,
                        &HubToClient::SessionError {
                            message: "workspace create timed out".into(),
                        },
                    )
                    .await;
                }
            }
            true
        }
        ClientToHub::DeleteWorkspace { name } => {
            // Hub-side guard: refuse if currently held.
            if state
                .workspaces
                .contains_key(&(agent_name.to_string(), name.clone()))
            {
                let _ = send_client(
                    sink,
                    &HubToClient::SessionError {
                        message: format!("workspace '{}' is currently in use", name),
                    },
                )
                .await;
                return true;
            }
            let request_id = Uuid::new_v4();
            let (tx, rx) = oneshot::channel();
            conn.register_workspace_request(request_id, tx);
            if conn
                .send(ServerMsg::WorkspaceDelete {
                    request_id,
                    name: name.clone(),
                })
                .await
                .is_err()
            {
                return false;
            }
            match tokio::time::timeout(WORKSPACE_REQUEST_TIMEOUT, rx).await {
                Ok(Ok(ClientMsg::WorkspaceDeleteResult { error, .. })) => {
                    if let Some(e) = error {
                        let _ = send_client(sink, &HubToClient::SessionError { message: e }).await;
                    } else {
                        let _ = send_client(sink, &HubToClient::WorkspaceDeleted { name }).await;
                    }
                }
                _ => {
                    let _ = send_client(
                        sink,
                        &HubToClient::SessionError {
                            message: "workspace delete timed out".into(),
                        },
                    )
                    .await;
                }
            }
            true
        }
        ClientToHub::Close => false,
        ClientToHub::Hello { .. } | ClientToHub::OpenSession { .. } | ClientToHub::Pong => true,
    }
}

async fn handle_agent_frame<S>(
    state: &Arc<AppState>,
    agent_name: &str,
    account_name: &str,
    session_id: Uuid,
    current_workspace: &mut String,
    frame: ClientMsg,
    sink: &mut S,
) -> bool
where
    S: SinkExt<Message, Error = axum::Error> + Unpin,
{
    match frame {
        ClientMsg::SessionTurnStarted { .. } => true,
        ClientMsg::SessionEvent { event, .. } => {
            send_client(sink, &HubToClient::ClaudeEvent { event })
                .await
                .is_ok()
        }
        ClientMsg::SessionTurnEnded {
            exit_code, error, ..
        } => {
            state.audit.write(AuditEvent {
                account: Some(account_name.to_string()),
                agent: Some(agent_name.to_string()),
                session_id: Some(session_id.to_string()),
                workspace: Some(current_workspace.clone()),
                exit_code: Some(exit_code),
                status: Some(if exit_code == 0 { 200 } else { 500 }),
                reason: error.clone(),
                ..AuditEvent::new("turn_ended")
            });
            send_client(sink, &HubToClient::TurnEnded { exit_code, error })
                .await
                .is_ok()
        }
        ClientMsg::SessionWorkspaceSwitched { workspace, cwd, .. } => {
            // Release the old workspace mutex slot (the new one was claimed
            // in handle_client_frame before we forwarded the switch).
            let old = std::mem::replace(current_workspace, workspace.clone());
            if old != workspace {
                state
                    .workspaces
                    .remove_if(&(agent_name.to_string(), old.clone()), |_, sid| {
                        *sid == session_id
                    });
            }
            state.audit.write(AuditEvent {
                account: Some(account_name.to_string()),
                agent: Some(agent_name.to_string()),
                session_id: Some(session_id.to_string()),
                workspace: Some(workspace.clone()),
                status: Some(200),
                ..AuditEvent::new("workspace_switched")
            });
            send_client(sink, &HubToClient::WorkspaceSwitched { workspace, cwd })
                .await
                .is_ok()
        }
        ClientMsg::SessionError { message, .. } => {
            send_client(sink, &HubToClient::SessionError { message })
                .await
                .is_ok()
        }
        ClientMsg::SessionClosed { reason, .. } => {
            let _ = send_client(sink, &HubToClient::SessionClosed { reason }).await;
            false
        }
        ClientMsg::SessionOpened { .. } => true, // already consumed earlier
        // Workspace results / hello / pong don't route here.
        _ => true,
    }
}

async fn cleanup(
    state: &Arc<AppState>,
    conn: &Arc<AgentConn>,
    session_id: Uuid,
    agent_name: &str,
    workspace: &str,
) {
    conn.unregister_session(session_id);
    state.workspaces.remove_if(
        &(agent_name.to_string(), workspace.to_string()),
        |_, sid| *sid == session_id,
    );
}

/// Try to claim `(agent, workspace)` for `session_id`. Returns (claimed, prior_holder).
fn try_claim_workspace(
    state: &Arc<AppState>,
    agent_name: &str,
    workspace: &str,
    session_id: Uuid,
) -> (bool, Option<Uuid>) {
    let key = (agent_name.to_string(), workspace.to_string());
    match state.workspaces.entry(key) {
        dashmap::mapref::entry::Entry::Occupied(o) => (false, Some(*o.get())),
        dashmap::mapref::entry::Entry::Vacant(v) => {
            v.insert(session_id);
            (true, None)
        }
    }
}

async fn send_client<S>(sink: &mut S, msg: &HubToClient) -> Result<(), ()>
where
    S: SinkExt<Message, Error = axum::Error> + Unpin,
{
    let text = serde_json::to_string(msg).map_err(|_| ())?;
    sink.send(Message::Text(text)).await.map_err(|_| ())
}

/// Suppress unused warnings for the protocol version constant (re-exported via
/// session_proto).
#[allow(dead_code)]
const _PROTOCOL_VERSION_REF: &str = SESSION_PROTOCOL_VERSION;
