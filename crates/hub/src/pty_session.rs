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
/// How long the hub waits for the agent's `WorkspacePullAck` after
/// finishing the file stream. Generous — large workspaces over a
/// slow link can take a while, and a stuck pull is a worse failure
/// mode than a slow one.
const PULL_ACK_TIMEOUT: Duration = Duration::from_secs(30);
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
    let pre_auth = if let Some(sid) = app::parse_cookie(&headers, USER_SESSION_COOKIE) {
        state.user_auth.lookup(&sid).await
    } else {
        None
    };
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
            self.state.session_locks.remove_if(
                &(
                    conn.name.clone(),
                    self.account_name.clone(),
                    active.workspace.clone(),
                ),
                |_, sid| *sid == active.session_id,
            );
            self.state.session_workspaces.remove(&active.session_id);
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
                let tools = ctx
                    .state
                    .registry
                    .get(&n)
                    .map(|c| c.tools.clone())
                    .unwrap_or_default();
                items.push(AgentInfo {
                    current: current.as_deref() == Some(&n),
                    name: n,
                    tools,
                });
            }
            let _ = send_client(sink, &HubToClient::AgentList { items }).await;
            true
        }
        ClientToHub::ListWorkspaces => {
            // v1.13: workspaces are hub-canonical and per-account.
            // No agent fan-out — just read the DB and surface the
            // current lock + sync metadata to the client.
            let rows = match ctx.state.db.list_workspaces(&ctx.account_name).await {
                Ok(r) => r,
                Err(e) => {
                    tracing::warn!(error = %e, "list_workspaces failed");
                    let _ = send_client(
                        sink,
                        &HubToClient::SessionError {
                            message: "could not list workspaces".into(),
                        },
                    )
                    .await;
                    return true;
                }
            };
            let infos: Vec<crate::pty_proto::WorkspaceInfo> = rows
                .into_iter()
                .map(|r| crate::pty_proto::WorkspaceInfo {
                    name: r.name,
                    locked_by_agent: r.locked_by_agent,
                    last_sync_at: r.last_sync_at,
                    size_bytes: r.size_bytes.max(0) as u64,
                })
                .collect();
            let _ = send_client(sink, &HubToClient::WorkspaceList { items: infos }).await;
            true
        }
        ClientToHub::CreateWorkspace { name } => {
            // v1.13: hub creates the canonical row + on-disk dir; no
            // agent involved. Duplicate name surfaces as SessionError
            // — the `workspaces` PK is `(account, name)` so the DB
            // returns a unique-violation we translate verbatim.
            if let Err(e) = ctx
                .state
                .db
                .create_workspace(&ctx.account_name, &name)
                .await
            {
                let msg = e.to_string();
                let display = if msg.contains("UNIQUE") || msg.contains("PRIMARY KEY") {
                    format!("workspace '{}' already exists", name)
                } else {
                    format!("could not create workspace: {msg}")
                };
                let _ = send_client(sink, &HubToClient::SessionError { message: display }).await;
                return true;
            }
            if let Err(e) = ctx
                .state
                .workspaces
                .create_empty(&ctx.account_name, &name)
            {
                // Roll back the DB row so the next attempt isn't
                // permanently blocked by a half-created entry.
                let _ = ctx
                    .state
                    .db
                    .delete_workspace(&ctx.account_name, &name)
                    .await;
                let _ = send_client(
                    sink,
                    &HubToClient::SessionError {
                        message: format!("could not allocate workspace dir: {e}"),
                    },
                )
                .await;
                return true;
            }
            let _ = send_client(sink, &HubToClient::WorkspaceCreated { name }).await;
            true
        }
        ClientToHub::DeleteWorkspace { name } => {
            // v1.13: refuse the delete if some agent holds the lock,
            // otherwise nuke both the DB row and the on-disk copy.
            match ctx
                .state
                .db
                .get_workspace_lock(&ctx.account_name, &name)
                .await
            {
                Ok(Some(holder)) => {
                    let _ = send_client(
                        sink,
                        &HubToClient::SessionError {
                            message: format!(
                                "workspace '{}' is currently in use by agent '{}'",
                                name, holder
                            ),
                        },
                    )
                    .await;
                    return true;
                }
                Ok(None) => {}
                Err(e) => {
                    tracing::warn!(error = %e, "get_workspace_lock failed");
                    let _ = send_client(
                        sink,
                        &HubToClient::SessionError {
                            message: "could not check workspace lock".into(),
                        },
                    )
                    .await;
                    return true;
                }
            }
            // Best-effort filesystem cleanup first — leaving orphan
            // bytes around is worse than leaving an orphan row (the
            // row is invisible after the next list).
            if let Err(e) = ctx.state.workspaces.delete(&ctx.account_name, &name) {
                tracing::warn!(error = %e, "workspace fs delete failed; proceeding with row delete");
            }
            if let Err(e) = ctx
                .state
                .db
                .delete_workspace(&ctx.account_name, &name)
                .await
            {
                let _ = send_client(
                    sink,
                    &HubToClient::SessionError {
                        message: format!("could not delete workspace: {e}"),
                    },
                )
                .await;
                return true;
            }
            let _ = send_client(sink, &HubToClient::WorkspaceDeleted { name }).await;
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
            if ctx.state.session_locks.contains_key(&(
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
            agent,
            force,
            cols,
            rows,
            claude_args,
        } => {
            open_session(ctx, workspace, agent, force, cols, rows, claude_args, sink).await
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

/// v1.13 OpenSession orchestration. Pulled out of the giant
/// `handle_client_frame` match so the steps are readable in order:
///
/// 1. workspace must exist in the hub DB.
/// 2. the named agent must be online + in the account's ACL.
/// 3. inspect the lock — proceed / refuse / force-take.
/// 4. take the lock for this agent.
/// 5. allocate a session_id + per-session event channel.
/// 6. stream the canonical workspace bytes to the agent.
/// 7. wait for `WorkspacePullAck`.
/// 8. send `PtyOpen`; wait for `PtyOpened` / `PtyError`.
/// 9. record audit + session row, forward `SessionOpened` to client.
///
/// Failure at any step releases everything acquired up to that point
/// so a botched open leaves the world in the same state it was in.
#[allow(clippy::too_many_arguments)]
async fn open_session<S>(
    ctx: &mut ConnCtx,
    workspace: String,
    agent: String,
    force: bool,
    cols: u16,
    rows: u16,
    claude_args: Vec<String>,
    sink: &mut S,
) -> bool
where
    S: SinkExt<Message, Error = axum::Error> + Unpin,
{
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

    // ---- 1. workspace exists --------------------------------------------
    let exists = match ctx.state.db.list_workspaces(&ctx.account_name).await {
        Ok(rows) => rows.iter().any(|r| r.name == workspace),
        Err(e) => {
            tracing::warn!(error = %e, "list_workspaces failed during OpenSession");
            let _ = send_client(
                sink,
                &HubToClient::SessionError {
                    message: "could not look up workspace".into(),
                },
            )
            .await;
            return true;
        }
    };
    if !exists {
        let _ = send_client(
            sink,
            &HubToClient::SessionError {
                message: format!("workspace '{}' does not exist", workspace),
            },
        )
        .await;
        return true;
    }

    // ---- 2. agent online + allowed --------------------------------------
    match ctx.state.db.is_agent_allowed(&ctx.account_name, &agent).await {
        Ok(true) => {}
        Ok(false) => {
            ctx.state.audit.write(AuditEvent {
                account: Some(ctx.account_name.clone()),
                agent: Some(agent.clone()),
                status: Some(403),
                reason: Some("agent not in account allowlist".into()),
                ..AuditEvent::new("agent_access_denied")
            });
            let _ = send_client(
                sink,
                &HubToClient::SessionError {
                    message: format!(
                        "account '{}' is not allowed to use agent '{}'",
                        ctx.account_name, agent
                    ),
                },
            )
            .await;
            return true;
        }
        Err(e) => {
            tracing::warn!(error = %e, "is_agent_allowed lookup failed");
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
    let Some(conn) = ctx.state.registry.get(&agent) else {
        let _ = send_client(
            sink,
            &HubToClient::SessionError {
                message: format!("agent '{}' is not online", agent),
            },
        )
        .await;
        return true;
    };
    // Stash for the rest of this connection — Resize / Close still
    // need an `AgentConn` to talk to.
    ctx.selected_agent = Some(conn.clone());

    // ---- 3. lock inspection ---------------------------------------------
    let current_holder = match ctx
        .state
        .db
        .get_workspace_lock(&ctx.account_name, &workspace)
        .await
    {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(error = %e, "get_workspace_lock failed");
            let _ = send_client(
                sink,
                &HubToClient::SessionError {
                    message: "could not read workspace lock".into(),
                },
            )
            .await;
            return true;
        }
    };
    if let Some(holder) = current_holder.as_ref() {
        if holder != &agent {
            if !force {
                let _ = send_client(
                    sink,
                    &HubToClient::SessionError {
                        message: format!("workspace is in use by agent '{}'", holder),
                    },
                )
                .await;
                return true;
            }
            // Queue the old agent for cleanup. It will rm-rf its local
            // copy on its next hello frame; in the meantime its in-flight
            // sync ops (if any) will fail their acks and be dropped.
            if let Err(e) = ctx
                .state
                .db
                .queue_pending_cleanup(holder, &ctx.account_name, &workspace)
                .await
            {
                tracing::warn!(error = %e, "queue_pending_cleanup failed");
            }
            // If the old holder is currently online, push the cleanup
            // through its live WS as well. Otherwise the rm-rf only
            // happens on the agent's next reconnect — fine for a dead
            // agent, but for a still-online agent the cleanup would
            // race against any sync pushes the agent is still trying
            // to send for the lost workspace. The queue + live-send
            // are complementary: the live send is best-effort (the WS
            // may have died between this check and our send) and the
            // db row covers the dropped-frame case via Welcome drain.
            if let Some(holder_conn) = ctx.state.registry.get(holder) {
                let items = vec![(ctx.account_name.clone(), workspace.clone())];
                if let Err(e) = holder_conn
                    .send(ServerMsg::WorkspaceCleanup { items })
                    .await
                {
                    tracing::warn!(
                        agent = %holder,
                        error = %e,
                        "live WorkspaceCleanup send failed; will retry on agent reconnect via pending row"
                    );
                } else {
                    // Live delivery confirmed at the channel layer.
                    // The pending row stays in place until the agent
                    // actually drains it on its next Welcome — defence
                    // against the WS dying between `send().await`
                    // succeeding and the agent applying the cleanup.
                    tracing::info!(
                        agent = %holder,
                        account = %ctx.account_name,
                        workspace = %workspace,
                        "pushed live WorkspaceCleanup to online previous holder"
                    );
                }
            }
            ctx.state.audit.write(AuditEvent {
                account: Some(ctx.account_name.clone()),
                agent: Some(agent.clone()),
                workspace: Some(workspace.clone()),
                status: Some(200),
                reason: Some(format!("forced takeover from '{}'", holder)),
                ..AuditEvent::new("workspace_force_taken")
            });
        }
    }

    // ---- 4. take the lock -----------------------------------------------
    if let Err(e) = ctx
        .state
        .db
        .set_workspace_lock(&ctx.account_name, &workspace, Some(&agent))
        .await
    {
        tracing::warn!(error = %e, "set_workspace_lock failed");
        let _ = send_client(
            sink,
            &HubToClient::SessionError {
                message: format!("could not lock workspace: {e}"),
            },
        )
        .await;
        return true;
    }

    // From here on every error path MUST release the lock. We use a
    // small helper macro to keep that obvious at the call sites.
    let session_id = Uuid::new_v4();

    // ---- 5. register the session channel --------------------------------
    let (evt_tx, mut evt_rx) = mpsc::channel::<PtyEventOut>(PTY_EVENT_QUEUE);
    conn.register_session(session_id, evt_tx);
    let session_key = (
        conn.name.clone(),
        ctx.account_name.clone(),
        workspace.clone(),
    );
    // Take-over semantics inside the same agent: if another client
    // was attached, evict it. The agent's tmux session keeps running.
    if let Some(prev) = ctx
        .state
        .session_locks
        .insert(session_key.clone(), session_id)
    {
        if prev != session_id {
            let _ = conn.send(ServerMsg::PtyClose { session_id: prev }).await;
            ctx.state.session_workspaces.remove(&prev);
        }
    }
    ctx.state.session_workspaces.insert(
        session_id,
        (
            ctx.account_name.clone(),
            workspace.clone(),
            conn.name.clone(),
        ),
    );

    // ---- 6. stream the canonical files to the agent ---------------------
    let files = match ctx
        .state
        .workspaces
        .list_files(&ctx.account_name, &workspace)
    {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(error = %e, "workspaces.list_files failed");
            release_session(ctx, &conn, session_id, &session_key).await;
            let _ = send_client(
                sink,
                &HubToClient::SessionError {
                    message: format!("could not enumerate workspace: {e}"),
                },
            )
            .await;
            return true;
        }
    };
    if conn
        .send(ServerMsg::WorkspacePullStart {
            session_id,
            account: ctx.account_name.clone(),
            workspace: workspace.clone(),
            file_count: files.len() as u64,
        })
        .await
        .is_err()
    {
        release_session(ctx, &conn, session_id, &session_key).await;
        let _ = send_client(
            sink,
            &HubToClient::SessionError {
                message: "agent disconnected".into(),
            },
        )
        .await;
        return true;
    }
    if files.is_empty() {
        // Empty workspace: send a single sentinel frame so the agent
        // gets the same end-of-stream signal.
        if conn
            .send(ServerMsg::WorkspaceFile {
                session_id,
                path: String::new(),
                content: Vec::new(),
                is_last: true,
            })
            .await
            .is_err()
        {
            release_session(ctx, &conn, session_id, &session_key).await;
            let _ = send_client(
                sink,
                &HubToClient::SessionError {
                    message: "agent disconnected".into(),
                },
            )
            .await;
            return true;
        }
    } else {
        let last_idx = files.len() - 1;
        for (i, (path, _)) in files.iter().enumerate() {
            let content = match ctx.state.workspaces.read_file(
                &ctx.account_name,
                &workspace,
                path,
            ) {
                Ok(c) => c,
                Err(e) => {
                    tracing::warn!(path = %path, error = %e, "read_file failed during pull");
                    release_session(ctx, &conn, session_id, &session_key).await;
                    let _ = send_client(
                        sink,
                        &HubToClient::SessionError {
                            message: format!("could not read workspace file '{}': {e}", path),
                        },
                    )
                    .await;
                    return true;
                }
            };
            if conn
                .send(ServerMsg::WorkspaceFile {
                    session_id,
                    path: path.clone(),
                    content,
                    is_last: i == last_idx,
                })
                .await
                .is_err()
            {
                release_session(ctx, &conn, session_id, &session_key).await;
                let _ = send_client(
                    sink,
                    &HubToClient::SessionError {
                        message: "agent disconnected mid-pull".into(),
                    },
                )
                .await;
                return true;
            }
        }
    }

    // ---- 7. wait for WorkspacePullAck -----------------------------------
    let pull_ok = match tokio::time::timeout(PULL_ACK_TIMEOUT, evt_rx.recv()).await {
        Ok(Some(PtyEventOut::Frame(ClientMsg::WorkspacePullAck { ok, error, .. }))) => {
            if !ok {
                let msg = error.unwrap_or_else(|| "agent rejected workspace pull".into());
                release_session(ctx, &conn, session_id, &session_key).await;
                let _ = send_client(sink, &HubToClient::SessionError { message: msg }).await;
                return true;
            }
            true
        }
        Ok(Some(PtyEventOut::Frame(ClientMsg::PtyError { message, .. }))) => {
            release_session(ctx, &conn, session_id, &session_key).await;
            let _ = send_client(sink, &HubToClient::SessionError { message }).await;
            return true;
        }
        _ => {
            release_session(ctx, &conn, session_id, &session_key).await;
            let _ = send_client(
                sink,
                &HubToClient::SessionError {
                    message: "workspace pull timed out".into(),
                },
            )
            .await;
            return true;
        }
    };
    let _ = pull_ok;

    // ---- 8. send PtyOpen, wait for PtyOpened ----------------------------
    let sandbox = ctx
        .state
        .db
        .account_sandbox_enabled(&ctx.account_name)
        .await
        .unwrap_or(true);
    // Merge in webterm-side default args when the CLI client sent
    // none. webterm pre-fills its own; the CLI doesn't have a
    // preferences UI, so we do it here.
    let claude_args = if claude_args.is_empty() {
        args_from_user_preferences(&ctx.state.db, &ctx.account_name, "claude").await
    } else {
        claude_args
    };
    if conn
        .send(ServerMsg::PtyOpen {
            session_id,
            account: ctx.account_name.clone(),
            workspace: workspace.clone(),
            cols,
            rows,
            claude_args,
            sandbox,
            // v1.13 is claude-only; back-compat field stays None.
            tool: None,
        })
        .await
        .is_err()
    {
        release_session(ctx, &conn, session_id, &session_key).await;
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
            release_session(ctx, &conn, session_id, &session_key).await;
            let _ = send_client(sink, &HubToClient::SessionError { message }).await;
            return true;
        }
        _ => {
            release_session(ctx, &conn, session_id, &session_key).await;
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

    // ---- 9. audit + sessions row + reply --------------------------------
    ctx.state.audit.write(AuditEvent {
        account: Some(ctx.account_name.clone()),
        agent: Some(conn.name.clone()),
        session_id: Some(session_id.to_string()),
        workspace: Some(workspace.clone()),
        status: Some(200),
        ..AuditEvent::new("session_opened")
    });
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

/// Best-effort cleanup of every piece of state `open_session` may
/// have acquired before it bailed. Idempotent — calling it on a
/// session that never made it past step 4 just no-ops the later
/// removals.
async fn release_session(
    ctx: &ConnCtx,
    conn: &Arc<AgentConn>,
    session_id: Uuid,
    session_key: &(String, String, String),
) {
    conn.unregister_session(session_id);
    ctx.state
        .session_locks
        .remove_if(session_key, |_, sid| *sid == session_id);
    ctx.state.session_workspaces.remove(&session_id);
    // Release the workspace lock so the next OpenSession on this
    // workspace doesn't need force=true. We drop it only if it's still
    // held by THIS agent — defensive against a concurrent take.
    let (_, account, workspace) = session_key;
    match ctx.state.db.get_workspace_lock(account, workspace).await {
        Ok(Some(holder)) if holder == conn.name => {
            if let Err(e) = ctx
                .state
                .db
                .set_workspace_lock(account, workspace, None)
                .await
            {
                tracing::warn!(error = %e, "set_workspace_lock(None) failed in release");
            }
        }
        _ => {}
    }
}

/// Read the user's webterm preferences blob for `account` and pull
/// out the default args for `tool`. The blob is opaque JSON owned by
/// webterm; we know its current shape is `{tool_args: {<tool>:
/// [String, ...]}}`. Any deviation (missing row, bad JSON, wrong
/// shape, unknown tool) maps to an empty argv — matching webterm's
/// own fall-back behaviour, so a misconfigured row never silently
/// injects wrong flags into claude.
async fn args_from_user_preferences(
    db: &crate::db::Db,
    account: &str,
    tool: &str,
) -> Vec<String> {
    let Ok(Some(blob)) = db.get_user_preferences(account).await else {
        return Vec::new();
    };
    parse_tool_args_blob(&blob, tool)
}

/// Pure parse step extracted so tests can exercise every shape-edge
/// case without standing up a DB.
fn parse_tool_args_blob(blob: &str, tool: &str) -> Vec<String> {
    let Ok(json) = serde_json::from_str::<serde_json::Value>(blob) else {
        return Vec::new();
    };
    let Some(arr) = json
        .get("tool_args")
        .and_then(|v| v.as_object())
        .and_then(|m| m.get(tool))
        .and_then(|v| v.as_array())
    else {
        return Vec::new();
    };
    arr.iter()
        .filter_map(|v| v.as_str().map(|s| s.to_string()))
        .collect()
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
                ctx.state.session_locks.remove_if(
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pref_args_pulls_typed_array_for_requested_tool() {
        let blob = r#"{"tool_args":{"claude":["--model","claude-3-opus"]}}"#;
        assert_eq!(
            parse_tool_args_blob(blob, "claude"),
            vec!["--model".to_string(), "claude-3-opus".to_string()]
        );
        // Unknown tool key -> empty argv (matches webterm fall-back).
        assert_eq!(parse_tool_args_blob(blob, "ghost"), Vec::<String>::new());
    }

    #[test]
    fn pref_args_fall_back_to_empty_on_bad_shapes() {
        assert!(parse_tool_args_blob("not json", "claude").is_empty());
        assert!(parse_tool_args_blob("\"oops\"", "claude").is_empty());
        assert!(parse_tool_args_blob("{}", "claude").is_empty());
        assert!(parse_tool_args_blob(r#"{"tool_args":[]}"#, "claude").is_empty());
        // Wrong tool key in the map -> empty.
        assert!(
            parse_tool_args_blob(r#"{"tool_args":{"ghost":["x"]}}"#, "claude").is_empty()
        );
        // Right key, wrong value type -> empty.
        assert!(
            parse_tool_args_blob(r#"{"tool_args":{"claude":"--model x"}}"#, "claude").is_empty()
        );
        // Mixed-type array: keep only the string entries.
        assert_eq!(
            parse_tool_args_blob(
                r#"{"tool_args":{"claude":["--a",42,"--b",null,"--c"]}}"#,
                "claude"
            ),
            vec!["--a".to_string(), "--b".to_string(), "--c".to_string()]
        );
    }
}
