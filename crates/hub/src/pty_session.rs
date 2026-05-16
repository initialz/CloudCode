//! Client ↔ hub WS endpoint at /v1/pty/ws.
//!
//! Each connection is two phases interleaved over one WebSocket:
//!   - **Menu phase**  — the client uses SelectAgent/ListAgents/
//!     ListWorkspaces/CreateWorkspace/DeleteWorkspace to browse, then
//!     issues OpenSession to enter
//!   - **PTY phase**   — bytes flow through to a tmux+claude session on the
//!     selected agent until the PTY closes (claude exits, agent disconnects,
//!     etc), at which point we drop back to the menu phase.
//!
//! Only an explicit ClientToHub::Close (or WS close) ends the whole
//! connection.

use crate::app::{self, USER_SESSION_COOKIE};
use crate::audit::AuditEvent;
use crate::auth;
use crate::pty_proto::{AgentInfo, ClientToHub, HubToClient};
use crate::registry::{AgentConn, PtyEventOut};
use crate::tunnel::{ClientMsg, ServerMsg};
use crate::AppState;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::State;
use axum::http::HeaderMap;
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

pub async fn upgrade(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> Response {
    // Resolve cookie auth *before* the WS upgrade; once the socket is
    // open we no longer have access to the original request headers.
    // `None` means "fall back to in-protocol Hello token auth", which
    // is what the CLI client uses.
    let pre_auth = app::parse_cookie(&headers, USER_SESSION_COOKIE)
        .and_then(|sid| state.user_auth.lookup(&sid));
    ws.on_upgrade(move |socket| handle_socket(socket, state, pre_auth))
}

async fn handle_socket(socket: WebSocket, state: Arc<AppState>, pre_auth: Option<String>) {
    let (mut sink, mut stream) = socket.split();

    // ---- Hello (auth) ----
    let account_name = match authenticate(&state, &mut sink, &mut stream, pre_auth).await {
        Some(a) => a,
        None => return,
    };

    let mut ctx = ConnCtx {
        state: state.clone(),
        account_name,
        selected_agent: None,
        active: None,
    };

    // Single big loop — menu phase + (optionally) PTY phase.
    loop {
        let agent_evt_recv = async {
            if let Some(active) = ctx.active.as_mut() {
                active.evt_rx.recv().await
            } else {
                std::future::pending::<Option<PtyEventOut>>().await
            }
        };

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
                        if !handle_client_frame(&mut ctx, frame, &mut sink).await {
                            break;
                        }
                    }
                    Message::Binary(b) => {
                        // Only meaningful if a PTY session is active.
                        if let (Some(conn), Some(active)) = (ctx.selected_agent.as_ref(), ctx.active.as_ref()) {
                            let _ = conn.send_pty_input(active.session_id, &b).await;
                        }
                    }
                    Message::Close(_) => break,
                    _ => {}
                }
            }
            evt = agent_evt_recv => {
                let Some(evt) = evt else { continue; };
                if !handle_agent_event(&mut ctx, evt, &mut sink).await {
                    break;
                }
            }
        }
    }

    ctx.teardown_active().await;
    state.audit.write(AuditEvent {
        account: Some(ctx.account_name.clone()),
        agent: ctx.selected_agent.as_ref().map(|c| c.name.clone()),
        status: Some(200),
        ..AuditEvent::new("connection_closed")
    });
    let _ = sink.close().await;
}

struct ConnCtx {
    state: Arc<AppState>,
    account_name: String,
    selected_agent: Option<Arc<AgentConn>>,
    active: Option<ActiveSession>,
}

struct ActiveSession {
    session_id: Uuid,
    workspace: String,
    cols: u16,
    rows: u16,
    evt_rx: mpsc::Receiver<PtyEventOut>,
}

impl ConnCtx {
    async fn teardown_active(&mut self) {
        if let (Some(conn), Some(active)) = (self.selected_agent.as_ref(), self.active.take()) {
            let _ = conn
                .send(ServerMsg::PtyClose {
                    session_id: active.session_id,
                })
                .await;
            conn.unregister_session(active.session_id);
            self.state.workspaces.remove_if(
                &(
                    conn.name.clone(),
                    self.account_name.clone(),
                    active.workspace.clone(),
                ),
                |_, sid| *sid == active.session_id,
            );
            // Mark the row in `sessions` as ended. Without this the
            // admin UI would keep showing the session as "live" even
            // after the client has gone, because the agent's reply
            // PtyClosed event never gets routed back here (we already
            // unregistered the channel above).
            let db = self.state.db.clone();
            let sid = active.session_id.to_string();
            tokio::spawn(async move {
                db.end_session(&sid, Some("client disconnect")).await;
            });
            self.state.audit.write(AuditEvent {
                account: Some(self.account_name.clone()),
                agent: Some(conn.name.clone()),
                session_id: Some(active.session_id.to_string()),
                workspace: Some(active.workspace),
                status: Some(200),
                reason: Some("client disconnect".into()),
                ..AuditEvent::new("session_closed")
            });
        }
    }
}

async fn authenticate<S, R>(
    state: &Arc<AppState>,
    sink: &mut S,
    stream: &mut R,
    pre_auth: Option<String>,
) -> Option<String>
where
    S: SinkExt<Message, Error = axum::Error> + Unpin,
    R: futures::Stream<Item = Result<Message, axum::Error>> + Unpin,
{
    // Still expect a Hello frame even when the cookie pre-authed the
    // connection — the protocol shape is shared with the CLI client
    // and the frame's `version` field is part of the contract. We just
    // ignore the embedded token when we already trust the cookie.
    let hello = tokio::time::timeout(HELLO_TIMEOUT, stream.next()).await;
    let token = match hello {
        Ok(Some(Ok(Message::Text(s)))) => match serde_json::from_str::<ClientToHub>(&s) {
            Ok(ClientToHub::Hello { token, .. }) => token,
            _ => {
                let _ = send_client(
                    sink,
                    &HubToClient::Rejected {
                        reason: "expected hello".into(),
                    },
                )
                .await;
                return None;
            }
        },
        _ => return None,
    };

    // Cookie-authed (webterm) path: take the account from the verified
    // session id, ignore whatever token the SPA put in Hello.token.
    if let Some(account_name) = pre_auth {
        if send_client(
            sink,
            &HubToClient::Welcome {
                account: account_name.clone(),
            },
        )
        .await
        .is_err()
        {
            return None;
        }
        return Some(account_name);
    }

    // Token-authed (CLI client) path — original behavior.
    let mut headers = axum::http::HeaderMap::new();
    if let Ok(v) = axum::http::HeaderValue::from_str(&format!("Bearer {}", token)) {
        headers.insert(axum::http::header::AUTHORIZATION, v);
    } else {
        let _ = send_client(
            sink,
            &HubToClient::Rejected {
                reason: "bad token".into(),
            },
        )
        .await;
        return None;
    }
    match auth::authenticate(&state.db, &headers).await {
        Ok(a) => {
            let name = a.name.clone();
            if send_client(
                sink,
                &HubToClient::Welcome {
                    account: name.clone(),
                },
            )
            .await
            .is_err()
            {
                return None;
            }
            Some(name)
        }
        Err(reason) => {
            state.audit.write(AuditEvent {
                status: Some(401),
                reason: Some(reason.into()),
                ..AuditEvent::new("session_auth_denied")
            });
            let _ = send_client(
                sink,
                &HubToClient::Rejected {
                    reason: reason.into(),
                },
            )
            .await;
            None
        }
    }
}

async fn handle_client_frame<S>(ctx: &mut ConnCtx, frame: ClientToHub, sink: &mut S) -> bool
where
    S: SinkExt<Message, Error = axum::Error> + Unpin,
{
    match frame {
        ClientToHub::SelectAgent { agent } => {
            // Resolve which name the client is asking for: explicit, or
            // "first available" when None.
            let target_name = match agent {
                Some(name) => Some(name),
                None => {
                    let mut active = ctx.state.registry.list_active();
                    active.sort();
                    // Pick the first agent in the allowlist; fall back to
                    // the first online agent only if the allowlist is empty.
                    let mut allowed_pick: Option<String> = None;
                    for n in &active {
                        if ctx
                            .state
                            .db
                            .is_agent_allowed(&ctx.account_name, n)
                            .await
                            .unwrap_or(false)
                        {
                            allowed_pick = Some(n.clone());
                            break;
                        }
                    }
                    allowed_pick.or_else(|| active.first().cloned())
                }
            };
            let Some(name) = target_name else {
                let _ = send_client(
                    sink,
                    &HubToClient::SessionError {
                        message: "agent not online".into(),
                    },
                )
                .await;
                return true;
            };
            match ctx.state.db.is_agent_allowed(&ctx.account_name, &name).await {
                Ok(true) => {}
                Ok(false) => {
                    ctx.state.audit.write(AuditEvent {
                        account: Some(ctx.account_name.clone()),
                        agent: Some(name.clone()),
                        status: Some(403),
                        reason: Some("agent not in account allowlist".into()),
                        ..AuditEvent::new("agent_access_denied")
                    });
                    let _ = send_client(
                        sink,
                        &HubToClient::SessionError {
                            message: format!(
                                "account '{}' is not allowed to use agent '{}'",
                                ctx.account_name, name
                            ),
                        },
                    )
                    .await;
                    return true;
                }
                Err(e) => {
                    tracing::warn!(error = %e, "allowlist lookup failed");
                    let _ = send_client(
                        sink,
                        &HubToClient::SessionError {
                            message: "internal error".into(),
                        },
                    )
                    .await;
                    return true;
                }
            }
            let Some(conn) = ctx.state.registry.get(&name) else {
                let _ = send_client(
                    sink,
                    &HubToClient::SessionError {
                        message: "agent not online".into(),
                    },
                )
                .await;
                return true;
            };
            ctx.selected_agent = Some(conn);
            let _ = send_client(sink, &HubToClient::AgentSelected { agent: name }).await;
            true
        }
        ClientToHub::ListAgents => {
            // Strict-whitelist semantics: only show agents this account
            // is allowed to use. The list comes from the registry of
            // currently-connected agents, intersected with the db
            // allowlist.
            let names = ctx.state.registry.list_active();
            let current = ctx.selected_agent.as_ref().map(|c| c.name.clone());
            let mut items: Vec<AgentInfo> = Vec::new();
            for n in names {
                let allowed = ctx
                    .state
                    .db
                    .is_agent_allowed(&ctx.account_name, &n)
                    .await
                    .unwrap_or(false);
                if !allowed {
                    continue;
                }
                items.push(AgentInfo {
                    current: current.as_deref() == Some(&n),
                    name: n,
                });
            }
            let _ = send_client(sink, &HubToClient::AgentList { items }).await;
            true
        }
        ClientToHub::ListWorkspaces => {
            let Some(conn) = ctx.selected_agent.clone() else {
                let _ = send_client(
                    sink,
                    &HubToClient::SessionError {
                        message: "no agent selected".into(),
                    },
                )
                .await;
                return true;
            };
            let request_id = Uuid::new_v4();
            let (tx, rx) = oneshot::channel();
            conn.register_workspace_request(request_id, tx);
            if conn
                .send(ServerMsg::WorkspaceList {
                    request_id,
                    account: ctx.account_name.clone(),
                })
                .await
                .is_err()
            {
                let _ = send_client(
                    sink,
                    &HubToClient::SessionError {
                        message: "agent disconnected".into(),
                    },
                )
                .await;
                return true;
            }
            match tokio::time::timeout(WORKSPACE_REQUEST_TIMEOUT, rx).await {
                Ok(Ok(ClientMsg::WorkspaceListResult { items, error, .. })) => match error {
                    Some(e) => {
                        let _ = send_client(sink, &HubToClient::SessionError { message: e }).await;
                    }
                    None => {
                        // Decorate each item with whether a cloudcode
                        // client is currently attached (look it up in
                        // the global workspace mutex). tmux_alive came
                        // straight from the agent.
                        let agent_name = conn.name.clone();
                        let account = ctx.account_name.clone();
                        let infos: Vec<crate::pty_proto::WorkspaceInfo> = items
                            .into_iter()
                            .map(|it| {
                                let key =
                                    (agent_name.clone(), account.clone(), it.name.clone());
                                let has_client =
                                    ctx.state.workspaces.contains_key(&key);
                                crate::pty_proto::WorkspaceInfo {
                                    name: it.name,
                                    tmux_alive: it.tmux_alive,
                                    has_client,
                                }
                            })
                            .collect();
                        let _ = send_client(
                            sink,
                            &HubToClient::WorkspaceList { items: infos },
                        )
                        .await;
                    }
                },
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
            let Some(conn) = ctx.selected_agent.clone() else {
                let _ = send_client(
                    sink,
                    &HubToClient::SessionError {
                        message: "no agent selected".into(),
                    },
                )
                .await;
                return true;
            };
            let request_id = Uuid::new_v4();
            let (tx, rx) = oneshot::channel();
            conn.register_workspace_request(request_id, tx);
            if conn
                .send(ServerMsg::WorkspaceCreate {
                    request_id,
                    account: ctx.account_name.clone(),
                    name: name.clone(),
                })
                .await
                .is_err()
            {
                let _ = send_client(
                    sink,
                    &HubToClient::SessionError {
                        message: "agent disconnected".into(),
                    },
                )
                .await;
                return true;
            }
            match tokio::time::timeout(WORKSPACE_REQUEST_TIMEOUT, rx).await {
                Ok(Ok(ClientMsg::WorkspaceCreateResult { error, .. })) => match error {
                    Some(e) => {
                        let _ = send_client(sink, &HubToClient::SessionError { message: e }).await;
                    }
                    None => {
                        let _ = send_client(sink, &HubToClient::WorkspaceCreated { name }).await;
                    }
                },
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
            let Some(conn) = ctx.selected_agent.clone() else {
                let _ = send_client(
                    sink,
                    &HubToClient::SessionError {
                        message: "no agent selected".into(),
                    },
                )
                .await;
                return true;
            };
            if ctx.state.workspaces.contains_key(&(
                conn.name.clone(),
                ctx.account_name.clone(),
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
                    account: ctx.account_name.clone(),
                    name: name.clone(),
                })
                .await
                .is_err()
            {
                let _ = send_client(
                    sink,
                    &HubToClient::SessionError {
                        message: "agent disconnected".into(),
                    },
                )
                .await;
                return true;
            }
            match tokio::time::timeout(WORKSPACE_REQUEST_TIMEOUT, rx).await {
                Ok(Ok(ClientMsg::WorkspaceDeleteResult { error, .. })) => match error {
                    Some(e) => {
                        let _ = send_client(sink, &HubToClient::SessionError { message: e }).await;
                    }
                    None => {
                        let _ = send_client(sink, &HubToClient::WorkspaceDeleted { name }).await;
                    }
                },
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
        ClientToHub::ResetWorkspace { name } => {
            let Some(conn) = ctx.selected_agent.clone() else {
                let _ = send_client(
                    sink,
                    &HubToClient::SessionError {
                        message: "no agent selected".into(),
                    },
                )
                .await;
                return true;
            };
            if ctx.state.workspaces.contains_key(&(
                conn.name.clone(),
                ctx.account_name.clone(),
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
                .send(ServerMsg::WorkspaceReset {
                    request_id,
                    account: ctx.account_name.clone(),
                    name: name.clone(),
                })
                .await
                .is_err()
            {
                let _ = send_client(
                    sink,
                    &HubToClient::SessionError {
                        message: "agent disconnected".into(),
                    },
                )
                .await;
                return true;
            }
            match tokio::time::timeout(WORKSPACE_REQUEST_TIMEOUT, rx).await {
                Ok(Ok(ClientMsg::WorkspaceResetResult { error, .. })) => match error {
                    Some(e) => {
                        let _ = send_client(sink, &HubToClient::SessionError { message: e }).await;
                    }
                    None => {
                        let _ = send_client(sink, &HubToClient::WorkspaceReset { name }).await;
                    }
                },
                _ => {
                    let _ = send_client(
                        sink,
                        &HubToClient::SessionError {
                            message: "workspace reset timed out".into(),
                        },
                    )
                    .await;
                }
            }
            true
        }
        ClientToHub::OpenSession {
            workspace,
            cols,
            rows,
            claude_args,
        } => {
            if ctx.active.is_some() {
                let _ = send_client(
                    sink,
                    &HubToClient::SessionError {
                        message: "session already open".into(),
                    },
                )
                .await;
                return true;
            }
            let Some(conn) = ctx.selected_agent.clone() else {
                let _ = send_client(
                    sink,
                    &HubToClient::SessionError {
                        message: "no agent selected".into(),
                    },
                )
                .await;
                return true;
            };
            let session_id = Uuid::new_v4();
            let key = (
                conn.name.clone(),
                ctx.account_name.clone(),
                workspace.clone(),
            );
            // Take-over semantics: if another cloudcode client is already
            // attached to this (agent, account, workspace), evict it. We
            // close just the previous PTY on the agent — the tmux server
            // and its session keep running, so when we issue PtyOpen
            // below the agent attaches to the *same* tmux session and
            // the new client picks up where the old one left off. The
            // evicted client sees SessionClosed and drops back to its
            // own menu.
            if let Some(prev) = ctx.state.workspaces.insert(key.clone(), session_id) {
                if prev != session_id {
                    let _ = conn.send(ServerMsg::PtyClose { session_id: prev }).await;
                }
            }
            let (evt_tx, mut evt_rx) = mpsc::channel::<PtyEventOut>(PTY_EVENT_QUEUE);
            conn.register_session(session_id, evt_tx);
            if conn
                .send(ServerMsg::PtyOpen {
                    session_id,
                    account: ctx.account_name.clone(),
                    workspace: workspace.clone(),
                    cols,
                    rows,
                    claude_args,
                })
                .await
                .is_err()
            {
                conn.unregister_session(session_id);
                ctx.state
                    .workspaces
                    .remove_if(&key, |_, sid| *sid == session_id);
                let _ = send_client(
                    sink,
                    &HubToClient::SessionError {
                        message: "agent disconnected".into(),
                    },
                )
                .await;
                return true;
            }
            let cwd = match tokio::time::timeout(OPEN_TIMEOUT, evt_rx.recv()).await {
                Ok(Some(PtyEventOut::Frame(ClientMsg::PtyOpened { cwd, .. }))) => cwd,
                Ok(Some(PtyEventOut::Frame(ClientMsg::PtyError { message, .. }))) => {
                    conn.unregister_session(session_id);
                    ctx.state
                        .workspaces
                        .remove_if(&key, |_, sid| *sid == session_id);
                    let _ = send_client(sink, &HubToClient::SessionError { message }).await;
                    return true;
                }
                _ => {
                    conn.unregister_session(session_id);
                    ctx.state
                        .workspaces
                        .remove_if(&key, |_, sid| *sid == session_id);
                    let _ = send_client(
                        sink,
                        &HubToClient::SessionError {
                            message: "pty open timeout".into(),
                        },
                    )
                    .await;
                    return true;
                }
            };
            ctx.state.audit.write(AuditEvent {
                account: Some(ctx.account_name.clone()),
                agent: Some(conn.name.clone()),
                session_id: Some(session_id.to_string()),
                workspace: Some(workspace.clone()),
                status: Some(200),
                ..AuditEvent::new("session_opened")
            });
            // Fire-and-forget the sessions-table insert. If it fails the
            // audit JSONL + audit_events row still records the start.
            {
                let db = ctx.state.db.clone();
                let sid = session_id.to_string();
                let account = ctx.account_name.clone();
                let agent = conn.name.clone();
                let ws = workspace.clone();
                tokio::spawn(async move {
                    db.start_session(&sid, &account, &agent, &ws).await;
                });
            }
            let _ = send_client(
                sink,
                &HubToClient::SessionOpened {
                    agent: conn.name.clone(),
                    workspace: workspace.clone(),
                    cwd,
                },
            )
            .await;
            ctx.active = Some(ActiveSession {
                session_id,
                workspace,
                cols,
                rows,
                evt_rx,
            });
            true
        }
        ClientToHub::Resize { cols, rows } => {
            if let (Some(conn), Some(active)) = (ctx.selected_agent.as_ref(), ctx.active.as_mut()) {
                active.cols = cols;
                active.rows = rows;
                let _ = conn
                    .send(ServerMsg::PtyResize {
                        session_id: active.session_id,
                        cols,
                        rows,
                    })
                    .await;
            }
            true
        }
        ClientToHub::Close => false,
        ClientToHub::Hello { .. } | ClientToHub::Pong => true,
    }
}

async fn handle_agent_event<S>(ctx: &mut ConnCtx, evt: PtyEventOut, sink: &mut S) -> bool
where
    S: SinkExt<Message, Error = axum::Error> + Unpin,
{
    match evt {
        PtyEventOut::Output(bytes) => {
            if sink.send(Message::Binary(bytes.to_vec())).await.is_err() {
                return false;
            }
            true
        }
        PtyEventOut::Frame(ClientMsg::PtyClosed { reason, .. }) => {
            if let (Some(conn), Some(active)) = (ctx.selected_agent.as_ref(), ctx.active.take()) {
                conn.unregister_session(active.session_id);
                ctx.state.workspaces.remove_if(
                    &(
                        conn.name.clone(),
                        ctx.account_name.clone(),
                        active.workspace.clone(),
                    ),
                    |_, sid| *sid == active.session_id,
                );
                ctx.state.audit.write(AuditEvent {
                    account: Some(ctx.account_name.clone()),
                    agent: Some(conn.name.clone()),
                    session_id: Some(active.session_id.to_string()),
                    workspace: Some(active.workspace),
                    status: Some(200),
                    reason: reason.clone(),
                    ..AuditEvent::new("session_closed")
                });
                let db = ctx.state.db.clone();
                let sid = active.session_id.to_string();
                let r = reason.clone();
                tokio::spawn(async move {
                    db.end_session(&sid, r.as_deref()).await;
                });
            }
            let _ = send_client(sink, &HubToClient::SessionClosed { reason }).await;
            true
        }
        PtyEventOut::Frame(ClientMsg::PtyError { message, .. }) => {
            let _ = send_client(sink, &HubToClient::SessionError { message }).await;
            true
        }
        PtyEventOut::Frame(_) => true,
    }
}

async fn send_client<S>(sink: &mut S, msg: &HubToClient) -> Result<(), ()>
where
    S: SinkExt<Message, Error = axum::Error> + Unpin,
{
    let text = serde_json::to_string(msg).map_err(|_| ())?;
    sink.send(Message::Text(text)).await.map_err(|_| ())
}
