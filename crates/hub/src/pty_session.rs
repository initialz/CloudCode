use crate::audit::AuditEvent;
use crate::auth;
use crate::pty_proto::{AgentInfo, ClientToHub, HubToClient};
use crate::registry::{AgentConn, PtyEventOut};
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
const PTY_EVENT_QUEUE: usize = 1024;

pub async fn upgrade(State(state): State<Arc<AppState>>, ws: WebSocketUpgrade) -> Response {
    ws.on_upgrade(move |socket| handle_socket(socket, state))
}

async fn handle_socket(socket: WebSocket, state: Arc<AppState>) {
    let (mut sink, mut stream) = socket.split();

    // ---- Hello (auth) ----
    let hello = tokio::time::timeout(HELLO_TIMEOUT, stream.next()).await;
    let token = match hello {
        Ok(Some(Ok(Message::Text(s)))) => match serde_json::from_str::<ClientToHub>(&s) {
            Ok(ClientToHub::Hello { token, .. }) => token,
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
    if let Ok(v) = axum::http::HeaderValue::from_str(&format!("Bearer {}", token)) {
        headers.insert(axum::http::header::AUTHORIZATION, v);
    } else {
        let _ = send_client(
            &mut sink,
            &HubToClient::Rejected {
                reason: "bad token".into(),
            },
        )
        .await;
        return;
    }
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

    // ---- OpenSession ----
    let open = tokio::time::timeout(OPEN_TIMEOUT, stream.next()).await;
    let (agent_filter, workspace, cols, rows) = match open {
        Ok(Some(Ok(Message::Text(s)))) => match serde_json::from_str::<ClientToHub>(&s) {
            Ok(ClientToHub::OpenSession {
                agent,
                workspace,
                cols,
                rows,
            }) => (agent, workspace, cols, rows),
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

    // Pick agent.
    let conn = match &agent_filter {
        Some(name) => match state.registry.get(name) {
            Some(c) => c,
            None => {
                let _ = send_client(
                    &mut sink,
                    &HubToClient::Rejected {
                        reason: format!("agent '{}' is not online", name),
                    },
                )
                .await;
                return;
            }
        },
        None => {
            let mut active = state.registry.list_active();
            if active.is_empty() {
                let _ = send_client(
                    &mut sink,
                    &HubToClient::Rejected {
                        reason: "no agent online".into(),
                    },
                )
                .await;
                return;
            }
            active.sort();
            state.registry.get(&active[0]).unwrap()
        }
    };
    let agent_name = conn.name.clone();

    // Workspace mutex (per-account namespace).
    let session_id = Uuid::new_v4();
    if !claim_workspace(&state, &agent_name, &account_name, &workspace, session_id) {
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

    let (evt_tx, mut evt_rx) = mpsc::channel::<PtyEventOut>(PTY_EVENT_QUEUE);
    conn.register_session(session_id, evt_tx);

    if conn
        .send(ServerMsg::PtyOpen {
            session_id,
            account: account_name.clone(),
            workspace: workspace.clone(),
            cols,
            rows,
        })
        .await
        .is_err()
    {
        cleanup(
            &state,
            &conn,
            session_id,
            &agent_name,
            &account_name,
            &workspace,
        )
        .await;
        let _ = send_client(
            &mut sink,
            &HubToClient::SessionError {
                message: "agent disconnected".into(),
            },
        )
        .await;
        return;
    }

    let cwd = match tokio::time::timeout(OPEN_TIMEOUT, evt_rx.recv()).await {
        Ok(Some(PtyEventOut::Frame(ClientMsg::PtyOpened { cwd, .. }))) => cwd,
        Ok(Some(PtyEventOut::Frame(ClientMsg::PtyError { message, .. }))) => {
            cleanup(
                &state,
                &conn,
                session_id,
                &agent_name,
                &account_name,
                &workspace,
            )
            .await;
            let _ = send_client(&mut sink, &HubToClient::Rejected { reason: message }).await;
            return;
        }
        _ => {
            cleanup(
                &state,
                &conn,
                session_id,
                &agent_name,
                &account_name,
                &workspace,
            )
            .await;
            let _ = send_client(
                &mut sink,
                &HubToClient::Rejected {
                    reason: "pty open timeout".into(),
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
        cleanup(
            &state,
            &conn,
            session_id,
            &agent_name,
            &account_name,
            &workspace,
        )
        .await;
        return;
    }

    let mut current_workspace = workspace.clone();
    let mut last_cols = cols;
    let mut last_rows = rows;

    // ---- Main multiplex loop ----
    loop {
        tokio::select! {
            client_msg = stream.next() => {
                let msg = match client_msg {
                    Some(Ok(m)) => m,
                    _ => break,
                };
                match msg {
                    Message::Text(s) => {
                        let frame: ClientToHub = match serde_json::from_str(&s) {
                            Ok(f) => f,
                            Err(e) => { tracing::warn!(error = %e, "bad client frame"); continue; }
                        };
                        if !handle_client_frame(
                            &state, &conn, &agent_name, &account_name, session_id,
                            &mut current_workspace, &mut last_cols, &mut last_rows,
                            frame, &mut sink,
                        ).await {
                            break;
                        }
                    }
                    Message::Binary(b) => {
                        // Forward as PTY input to the agent (tag 0x01 + session_id + payload).
                        let _ = conn.send_pty_input(session_id, &b).await;
                    }
                    Message::Close(_) => break,
                    _ => {}
                }
            }
            evt = evt_rx.recv() => {
                let Some(evt) = evt else { break; };
                if !handle_agent_event(
                    &state, &agent_name, &account_name, session_id,
                    &mut current_workspace, evt, &mut sink,
                ).await {
                    break;
                }
            }
        }
    }

    let _ = conn.send(ServerMsg::PtyClose { session_id }).await;
    cleanup(
        &state,
        &conn,
        session_id,
        &agent_name,
        &account_name,
        &current_workspace,
    )
    .await;
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

#[allow(clippy::too_many_arguments)]
async fn handle_client_frame<S>(
    state: &Arc<AppState>,
    conn: &Arc<AgentConn>,
    agent_name: &str,
    account_name: &str,
    session_id: Uuid,
    current_workspace: &mut String,
    last_cols: &mut u16,
    last_rows: &mut u16,
    frame: ClientToHub,
    sink: &mut S,
) -> bool
where
    S: SinkExt<Message, Error = axum::Error> + Unpin,
{
    match frame {
        ClientToHub::Resize { cols, rows } => {
            *last_cols = cols;
            *last_rows = rows;
            let _ = conn
                .send(ServerMsg::PtyResize {
                    session_id,
                    cols,
                    rows,
                })
                .await;
            true
        }
        ClientToHub::SwitchWorkspace { workspace } => {
            if workspace == *current_workspace {
                let _ = send_client(
                    sink,
                    &HubToClient::SessionError {
                        message: "already in this workspace".into(),
                    },
                )
                .await;
                return true;
            }
            if !claim_workspace(state, agent_name, account_name, &workspace, session_id) {
                let _ = send_client(
                    sink,
                    &HubToClient::SessionError {
                        message: format!("workspace '{}' is busy", workspace),
                    },
                )
                .await;
                return true;
            }
            // Reusing the same session_id signals "swap workspace in place".
            if conn
                .send(ServerMsg::PtyOpen {
                    session_id,
                    account: account_name.to_string(),
                    workspace: workspace.clone(),
                    cols: *last_cols,
                    rows: *last_rows,
                })
                .await
                .is_err()
            {
                state.workspaces.remove_if(
                    &(
                        agent_name.to_string(),
                        account_name.to_string(),
                        workspace.clone(),
                    ),
                    |_, sid| *sid == session_id,
                );
                return false;
            }
            let prev = std::mem::replace(current_workspace, workspace);
            state.workspaces.remove_if(
                &(agent_name.to_string(), account_name.to_string(), prev),
                |_, sid| *sid == session_id,
            );
            state.audit.write(AuditEvent {
                account: Some(account_name.to_string()),
                agent: Some(agent_name.to_string()),
                session_id: Some(session_id.to_string()),
                workspace: Some(current_workspace.clone()),
                status: Some(200),
                ..AuditEvent::new("workspace_switched")
            });
            true
        }
        ClientToHub::ListAgents => {
            let names = state.registry.list_active();
            let items: Vec<AgentInfo> = names
                .into_iter()
                .map(|n| AgentInfo {
                    current: n == agent_name,
                    name: n,
                })
                .collect();
            let _ = send_client(sink, &HubToClient::AgentList { items }).await;
            true
        }
        ClientToHub::ListWorkspaces => {
            let request_id = Uuid::new_v4();
            let (tx, rx) = oneshot::channel();
            conn.register_workspace_request(request_id, tx);
            if conn
                .send(ServerMsg::WorkspaceList {
                    request_id,
                    account: account_name.to_string(),
                })
                .await
                .is_err()
            {
                return false;
            }
            match tokio::time::timeout(WORKSPACE_REQUEST_TIMEOUT, rx).await {
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
                    account: account_name.to_string(),
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
            if state.workspaces.contains_key(&(
                agent_name.to_string(),
                account_name.to_string(),
                name.clone(),
            )) {
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
                    account: account_name.to_string(),
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

async fn handle_agent_event<S>(
    _state: &Arc<AppState>,
    _agent_name: &str,
    _account_name: &str,
    _session_id: Uuid,
    current_workspace: &mut String,
    evt: PtyEventOut,
    sink: &mut S,
) -> bool
where
    S: SinkExt<Message, Error = axum::Error> + Unpin,
{
    match evt {
        PtyEventOut::Output(bytes) => sink.send(Message::Binary(bytes.to_vec())).await.is_ok(),
        PtyEventOut::Frame(ClientMsg::PtyOpened { workspace, cwd, .. }) => {
            // Workspace-switch confirmation comes back here too (after the
            // initial open). Report it as WorkspaceSwitched if the workspace
            // has changed; the initial open was already reported earlier.
            if workspace != *current_workspace {
                // Mismatch — agent confirmed a different workspace than what
                // we recorded. Sync up just in case.
                *current_workspace = workspace.clone();
            }
            send_client(sink, &HubToClient::WorkspaceSwitched { workspace, cwd })
                .await
                .is_ok()
        }
        PtyEventOut::Frame(ClientMsg::PtyClosed { reason, .. }) => {
            let _ = send_client(sink, &HubToClient::SessionClosed { reason }).await;
            false
        }
        PtyEventOut::Frame(ClientMsg::PtyError { message, .. }) => {
            send_client(sink, &HubToClient::SessionError { message })
                .await
                .is_ok()
        }
        PtyEventOut::Frame(_) => true,
    }
}

async fn cleanup(
    state: &Arc<AppState>,
    conn: &Arc<AgentConn>,
    session_id: Uuid,
    agent_name: &str,
    account_name: &str,
    workspace: &str,
) {
    conn.unregister_session(session_id);
    state.workspaces.remove_if(
        &(
            agent_name.to_string(),
            account_name.to_string(),
            workspace.to_string(),
        ),
        |_, sid| *sid == session_id,
    );
}

fn claim_workspace(
    state: &Arc<AppState>,
    agent_name: &str,
    account_name: &str,
    workspace: &str,
    session_id: Uuid,
) -> bool {
    let key = (
        agent_name.to_string(),
        account_name.to_string(),
        workspace.to_string(),
    );
    match state.workspaces.entry(key) {
        dashmap::mapref::entry::Entry::Occupied(_) => false,
        dashmap::mapref::entry::Entry::Vacant(v) => {
            v.insert(session_id);
            true
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
