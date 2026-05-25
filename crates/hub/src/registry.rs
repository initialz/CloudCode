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
        agent_version: Option<String>,
        target_triple: Option<String>,
        send: mpsc::Sender<OutgoingFrame>,
    ) -> Option<Arc<AgentConn>> {
        match self.agents.entry(name.clone()) {
            dashmap::mapref::entry::Entry::Occupied(_) => None,
            dashmap::mapref::entry::Entry::Vacant(v) => {
                let conn = Arc::new(AgentConn {
                    name,
                    agent_version,
                    target_triple,
                    id: NEXT_CONN_ID.fetch_add(1, Ordering::Relaxed),
                    send,
                    sessions: DashMap::new(),
                    workspace_requests: DashMap::new(),
                    fs_read_streams: DashMap::new(),
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
        // Drain any in-flight FsRead streams: send a terminal error
        // chunk so the HTTP handler closes its response body promptly
        // (instead of hanging on a 30s idle timeout).
        let rids: Vec<Uuid> = conn.fs_read_streams.iter().map(|e| *e.key()).collect();
        for rid in rids {
            if let Some((_, tx)) = conn.fs_read_streams.remove(&rid) {
                let _ = tx.try_send(ClientMsg::FsReadChunk {
                    request_id: rid,
                    data_b64: String::new(),
                    eof: true,
                    error: Some("agent disconnected".into()),
                });
            }
        }
    }

    pub fn get(&self, name: &str) -> Option<Arc<AgentConn>> {
        self.agents.get(name).map(|e| e.value().clone())
    }

    pub fn list_active(&self) -> Vec<String> {
        self.agents.iter().map(|e| e.key().clone()).collect()
    }

    /// Snapshot of every currently-connected agent. Used by the admin
    /// workspaces endpoint to fan out a `WorkspaceListAll` request.
    pub fn list_conns(&self) -> Vec<Arc<AgentConn>> {
        self.agents.iter().map(|e| e.value().clone()).collect()
    }
}

impl Default for AgentRegistry {
    fn default() -> Self {
        Self::new()
    }
}

pub struct AgentConn {
    pub name: String,
    /// Self-reported agent build version from the hello frame
    /// (`CARGO_PKG_VERSION`), if the agent is new enough to send it.
    pub agent_version: Option<String>,
    /// Rust target triple of the agent binary, used to pick the right
    /// release asset on self-update.
    pub target_triple: Option<String>,
    id: u64,
    send: mpsc::Sender<OutgoingFrame>,
    /// Active PTY sessions hosted by this agent, keyed by session_id.
    sessions: DashMap<Uuid, mpsc::Sender<PtyEventOut>>,
    /// One-shot reply slots for workspace_list / create / delete / update by request_id.
    workspace_requests: DashMap<Uuid, oneshot::Sender<ClientMsg>>,
    /// Streaming reply slots for `FsRead`: many `FsReadChunk` frames per
    /// request_id until one arrives with `eof = true` (or an `error`).
    /// Distinct from `workspace_requests` because oneshot can only carry
    /// a single frame.
    fs_read_streams: DashMap<Uuid, mpsc::Sender<ClientMsg>>,
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

    /// Register an mpsc sink that will receive every `FsReadChunk`
    /// frame for `request_id` until the agent emits `eof = true`
    /// (or an `error`). The hub's HTTP download handler holds the
    /// matching receiver and decodes chunks as they arrive.
    pub fn register_fs_read_stream(&self, request_id: Uuid, tx: mpsc::Sender<ClientMsg>) {
        self.fs_read_streams.insert(request_id, tx);
    }

    /// Best-effort cleanup of the stream entry. Idempotent — safe to
    /// call from the HTTP handler's drop path even if the routing
    /// layer already removed it on EOF.
    pub fn unregister_fs_read_stream(&self, request_id: Uuid) {
        self.fs_read_streams.remove(&request_id);
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
            Routing::FsStream(rid) => {
                // Inspect terminal-ness BEFORE forwarding so we can
                // remove the routing entry first — the receiver may
                // drop its handle on EOF and we don't want a racing
                // chunk to log "no route" spuriously.
                let terminal = matches!(
                    &frame,
                    ClientMsg::FsReadChunk { eof, error, .. } if *eof || error.is_some()
                );
                let tx = self.fs_read_streams.get(&rid).map(|e| e.value().clone());
                if let Some(tx) = tx {
                    // Backpressure: if the HTTP client is slow, this
                    // await blocks the per-agent frame loop. That is
                    // intentional — the agent's chunk emitter is
                    // similarly bounded so file streaming applies
                    // end-to-end backpressure.
                    let _ = tx.send(frame).await;
                    if terminal {
                        self.fs_read_streams.remove(&rid);
                    }
                } else {
                    tracing::warn!(request = %rid, "no fs read stream route");
                    if terminal {
                        // Already removed; ensure idempotency.
                        self.fs_read_streams.remove(&rid);
                    }
                }
            }
            Routing::Discard => {
                tracing::debug!("discarding unroutable frame");
            }
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
    /// `FsReadChunk` stream: many frames per request_id, routed via the
    /// mpsc-backed `fs_read_streams` map (oneshot can't carry a stream).
    FsStream(Uuid),
    Discard,
}

fn classify(frame: &ClientMsg) -> Routing {
    match frame {
        ClientMsg::PtyOpened { session_id, .. }
        | ClientMsg::PtyClosed { session_id, .. }
        | ClientMsg::PtyError { session_id, .. }
        | ClientMsg::SplitPaneResult { session_id, .. } => Routing::Session(*session_id),
        ClientMsg::WorkspaceListResult { request_id, .. }
        | ClientMsg::WorkspaceCreateResult { request_id, .. }
        | ClientMsg::WorkspaceDeleteResult { request_id, .. }
        | ClientMsg::WorkspaceResetResult { request_id, .. }
        | ClientMsg::WorkspaceListAllResult { request_id, .. }
        | ClientMsg::UpdateAgentResult { request_id, .. } => Routing::Workspace(*request_id),
        ClientMsg::FsListResult { request_id, .. } => Routing::Workspace(*request_id),
        ClientMsg::FsReadChunk { request_id, .. } => Routing::FsStream(*request_id),
        ClientMsg::Hello { .. }
        | ClientMsg::Pong
        | ClientMsg::Message { .. }
        | ClientMsg::UserInteraction { .. } => {
            // Message + UserInteraction frames are intercepted upstream
            // in ws_handler and persisted to the admin db directly —
            // they never reach here under normal operation. Discard
            // defensively.
            Routing::Discard
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mk_conn() -> Arc<AgentConn> {
        // Drop the send half — we never exercise it in these unit
        // tests; the receiver going away just means
        // `AgentConn::send` would fail, which we don't call.
        let (send, _rx) = mpsc::channel(1);
        Arc::new(AgentConn {
            name: "test".into(),
            agent_version: None,
            target_triple: None,
            id: 0,
            send,
            sessions: DashMap::new(),
            workspace_requests: DashMap::new(),
            fs_read_streams: DashMap::new(),
        })
    }

    #[tokio::test]
    async fn fs_stream_eof_removes_route() {
        let conn = mk_conn();
        let rid = Uuid::new_v4();
        let (tx, mut rx) = mpsc::channel(8);
        conn.register_fs_read_stream(rid, tx);
        assert!(conn.fs_read_streams.contains_key(&rid));

        // First chunk: not terminal — entry must remain.
        conn.handle_text_frame(ClientMsg::FsReadChunk {
            request_id: rid,
            data_b64: "aGVsbG8=".into(),
            eof: false,
            error: None,
        })
        .await;
        assert!(conn.fs_read_streams.contains_key(&rid));
        let got = rx.recv().await.expect("first chunk");
        assert!(matches!(
            got,
            ClientMsg::FsReadChunk { eof: false, ref data_b64, .. } if data_b64 == "aGVsbG8="
        ));

        // Terminal chunk: entry must be removed.
        conn.handle_text_frame(ClientMsg::FsReadChunk {
            request_id: rid,
            data_b64: String::new(),
            eof: true,
            error: None,
        })
        .await;
        assert!(!conn.fs_read_streams.contains_key(&rid));
        let got = rx.recv().await.expect("eof chunk");
        assert!(matches!(got, ClientMsg::FsReadChunk { eof: true, .. }));
    }

    #[tokio::test]
    async fn fs_stream_error_removes_route() {
        let conn = mk_conn();
        let rid = Uuid::new_v4();
        let (tx, mut rx) = mpsc::channel(8);
        conn.register_fs_read_stream(rid, tx);

        conn.handle_text_frame(ClientMsg::FsReadChunk {
            request_id: rid,
            data_b64: String::new(),
            eof: true,
            error: Some("boom".into()),
        })
        .await;
        assert!(!conn.fs_read_streams.contains_key(&rid));
        let got = rx.recv().await.expect("error chunk");
        assert!(matches!(
            got,
            ClientMsg::FsReadChunk { error: Some(ref e), .. } if e == "boom"
        ));
    }
}
