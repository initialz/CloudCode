use crate::tunnel::{ClientMsg, ServerMsg};
use dashmap::DashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use tokio::sync::{mpsc, oneshot};
use uuid::Uuid;

static NEXT_CONN_ID: AtomicU64 = AtomicU64::new(1);

pub struct AgentRegistry {
    agents: DashMap<String, Arc<AgentConn>>,
}

impl AgentRegistry {
    pub fn new() -> Self {
        Self {
            agents: DashMap::new(),
        }
    }

    /// Atomically register `name`. Returns None if a connection with this
    /// name is already registered (caller should send Rejected::NameTaken).
    pub fn try_register(
        self: &Arc<Self>,
        name: String,
        send: mpsc::Sender<ServerMsg>,
    ) -> Option<Arc<AgentConn>> {
        match self.agents.entry(name.clone()) {
            dashmap::mapref::entry::Entry::Occupied(_) => None,
            dashmap::mapref::entry::Entry::Vacant(v) => {
                let conn = Arc::new(AgentConn {
                    name,
                    id: NEXT_CONN_ID.fetch_add(1, Ordering::Relaxed),
                    send,
                    sessions: DashMap::new(),
                    workspace_requests: DashMap::new(),
                });
                v.insert(conn.clone());
                Some(conn)
            }
        }
    }

    /// Remove the entry only if it still matches `conn`'s id. Tells every
    /// in-flight session / pending workspace request that the agent vanished.
    pub fn unregister(&self, conn: &AgentConn) {
        let should_remove = self
            .agents
            .get(&conn.name)
            .map(|e| e.value().id == conn.id)
            .unwrap_or(false);
        if should_remove {
            self.agents.remove(&conn.name);
        }
        // Drain sessions and signal disconnect.
        let sids: Vec<Uuid> = conn.sessions.iter().map(|e| *e.key()).collect();
        for sid in sids {
            if let Some((_, tx)) = conn.sessions.remove(&sid) {
                let _ = tx.try_send(ClientMsg::SessionClosed {
                    session_id: sid,
                    reason: Some("agent disconnected".into()),
                });
            }
        }
        // Drop all pending workspace request oneshots; receivers wake with
        // RecvError and surface "agent disconnected" themselves.
        conn.workspace_requests.clear();
    }

    pub fn get(&self, name: &str) -> Option<Arc<AgentConn>> {
        self.agents.get(name).map(|e| e.value().clone())
    }
}

impl Default for AgentRegistry {
    fn default() -> Self {
        Self::new()
    }
}

pub struct AgentConn {
    pub name: String,
    id: u64,
    send: mpsc::Sender<ServerMsg>,
    /// Per-session forward queue. Filled by hub session router when a session
    /// opens; drained by handle_frame when agent emits Session* frames.
    sessions: DashMap<Uuid, mpsc::Sender<ClientMsg>>,
    /// One-shot reply slots keyed by request_id for workspace ops.
    workspace_requests: DashMap<Uuid, oneshot::Sender<ClientMsg>>,
}

#[derive(Debug, thiserror::Error)]
pub enum DispatchError {
    #[error("agent disconnected")]
    Disconnected,
}

impl AgentConn {
    /// Send any ServerMsg to the agent over the WS tunnel.
    pub async fn send(&self, msg: ServerMsg) -> Result<(), DispatchError> {
        self.send
            .send(msg)
            .await
            .map_err(|_| DispatchError::Disconnected)
    }

    /// Register a per-session forward channel. Replaces any previous entry.
    pub fn register_session(&self, session_id: Uuid, tx: mpsc::Sender<ClientMsg>) {
        self.sessions.insert(session_id, tx);
    }

    pub fn unregister_session(&self, session_id: Uuid) {
        self.sessions.remove(&session_id);
    }

    /// Register a one-shot reply slot for a workspace op.
    pub fn register_workspace_request(&self, request_id: Uuid, tx: oneshot::Sender<ClientMsg>) {
        self.workspace_requests.insert(request_id, tx);
    }

    /// Apply an incoming agent frame.
    pub async fn handle_frame(&self, frame: ClientMsg) {
        match classify(&frame) {
            Routing::Session(sid) => {
                let tx = self.sessions.get(&sid).map(|e| e.value().clone());
                if let Some(tx) = tx {
                    let _ = tx.send(frame).await;
                } else {
                    tracing::warn!(session = %sid, "no session route for frame; dropping");
                }
            }
            Routing::Workspace(rid) => {
                if let Some((_, tx)) = self.workspace_requests.remove(&rid) {
                    let _ = tx.send(frame);
                } else {
                    tracing::warn!(request = %rid, "no workspace request route");
                }
            }
            Routing::Discard => {}
        }
    }
}

enum Routing {
    Session(Uuid),
    Workspace(Uuid),
    Discard,
}

fn classify(frame: &ClientMsg) -> Routing {
    match frame {
        ClientMsg::SessionOpened { session_id, .. }
        | ClientMsg::SessionTurnStarted { session_id, .. }
        | ClientMsg::SessionEvent { session_id, .. }
        | ClientMsg::SessionTurnEnded { session_id, .. }
        | ClientMsg::SessionWorkspaceSwitched { session_id, .. }
        | ClientMsg::SessionClosed { session_id, .. }
        | ClientMsg::SessionError { session_id, .. } => Routing::Session(*session_id),
        ClientMsg::WorkspaceListResult { request_id, .. }
        | ClientMsg::WorkspaceCreateResult { request_id, .. }
        | ClientMsg::WorkspaceDeleteResult { request_id, .. } => Routing::Workspace(*request_id),
        ClientMsg::Hello { .. } | ClientMsg::Pong => Routing::Discard,
    }
}
