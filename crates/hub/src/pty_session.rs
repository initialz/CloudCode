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
use crate::pty_proto::{
    AgentInfo, ClientToHub, HubToClient, PaneLayout as ClientPaneLayout,
    SplitDirection as ClientSplitDir,
};
use crate::registry::{AgentConn, PtyEventOut};
use crate::tunnel::{
    ClientMsg, PaneLayout as AgentPaneLayout, ServerMsg, SplitDirection as AgentSplitDir,
};
use crate::AppState;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::State;
use base64::Engine;
use axum::http::HeaderMap;
use axum::response::Response;
use futures::{SinkExt, StreamExt};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{mpsc, oneshot};
use uuid::Uuid;

const HELLO_TIMEOUT: Duration = Duration::from_secs(10);
const OPEN_TIMEOUT: Duration = Duration::from_secs(20);
const WORKSPACE_REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
const PTY_EVENT_QUEUE: usize = 1024;
/// Cadence for the periodic `HubToClient::Ping` we shove down every
/// live user WS. Purpose is purely keepalive — without an
/// application-level frame on the wire, intermediate proxies (nginx,
/// Cloudflare, corporate firewalls) and browsers' own background-tab
/// throttling routinely drop "idle" WebSockets after 30-60s. The
/// client's wire.ts replies with `pong` and the connection stays
/// fresh. 25s is the safe side of every default proxy idle timeout
/// I've seen, while still cheap (one tiny text frame per minute per
/// user).
const USER_PING_INTERVAL: Duration = Duration::from_secs(25);

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

    // Subscribe to the per-account kick channel BEFORE entering the
    // loop so an admin "disconnect" fired between authenticate() and
    // the first select! is still caught by this connection.
    let mut kick_rx = state.user_kick_sender(&account_name).subscribe();

    // Channel used by spawned CLI file-drop upload tasks to hand their
    // `FsWriteResult` back to this loop for forwarding to the client.
    // Buffered modestly — uploads complete one result each.
    let (fs_result_tx, mut fs_result_rx) = mpsc::channel::<HubToClient>(16);

    let mut ctx = ConnCtx {
        state: state.clone(),
        account_name,
        selected_agent: None,
        active: None,
        fs_uploads: HashMap::new(),
    };

    // Application-level keepalive ticker (see USER_PING_INTERVAL doc).
    // The first tick fires immediately by default; consume it so we
    // don't ping the moment the socket opens.
    let mut ping_tick = tokio::time::interval(USER_PING_INTERVAL);
    ping_tick.tick().await;

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
            _ = ping_tick.tick() => {
                if send_client(&mut sink, &HubToClient::Ping).await.is_err() {
                    break;
                }
            }
            kick = kick_rx.recv() => {
                // Either we got a kick (Ok) or the sender was lagged
                // (RecvError::Lagged) — both mean an admin asked us
                // to leave. RecvError::Closed shouldn't happen (the
                // Sender lives in AppState for the hub's lifetime),
                // but if it does we treat it the same.
                let _ = kick;
                let frame = HubToClient::Rejected {
                    reason: "disconnected by administrator".into(),
                };
                if let Ok(json) = serde_json::to_string(&frame) {
                    let _ = sink.send(Message::Text(json)).await;
                }
                break;
            }
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
                        if !handle_client_frame(&mut ctx, frame, &mut sink, &fs_result_tx).await {
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
            fs_result = fs_result_rx.recv() => {
                // A spawned CLI file-drop upload finished — forward its
                // FsWriteResult to the client. The Sender lives in ctx +
                // the loop for the connection's life, so `None` can't
                // happen, but treat it as a no-op if it ever does.
                if let Some(frame) = fs_result {
                    if send_client(&mut sink, &frame).await.is_err() {
                        break;
                    }
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
    /// In-flight CLI file-drop uploads, keyed by request_id. Each entry
    /// is the sender half of the chunk stream feeding a spawned
    /// `upload_file_to_agent`; `FsWriteChunk` frames push decoded bytes
    /// here, and the `eof` chunk drops the sender to close the stream.
    fs_uploads: HashMap<Uuid, mpsc::Sender<Result<bytes::Bytes, String>>>,
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

async fn handle_client_frame<S>(
    ctx: &mut ConnCtx,
    frame: ClientToHub,
    sink: &mut S,
    fs_result_tx: &mpsc::Sender<HubToClient>,
) -> bool
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
            // v1.13: workspaces are bound to agents in hub's `workspaces`
            // table at create time. Listing reads straight from the DB
            // — no longer per-agent — and decorates each row with
            // whether the bound agent is currently registered + whether
            // a client is already attached. `tmux_alive` is left false
            // here; without a round-trip to each agent we can't tell,
            // and the picker doesn't surface it anyway in the new flow.
            let bindings = match ctx
                .state
                .db
                .list_workspaces_for_account(&ctx.account_name)
                .await
            {
                Ok(b) => b,
                Err(e) => {
                    tracing::warn!(error = %e, "list_workspaces_for_account failed");
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
            // Filter to the ACL — if an admin revoked the account's
            // access to an agent after a workspace was created on it,
            // the picker shouldn't keep advertising that workspace.
            let allowed = match ctx
                .state
                .db
                .list_allowed_agents(&ctx.account_name)
                .await
            {
                Ok(v) => v.into_iter().collect::<std::collections::HashSet<_>>(),
                Err(_) => Default::default(),
            };
            let infos: Vec<crate::pty_proto::WorkspaceInfo> = bindings
                .into_iter()
                .filter(|b| allowed.contains(&b.agent))
                .map(|b| {
                    let agent_online = ctx.state.registry.get(&b.agent).is_some();
                    let key = (
                        b.agent.clone(),
                        ctx.account_name.clone(),
                        b.name.clone(),
                    );
                    let has_client = ctx.state.workspaces.contains_key(&key);
                    crate::pty_proto::WorkspaceInfo {
                        name: b.name,
                        agent: b.agent,
                        agent_online,
                        tmux_alive: false,
                        has_client,
                    }
                })
                .collect();
            let _ = send_client(sink, &HubToClient::WorkspaceList { items: infos }).await;
            true
        }
        ClientToHub::CreateWorkspace { name, agent } => {
            // v1.13: client picks the owning agent at creation time
            // and carries it on the wire. Hub validates the ACL,
            // inserts the binding (UNIQUE on (account, agent, name)
            // → duplicate gives a clean SessionError), then asks the
            // agent to mkdir its on-disk slot.
            if name.trim().is_empty() || agent.trim().is_empty() {
                let _ = send_client(
                    sink,
                    &HubToClient::SessionError {
                        message: "workspace name and agent are required".into(),
                    },
                )
                .await;
                return true;
            }
            match ctx
                .state
                .db
                .is_agent_allowed(&ctx.account_name, &agent)
                .await
            {
                Ok(true) => {}
                Ok(false) => {
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
                    tracing::warn!(error = %e, "is_agent_allowed failed during CreateWorkspace");
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
                        message: format!("agent '{}' is offline", agent),
                    },
                )
                .await;
                return true;
            };
            // Record the binding first. If the agent then fails to
            // create the dir we rollback below — better than a
            // half-created state with the agent owning a dir nobody
            // can find.
            if let Err(e) = ctx
                .state
                .db
                .insert_workspace_binding(&ctx.account_name, &agent, &name)
                .await
            {
                let msg = e.to_string();
                let display = if msg.contains("UNIQUE") {
                    format!(
                        "workspace '{}' already exists on agent '{}'",
                        name, agent
                    )
                } else {
                    format!("could not record workspace binding: {msg}")
                };
                let _ = send_client(sink, &HubToClient::SessionError { message: display }).await;
                return true;
            }
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
                // Rollback the binding so the user can retry on a
                // different agent / after the offending one reconnects.
                let _ = ctx
                    .state
                    .db
                    .delete_workspace_binding(&ctx.account_name, &agent, &name)
                    .await;
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
                        let _ = ctx
                            .state
                            .db
                            .delete_workspace_binding(&ctx.account_name, &agent, &name)
                            .await;
                        let _ = send_client(sink, &HubToClient::SessionError { message: e }).await;
                    }
                    None => {
                        let _ = send_client(sink, &HubToClient::WorkspaceCreated { name }).await;
                    }
                },
                _ => {
                    let _ = ctx
                        .state
                        .db
                        .delete_workspace_binding(&ctx.account_name, &agent, &name)
                        .await;
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
        ClientToHub::DeleteWorkspace { name, agent } => {
            handle_workspace_op(ctx, sink, &name, &agent, WorkspaceOp::Delete).await;
            true
        }
        ClientToHub::ResetWorkspace { name, agent } => {
            handle_workspace_op(ctx, sink, &name, &agent, WorkspaceOp::Reset).await;
            true
        }
        ClientToHub::OpenSession {
            workspace,
            agent,
            cols,
            rows,
            claude_args,
            tool,
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
            // v1.13: route by the workspace's bound agent — no
            // separate SelectAgent step required. Verify the
            // binding exists, the account is ACL'd for the agent,
            // and the agent is online.
            match ctx
                .state
                .db
                .get_workspace_agent(&ctx.account_name, &agent, &workspace)
                .await
            {
                Ok(Some(_)) => {}
                Ok(None) => {
                    let _ = send_client(
                        sink,
                        &HubToClient::SessionError {
                            message: format!(
                                "workspace '{}' on agent '{}' does not exist",
                                workspace, agent
                            ),
                        },
                    )
                    .await;
                    return true;
                }
                Err(e) => {
                    tracing::warn!(error = %e, "get_workspace_agent during OpenSession");
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
            match ctx
                .state
                .db
                .is_agent_allowed(&ctx.account_name, &agent)
                .await
            {
                Ok(true) => {}
                Ok(false) => {
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
                    tracing::warn!(error = %e, "is_agent_allowed during OpenSession");
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
                        message: format!("agent '{}' is offline", agent),
                    },
                )
                .await;
                return true;
            };
            // Cache the connection on ctx so subsequent in-session
            // ops (Resize, Close) still find it via the existing
            // selected_agent path.
            ctx.selected_agent = Some(conn.clone());
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
            // Per-account sandbox mode — looked up once per OpenSession
            // so admin-UI changes take effect on the next session.
            let sandbox_mode = ctx
                .state
                .db
                .account_sandbox_mode(&ctx.account_name)
                .await
                .unwrap_or_else(|_| "strict".to_string());
            // Legacy bool kept for pre-v1.23 agents that don't read
            // sandbox_mode. "strict" or "off" both map cleanly; "off"
            // collapses to false (permissive) for legacy agents — the
            // user gets one mode worse than they asked for, but it
            // stays running.
            let sandbox = sandbox_mode == "strict";
            // Resolve effective env + args from the stored
            // `user_preferences` blob, applying the per-workspace
            // snapshot rule: if `workspaces["<agent>/<workspace>"]`
            // exists, use ITS env + tool_args (a full snapshot — no
            // merge with global); otherwise fall back to global env +
            // global tool_args. `tool` falls back to "claude" because
            // the CLI omits it when relying on the agent's configured
            // default — using webterm's default tool keeps the common
            // case (user set claude args + CLI without --tool) doing
            // the right thing.
            let lookup_tool = tool.as_deref().unwrap_or("claude");
            let (effective_env, effective_args) = match ctx
                .state
                .db
                .get_user_preferences(&ctx.account_name)
                .await
            {
                Ok(Some(blob)) => {
                    resolve_effective_config(&blob, &agent, &workspace, lookup_tool)
                }
                _ => (HashMap::new(), Vec::new()),
            };
            // Non-empty CLI args always win for args (preserves the
            // existing behaviour where webterm pre-populates claude_args
            // from prefs before dispatching). Env always comes from
            // stored config — clients never send env.
            let claude_args = if claude_args.is_empty() {
                effective_args
            } else {
                claude_args
            };
            let env = effective_env;
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
                    sandbox,
                    sandbox_mode: Some(sandbox_mode),
                    tool,
                    env,
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
        ClientToHub::SplitPane {
            tool,
            direction,
            args,
        } => {
            // Split adds another tmux pane *to the active session*; it
            // requires both an agent and an open session to be in scope.
            let (Some(conn), Some(active)) = (ctx.selected_agent.as_ref(), ctx.active.as_ref())
            else {
                let _ = send_client(
                    sink,
                    &HubToClient::SessionError {
                        message: "no active session to split".into(),
                    },
                )
                .await;
                return true;
            };
            if !valid_tool_name(&tool) {
                let _ = send_client(
                    sink,
                    &HubToClient::SessionError {
                        message: format!("invalid tool name '{}'", tool),
                    },
                )
                .await;
                return true;
            }
            // Fire-and-forget. The agent will reply with a
            // SplitPaneResult on the same session_id; the
            // session-event arm of this loop turns errors into
            // SessionError frames for the client. Success is silent —
            // the new pane's output already streams through the
            // existing PTY tap.
            let direction = match direction {
                ClientSplitDir::Right => AgentSplitDir::Right,
                ClientSplitDir::Down => AgentSplitDir::Down,
            };
            if conn
                .send(ServerMsg::SplitPane {
                    session_id: active.session_id,
                    tool,
                    direction,
                    args,
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
            }
            true
        }
        ClientToHub::ChangeLayout { layout } => {
            // Same shape as SplitPane: needs an open session, fire-and-forget.
            let (Some(conn), Some(active)) = (ctx.selected_agent.as_ref(), ctx.active.as_ref())
            else {
                let _ = send_client(
                    sink,
                    &HubToClient::SessionError {
                        message: "no active session to re-layout".into(),
                    },
                )
                .await;
                return true;
            };
            let layout = match layout {
                ClientPaneLayout::SideBySide => AgentPaneLayout::SideBySide,
                ClientPaneLayout::Stacked => AgentPaneLayout::Stacked,
            };
            if conn
                .send(ServerMsg::ChangeLayout {
                    session_id: active.session_id,
                    layout,
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
            }
            true
        }
        ClientToHub::FsWriteInit {
            request_id,
            agent,
            workspace,
            path,
        } => {
            // CLI file-drop (Phase 2): authorize, resolve the agent
            // conn, then spawn a task that streams chunks (arriving as
            // subsequent FsWriteChunk frames) to the agent via the
            // shared upload helper. The chunk stream is fed by an mpsc
            // channel whose sender we stash on ctx keyed by request_id.
            if let Err(reason) =
                authorize_workspace_ws(&ctx.state, &ctx.account_name, &agent, &workspace).await
            {
                let _ = fs_result_tx
                    .send(HubToClient::FsWriteResult {
                        request_id,
                        final_name: None,
                        error: Some(reason),
                    })
                    .await;
                return true;
            }
            let Some(conn) = ctx.state.registry.get(&agent) else {
                let _ = fs_result_tx
                    .send(HubToClient::FsWriteResult {
                        request_id,
                        final_name: None,
                        error: Some(format!("agent '{}' is offline", agent)),
                    })
                    .await;
                return true;
            };
            // Split `path` into (dir-with-trailing-slash, file_name) so
            // the shared helper rebuilds the same `target_path` the HTTP
            // path does. A path with no '/' is a bare filename in the
            // workspace root.
            let (dir, file_name) = match path.rsplit_once('/') {
                Some((d, f)) => (format!("{d}/"), f.to_string()),
                None => (String::new(), path.clone()),
            };
            let (chunk_tx, chunk_rx) = mpsc::channel::<Result<bytes::Bytes, String>>(16);
            ctx.fs_uploads.insert(request_id, chunk_tx);
            let account = ctx.account_name.clone();
            let result_tx = fs_result_tx.clone();
            tokio::spawn(async move {
                let stream = futures::stream::unfold(chunk_rx, |mut rx| async move {
                    rx.recv().await.map(|item| (item, rx))
                });
                let outcome = crate::app::api::upload_file_to_agent(
                    &conn, &account, &workspace, &file_name, &dir, stream,
                )
                .await;
                let _ = result_tx
                    .send(HubToClient::FsWriteResult {
                        request_id,
                        final_name: if outcome.error.is_none() {
                            Some(outcome.final_name)
                        } else {
                            None
                        },
                        error: outcome.error,
                    })
                    .await;
            });
            true
        }
        ClientToHub::FsWriteChunk {
            request_id,
            data_b64,
            eof,
        } => {
            // Route this chunk into the in-flight upload's stream. An
            // unknown request_id (init never seen / already finished) is
            // silently ignored.
            if eof {
                // Dropping the sender closes the stream → the spawned
                // task sees end-of-input and sends the terminal chunk.
                ctx.fs_uploads.remove(&request_id);
            } else if let Some(tx) = ctx.fs_uploads.get(&request_id) {
                let item = match base64::engine::general_purpose::STANDARD.decode(&data_b64) {
                    Ok(bytes) => Ok(bytes::Bytes::from(bytes)),
                    Err(e) => Err(format!("invalid base64 in upload chunk: {e}")),
                };
                // If the upload task already exited (channel closed),
                // drop the dangling entry so we stop trying.
                if tx.send(item).await.is_err() {
                    ctx.fs_uploads.remove(&request_id);
                }
            }
            true
        }
        // Routing not yet implemented — accepted and ignored until Task 2+.
        ClientToHub::BrowserRpc { .. } | ClientToHub::BrowserClosed { .. } => true,
        ClientToHub::Close => false,
        ClientToHub::Hello { .. } | ClientToHub::Pong => true,
    }
}

/// Read the user's webterm preferences blob for `account` and pull
/// out the default args for `tool`. The blob is opaque JSON owned by
#[derive(Clone, Copy)]
enum WorkspaceOp {
    Delete,
    Reset,
}

/// Shared body of Delete / Reset — they only differ in the wire
/// frame they send the agent and the success reply they send the
/// client. Both:
///   1. Verify the (account, agent, name) binding exists in the DB.
///   2. Refuse if the workspace is currently in use (`state.workspaces`
///      contains the (agent, account, name) tuple — set during
///      OpenSession, cleared on close).
///   3. Look up the live agent conn; bail if it's offline.
///   4. Forward the op via the existing WorkspaceRequest plumbing.
///   5. On Delete success, drop the DB binding too.
async fn handle_workspace_op<S>(
    ctx: &mut ConnCtx,
    sink: &mut S,
    name: &str,
    agent: &str,
    op: WorkspaceOp,
) where
    S: SinkExt<Message, Error = axum::Error> + Unpin,
{
    let label = match op {
        WorkspaceOp::Delete => "delete",
        WorkspaceOp::Reset => "reset",
    };
    match ctx
        .state
        .db
        .get_workspace_agent(&ctx.account_name, agent, name)
        .await
    {
        Ok(Some(_)) => {}
        Ok(None) => {
            let _ = send_client(
                sink,
                &HubToClient::SessionError {
                    message: format!(
                        "workspace '{}' on agent '{}' does not exist",
                        name, agent
                    ),
                },
            )
            .await;
            return;
        }
        Err(e) => {
            tracing::warn!(error = %e, "get_workspace_agent failed");
            let _ = send_client(
                sink,
                &HubToClient::SessionError {
                    message: "internal error".into(),
                },
            )
            .await;
            return;
        }
    }
    if ctx.state.workspaces.contains_key(&(
        agent.to_string(),
        ctx.account_name.clone(),
        name.to_string(),
    )) {
        let _ = send_client(
            sink,
            &HubToClient::SessionError {
                message: format!("workspace '{}' is currently in use", name),
            },
        )
        .await;
        return;
    }
    let conn = match ctx.state.registry.get(agent) {
        Some(c) => c,
        None => {
            // Agent offline. Reset needs the agent (tmux state lives
            // there), so we still bail. Delete can degrade gracefully:
            // drop the hub-side binding so the row stops haunting the
            // picker. The on-disk dir on the dead agent stays put —
            // when/if that agent comes back the Hello re-seed will
            // re-create the binding, which matches the user's mental
            // model ("workspace came back when the agent did").
            match op {
                WorkspaceOp::Delete => {
                    if let Err(e) = ctx
                        .state
                        .db
                        .delete_workspace_binding(&ctx.account_name, agent, name)
                        .await
                    {
                        tracing::warn!(error = %e, "delete_workspace_binding (offline path)");
                        let _ = send_client(
                            sink,
                            &HubToClient::SessionError {
                                message: "could not delete binding".into(),
                            },
                        )
                        .await;
                        return;
                    }
                    tracing::info!(
                        agent = %agent,
                        account = %ctx.account_name,
                        workspace = %name,
                        "user deleted workspace binding while agent offline"
                    );
                    let _ = send_client(
                        sink,
                        &HubToClient::WorkspaceDeleted {
                            name: name.to_string(),
                        },
                    )
                    .await;
                    return;
                }
                WorkspaceOp::Reset => {
                    let _ = send_client(
                        sink,
                        &HubToClient::SessionError {
                            message: format!("agent '{}' is offline", agent),
                        },
                    )
                    .await;
                    return;
                }
            }
        }
    };
    let request_id = Uuid::new_v4();
    let (tx, rx) = oneshot::channel();
    conn.register_workspace_request(request_id, tx);
    let send_result = match op {
        WorkspaceOp::Delete => {
            conn.send(ServerMsg::WorkspaceDelete {
                request_id,
                account: ctx.account_name.clone(),
                name: name.to_string(),
            })
            .await
        }
        WorkspaceOp::Reset => {
            conn.send(ServerMsg::WorkspaceReset {
                request_id,
                account: ctx.account_name.clone(),
                name: name.to_string(),
            })
            .await
        }
    };
    if send_result.is_err() {
        let _ = send_client(
            sink,
            &HubToClient::SessionError {
                message: "agent disconnected".into(),
            },
        )
        .await;
        return;
    }
    let response = tokio::time::timeout(WORKSPACE_REQUEST_TIMEOUT, rx).await;
    match (op, response) {
        (WorkspaceOp::Delete, Ok(Ok(ClientMsg::WorkspaceDeleteResult { error, .. }))) => {
            match error {
                Some(e) => {
                    let _ = send_client(sink, &HubToClient::SessionError { message: e }).await;
                }
                None => {
                    if let Err(e) = ctx
                        .state
                        .db
                        .delete_workspace_binding(&ctx.account_name, agent, name)
                        .await
                    {
                        tracing::warn!(error = %e, "delete_workspace_binding (post-agent-ack)");
                    }
                    let _ = send_client(
                        sink,
                        &HubToClient::WorkspaceDeleted {
                            name: name.to_string(),
                        },
                    )
                    .await;
                }
            }
        }
        (WorkspaceOp::Reset, Ok(Ok(ClientMsg::WorkspaceResetResult { error, .. }))) => {
            match error {
                Some(e) => {
                    let _ = send_client(sink, &HubToClient::SessionError { message: e }).await;
                }
                None => {
                    let _ = send_client(
                        sink,
                        &HubToClient::WorkspaceReset {
                            name: name.to_string(),
                        },
                    )
                    .await;
                }
            }
        }
        _ => {
            let _ = send_client(
                sink,
                &HubToClient::SessionError {
                    message: format!("workspace {label} timed out"),
                },
            )
            .await;
        }
    }
}

/// Resolve the effective `(env, tool_args)` for an OpenSession from the
/// stored `user_preferences` blob. webterm owns the blob schema; the hub
/// reads only what it needs:
///
/// ```jsonc
/// {
///   "tool_args": { "<tool>": [String, ...] },   // global
///   "env": { "KEY": "VALUE" },                    // global
///   "workspaces": {
///     "<agent>/<workspace>": {
///       "env": { ... },
///       "tool_args": { "<tool>": [...] }
///     }
///   }
/// }
/// ```
///
/// Resolution rule (per-workspace is a FULL SNAPSHOT — pick one or the
/// other, never merge): if `workspaces["<agent>/<workspace>"]` exists,
/// use its `env` + `tool_args[tool]`; otherwise use the global `env` +
/// global `tool_args[tool]`. Any deviation (missing key, bad JSON, wrong
/// shape) maps to defaults (`{}` env, `[]` args) — matching webterm's own
/// defensive fall-back, so a misconfigured row never silently injects
/// wrong env/flags into claude/codex.
fn resolve_effective_config(
    blob: &str,
    agent: &str,
    workspace: &str,
    tool: &str,
) -> (HashMap<String, String>, Vec<String>) {
    let Ok(json) = serde_json::from_str::<serde_json::Value>(blob) else {
        return (HashMap::new(), Vec::new());
    };
    let ws_key = format!("{}/{}", agent, workspace);
    // Scope from which we read `env` + `tool_args`: the workspace
    // snapshot if present, otherwise the top-level (global) object.
    let scope = json
        .get("workspaces")
        .and_then(|v| v.as_object())
        .and_then(|m| m.get(&ws_key))
        .unwrap_or(&json);
    (env_from_scope(scope), tool_args_from_scope(scope, tool))
}

/// Pull `env` (a flat `{string: string}` map) from a config scope,
/// dropping any non-string values defensively.
fn env_from_scope(scope: &serde_json::Value) -> HashMap<String, String> {
    let Some(obj) = scope.get("env").and_then(|v| v.as_object()) else {
        return HashMap::new();
    };
    obj.iter()
        .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
        .collect()
}

/// Pull `tool_args[tool]` (an array of strings) from a config scope,
/// dropping any non-string entries defensively.
fn tool_args_from_scope(scope: &serde_json::Value, tool: &str) -> Vec<String> {
    let Some(arr) = scope
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

/// Authorize an (account, agent, workspace) tuple for a CLI file-drop
/// upload. Same two checks as the HTTP `app::api::authorize_workspace`
/// (account allowed on agent + workspace exists for this account/agent)
/// but returns a plain string reason for the WS `FsWriteResult.error`
/// instead of an HTTP `Response`.
async fn authorize_workspace_ws(
    state: &Arc<AppState>,
    account: &str,
    agent: &str,
    workspace: &str,
) -> Result<(), String> {
    match state.db.is_agent_allowed(account, agent).await {
        Ok(true) => {}
        Ok(false) => return Err("account is not allowed on this agent".into()),
        Err(e) => {
            tracing::warn!(error = %e, "is_agent_allowed failed (fs upload)");
            return Err("internal error".into());
        }
    }
    match state.db.get_workspace_agent(account, agent, workspace).await {
        Ok(Some(_)) => Ok(()),
        Ok(None) => Err("workspace not found for this account/agent".into()),
        Err(e) => {
            tracing::warn!(error = %e, "get_workspace_agent failed (fs upload)");
            Err("internal error".into())
        }
    }
}

/// Same rule as the agent's `validate_name(_, "tool")` — keep them
/// aligned so we reject early on the hub instead of round-tripping.
fn valid_tool_name(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 63
        && !s.starts_with('-')
        && !s.starts_with('.')
        && s.chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_')
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
        PtyEventOut::Frame(ClientMsg::SplitPaneResult { error, .. }) => {
            if let Some(message) = error {
                let _ = send_client(sink, &HubToClient::SessionError { message }).await;
            }
            // Success: the agent already spawned the pane; its bytes
            // arrive through the existing PTY tap. Nothing else to do.
            true
        }
        PtyEventOut::Frame(ClientMsg::BrowserRpc { payload, .. }) => {
            let _ = send_client(sink, &HubToClient::BrowserRpc { payload }).await;
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

    // Helper: resolve args only, falling through to global (no matching
    // workspace key), to keep these focused on the tool_args parsing.
    fn global_args(blob: &str, tool: &str) -> Vec<String> {
        resolve_effective_config(blob, "ag", "ws", tool).1
    }
    fn global_env(blob: &str) -> HashMap<String, String> {
        resolve_effective_config(blob, "ag", "ws", "claude").0
    }

    #[test]
    fn pref_args_pulls_typed_array_for_requested_tool() {
        let blob = r#"{"tool_args":{"claude":["--model","claude-3-opus"],"codex":[]}}"#;
        assert_eq!(
            global_args(blob, "claude"),
            vec!["--model".to_string(), "claude-3-opus".to_string()]
        );
        assert_eq!(global_args(blob, "codex"), Vec::<String>::new());
    }

    #[test]
    fn pref_args_fall_back_to_empty_on_bad_shapes() {
        assert!(global_args("not json", "claude").is_empty());
        assert!(global_args("\"oops\"", "claude").is_empty());
        assert!(global_args("{}", "claude").is_empty());
        assert!(global_args(r#"{"tool_args":[]}"#, "claude").is_empty());
        assert!(global_args(r#"{"tool_args":{"codex":["x"]}}"#, "claude").is_empty());
        assert!(global_args(r#"{"tool_args":{"claude":"--model x"}}"#, "claude").is_empty());
        assert_eq!(
            global_args(
                r#"{"tool_args":{"claude":["--a",42,"--b",null,"--c"]}}"#,
                "claude"
            ),
            vec!["--a".to_string(), "--b".to_string(), "--c".to_string()]
        );
    }

    #[test]
    fn env_pulls_string_map_and_drops_non_strings() {
        let blob = r#"{"env":{"A":"1","B":"two","C":3,"D":null}}"#;
        let env = global_env(blob);
        assert_eq!(env.get("A"), Some(&"1".to_string()));
        assert_eq!(env.get("B"), Some(&"two".to_string()));
        assert!(!env.contains_key("C"));
        assert!(!env.contains_key("D"));
    }

    #[test]
    fn env_defaults_empty_on_bad_shapes() {
        assert!(global_env("not json").is_empty());
        assert!(global_env("{}").is_empty());
        assert!(global_env(r#"{"env":[]}"#).is_empty());
        assert!(global_env(r#"{"env":"oops"}"#).is_empty());
    }

    #[test]
    fn workspace_snapshot_wins_over_global_no_merge() {
        // Workspace key present -> use its env + tool_args ONLY (full
        // snapshot, no merge with global).
        let blob = r#"{
            "env": {"GLOBAL":"g"},
            "tool_args": {"claude":["--global"]},
            "workspaces": {
                "ag/ws": {
                    "env": {"WS":"w"},
                    "tool_args": {"claude":["--ws"]}
                }
            }
        }"#;
        let (env, args) = resolve_effective_config(blob, "ag", "ws", "claude");
        assert_eq!(env.get("WS"), Some(&"w".to_string()));
        assert!(!env.contains_key("GLOBAL"), "snapshot must not merge global env");
        assert_eq!(args, vec!["--ws".to_string()]);
    }

    #[test]
    fn missing_workspace_key_falls_through_to_global() {
        let blob = r#"{
            "env": {"GLOBAL":"g"},
            "tool_args": {"claude":["--global"]},
            "workspaces": {
                "other/ws": {"env":{"X":"1"}}
            }
        }"#;
        let (env, args) = resolve_effective_config(blob, "ag", "ws", "claude");
        assert_eq!(env.get("GLOBAL"), Some(&"g".to_string()));
        assert_eq!(args, vec!["--global".to_string()]);
    }

    #[test]
    fn workspace_key_uses_agent_and_name() {
        // Same workspace name on a different agent must NOT collide.
        let blob = r#"{
            "workspaces": {
                "agentA/proj": {"env":{"WHICH":"A"}},
                "agentB/proj": {"env":{"WHICH":"B"}}
            }
        }"#;
        assert_eq!(
            resolve_effective_config(blob, "agentA", "proj", "claude")
                .0
                .get("WHICH"),
            Some(&"A".to_string())
        );
        assert_eq!(
            resolve_effective_config(blob, "agentB", "proj", "claude")
                .0
                .get("WHICH"),
            Some(&"B".to_string())
        );
    }

    #[test]
    fn workspace_snapshot_with_empty_env_deletes_inherited() {
        // Forked snapshot with no env -> empty map even though global has env.
        let blob = r#"{
            "env": {"GLOBAL":"g"},
            "workspaces": { "ag/ws": {"tool_args":{"claude":["--ws"]}} }
        }"#;
        let (env, _) = resolve_effective_config(blob, "ag", "ws", "claude");
        assert!(env.is_empty());
    }
}
