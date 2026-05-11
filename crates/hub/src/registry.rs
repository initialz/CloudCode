use crate::tunnel::{
    pack_pty_frame, unpack_pty_frame, ClientMsg, ServerMsg, TAG_PTY_INPUT, TAG_PTY_OUTPUT,
};
use bytes::Bytes;
use dashmap::DashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use tokio::sync::{mpsc, oneshot};
use uuid::Uuid;

static NEXT_CONN_ID: AtomicU64 = AtomicU64::new(1);

/// Wraps a frame destined for the agent over the WS tunnel.
#[derive(Debug)]
pub enum OutgoingFrame {
    Text(ServerMsg),
    Binary(Vec<u8>),
}

/// Per-PTY-session events that the hub session router consumes.
#[derive(Debug)]
pub enum PtyEventOut {
    /// Forwarded text frame from the agent (PtyOpened / PtyClosed / PtyError).
    Frame(ClientMsg),
    /// PTY output payload (already de-prefixed from the binary frame).
    Output(Bytes),
}

pub struct AgentRegistry {
    agents: DashMap<String, Arc<AgentConn>>,
}

impl AgentRegistry {
    pub fn new() -> Self {
        Self {
            agents: DashMap::new(),
        }
    }

    pub fn try_register(
        self: &Arc<Self>,
        name: String,
        send: mpsc::Sender<OutgoingFrame>,
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

    pub fn unregister(&self, conn: &AgentConn) {
        let should_remove = self
            .agents
            .get(&conn.name)
            .map(|e| e.value().id == conn.id)
            .unwrap_or(false);
        if should_remove {
            self.agents.remove(&conn.name);
        }
        let sids: Vec<Uuid> = conn.sessions.iter().map(|e| *e.key()).collect();
        for sid in sids {
            if let Some((_, tx)) = conn.sessions.remove(&sid) {
                let _ = tx.try_send(PtyEventOut::Frame(ClientMsg::PtyClosed {
                    session_id: sid,
                    reason: Some("agent disconnected".into()),
                }));
            }
        }
        conn.workspace_requests.clear();
    }

    pub fn get(&self, name: &str) -> Option<Arc<AgentConn>> {
        self.agents.get(name).map(|e| e.value().clone())
    }

    pub fn list_active(&self) -> Vec<String> {
        self.agents.iter().map(|e| e.key().clone()).collect()
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
    send: mpsc::Sender<OutgoingFrame>,
    /// Active PTY sessions hosted by this agent, keyed by session_id.
    sessions: DashMap<Uuid, mpsc::Sender<PtyEventOut>>,
    /// One-shot reply slots for workspace_list / create / delete by request_id.
    workspace_requests: DashMap<Uuid, oneshot::Sender<ClientMsg>>,
}

#[derive(Debug, thiserror::Error)]
pub enum DispatchError {
    #[error("agent disconnected")]
    Disconnected,
}

impl AgentConn {
    pub async fn send(&self, msg: ServerMsg) -> Result<(), DispatchError> {
        self.send
            .send(OutgoingFrame::Text(msg))
            .await
            .map_err(|_| DispatchError::Disconnected)
    }

    /// Pack and send a TAG_PTY_INPUT binary frame for `session_id`.
    pub async fn send_pty_input(
        &self,
        session_id: Uuid,
        payload: &[u8],
    ) -> Result<(), DispatchError> {
        let frame = pack_pty_frame(TAG_PTY_INPUT, session_id, payload);
        self.send
            .send(OutgoingFrame::Binary(frame))
            .await
            .map_err(|_| DispatchError::Disconnected)
    }

    pub fn register_session(&self, session_id: Uuid, tx: mpsc::Sender<PtyEventOut>) {
        self.sessions.insert(session_id, tx);
    }

    pub fn unregister_session(&self, session_id: Uuid) {
        self.sessions.remove(&session_id);
    }

    pub fn register_workspace_request(&self, request_id: Uuid, tx: oneshot::Sender<ClientMsg>) {
        self.workspace_requests.insert(request_id, tx);
    }

    /// Handle an incoming text JSON frame from the agent.
    pub async fn handle_text_frame(&self, frame: ClientMsg) {
        match classify(&frame) {
            Routing::Session(sid) => {
                let tx = self.sessions.get(&sid).map(|e| e.value().clone());
                if let Some(tx) = tx {
                    let _ = tx.send(PtyEventOut::Frame(frame)).await;
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

    /// Handle an incoming binary frame from the agent. Only TAG_PTY_OUTPUT is
    /// expected; payload is forwarded to the matching session's PTY channel.
    pub async fn handle_binary_frame(&self, raw: &[u8]) {
        let Some((tag, sid, payload)) = unpack_pty_frame(raw) else {
            tracing::warn!("malformed binary frame from agent");
            return;
        };
        if tag != TAG_PTY_OUTPUT {
            tracing::warn!(tag, "unexpected binary tag from agent");
            return;
        }
        let tx = self.sessions.get(&sid).map(|e| e.value().clone());
        if let Some(tx) = tx {
            let bytes = Bytes::copy_from_slice(payload);
            let _ = tx.send(PtyEventOut::Output(bytes)).await;
        } else {
            tracing::trace!(session = %sid, "binary frame for unknown session");
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
        ClientMsg::PtyOpened { session_id, .. }
        | ClientMsg::PtyClosed { session_id, .. }
        | ClientMsg::PtyError { session_id, .. } => Routing::Session(*session_id),
        ClientMsg::WorkspaceListResult { request_id, .. }
        | ClientMsg::WorkspaceCreateResult { request_id, .. }
        | ClientMsg::WorkspaceDeleteResult { request_id, .. } => Routing::Workspace(*request_id),
        ClientMsg::Hello { .. } | ClientMsg::Pong => Routing::Discard,
    }
}
