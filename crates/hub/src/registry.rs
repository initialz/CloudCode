use crate::tunnel::{
    pack_pty_frame, unpack_pty_frame, ClientMsg, ServerMsg, TAG_PTY_INPUT, TAG_PTY_OUTPUT,
    TAG_SCREENCAST_FRAME,
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

/// What flows down a viewer's channel to its ws relay loop
/// (`viewer_session::relay_loop`). Pre-P6 this was bare `Bytes` (JPEG frames
/// only) and `ViewerClosed` was signalled by dropping the channel; the
/// multi-target viewer also needs deliverable Text payloads (the targets
/// list), so the channel now carries an explicit enum.
#[derive(Debug)]
pub enum ViewerOut {
    /// A JPEG screencast frame → forwarded as a Binary ws message.
    Frame(Bytes),
    /// Pre-serialized targets JSON (`{"kind":"targets","targets":[…]}`) →
    /// forwarded as a Text ws message.
    Targets(String),
    /// The agent's screencast for this viewer ended → the relay loop breaks.
    Closed(Option<String>),
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
                    viewer_sessions: DashMap::new(),
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
        // Dropping the frame senders closes each viewer's channel, which
        // ends its ws relay loop (the viewer sees the stream stop). No
        // sentinel needed — the receiver observing a closed channel is the
        // signal.
        conn.viewer_sessions.clear();
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
    /// Active browser screencast viewers, keyed by viewer_session_id. Each
    /// `TAG_SCREENCAST_FRAME` binary frame (a raw JPEG), `ViewerTargets`
    /// list, and `ViewerClosed` signal is forwarded to the matching sender
    /// as a [`ViewerOut`]; the viewer ws relay loop holds the receiver.
    viewer_sessions: DashMap<Uuid, mpsc::Sender<ViewerOut>>,
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

    /// Register a viewer sink for `viewer_session_id`. The viewer ws relay
    /// loop (P2 Task 3) holds the receiver and forwards each [`ViewerOut`]
    /// out to the browser/app (Binary for frames, Text for targets).
    pub fn register_viewer(&self, viewer_session_id: Uuid, tx: mpsc::Sender<ViewerOut>) {
        self.viewer_sessions.insert(viewer_session_id, tx);
    }

    pub fn unregister_viewer(&self, viewer_session_id: Uuid) {
        self.viewer_sessions.remove(&viewer_session_id);
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
    ///
    /// MUST NOT block: every `.send()` uses `try_send` so a slow
    /// consumer on one session / download can't stall the per-agent
    /// read loop and freeze all other sessions on the same agent.
    pub fn handle_text_frame(&self, frame: ClientMsg) {
        match classify(&frame) {
            Routing::Session(sid) => {
                let tx = self.sessions.get(&sid).map(|e| e.value().clone());
                if let Some(tx) = tx {
                    if let Err(mpsc::error::TrySendError::Full(_)) =
                        tx.try_send(PtyEventOut::Frame(frame))
                    {
                        tracing::warn!(session = %sid, "session event channel full; dropping frame");
                    }
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
                let terminal = matches!(
                    &frame,
                    ClientMsg::FsReadChunk { eof, error, .. } if *eof || error.is_some()
                );
                let tx = self.fs_read_streams.get(&rid).map(|e| e.value().clone());
                if let Some(tx) = tx {
                    match tx.try_send(frame) {
                        Ok(()) => {}
                        Err(mpsc::error::TrySendError::Full(_)) => {
                            tracing::warn!(request = %rid, "fs read channel full; dropping chunk");
                        }
                        Err(mpsc::error::TrySendError::Closed(_)) => {
                            tracing::debug!(request = %rid, "fs read receiver gone");
                        }
                    }
                    if terminal {
                        self.fs_read_streams.remove(&rid);
                    }
                } else {
                    tracing::warn!(request = %rid, "no fs read stream route");
                    if terminal {
                        self.fs_read_streams.remove(&rid);
                    }
                }
            }
            Routing::Viewer(vid) => match frame {
                // The agent's screencast ended: DELIVER the close (so the
                // relay loop breaks promptly) and drop the route. The
                // agent-disconnect path in `unregister` still uses bare
                // channel-drop as its close signal.
                ClientMsg::ViewerClosed { reason, .. } => {
                    tracing::debug!(viewer = %vid, reason = ?reason, "viewer screencast closed by agent");
                    if let Some((_, tx)) = self.viewer_sessions.remove(&vid) {
                        let _ = tx.try_send(ViewerOut::Closed(reason));
                    }
                }
                // Targets list: a deliverable update, NOT a close signal —
                // the route stays registered. Serialized here once so the
                // relay loop just ships a ready Text frame.
                ClientMsg::ViewerTargets { targets, .. } => {
                    let tx = self.viewer_sessions.get(&vid).map(|e| e.value().clone());
                    if let Some(tx) = tx {
                        let json = crate::viewer_session::targets_wire_json(&targets);
                        if let Err(mpsc::error::TrySendError::Full(_)) =
                            tx.try_send(ViewerOut::Targets(json))
                        {
                            tracing::warn!(viewer = %vid, "viewer channel full; dropping targets update");
                        }
                    } else {
                        tracing::debug!(viewer = %vid, "targets update for unknown viewer");
                    }
                }
                _ => unreachable!("classify() routes only viewer frames here"),
            },
            Routing::Discard => {}
        }
    }

    /// Handle an incoming binary frame from the agent, dispatching on the tag:
    ///
    ///   * `TAG_PTY_OUTPUT` — PTY output, forwarded to the matching session's
    ///     PTY channel (keyed by session_id).
    ///   * `TAG_SCREENCAST_FRAME` — a raw JPEG, forwarded to the matching
    ///     viewer's frame channel (here `sid` is the viewer_session_id).
    ///
    /// Uses `try_send` everywhere — a slow webterm / viewer consumer drops
    /// frames instead of blocking the entire agent read loop.
    pub fn handle_binary_frame(&self, raw: &[u8]) {
        let Some((tag, sid, payload)) = unpack_pty_frame(raw) else {
            tracing::warn!("malformed binary frame from agent");
            return;
        };
        match tag {
            TAG_PTY_OUTPUT => {
                let tx = self.sessions.get(&sid).map(|e| e.value().clone());
                if let Some(tx) = tx {
                    let bytes = Bytes::copy_from_slice(payload);
                    if let Err(mpsc::error::TrySendError::Full(_)) =
                        tx.try_send(PtyEventOut::Output(bytes))
                    {
                        tracing::warn!(session = %sid, "pty output channel full; dropping frame");
                    }
                } else {
                    tracing::trace!(session = %sid, "binary frame for unknown session");
                }
            }
            TAG_SCREENCAST_FRAME => {
                // `sid` is the viewer_session_id for screencast frames.
                let tx = self.viewer_sessions.get(&sid).map(|e| e.value().clone());
                if let Some(tx) = tx {
                    let bytes = Bytes::copy_from_slice(payload);
                    if let Err(mpsc::error::TrySendError::Full(_)) =
                        tx.try_send(ViewerOut::Frame(bytes))
                    {
                        tracing::warn!(viewer = %sid, "viewer frame channel full; dropping frame");
                    }
                } else {
                    tracing::trace!(viewer = %sid, "screencast frame for unknown viewer");
                }
            }
            other => {
                tracing::warn!(tag = other, "unexpected binary tag from agent");
            }
        }
    }
}

enum Routing {
    Session(Uuid),
    Workspace(Uuid),
    /// `FsReadChunk` stream: many frames per request_id, routed via the
    /// mpsc-backed `fs_read_streams` map (oneshot can't carry a stream).
    FsStream(Uuid),
    /// Viewer-keyed frames (`ViewerClosed` / `ViewerTargets`): forwarded to
    /// the viewer's [`ViewerOut`] channel. `ViewerClosed` also drops the
    /// route so the relay loop exits; `ViewerTargets` is a plain delivery.
    Viewer(Uuid),
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
        ClientMsg::FsListResult { request_id, .. }
        | ClientMsg::FsWriteResult { request_id, .. }
        | ClientMsg::FsDeleteResult { request_id, .. } => Routing::Workspace(*request_id),
        ClientMsg::FsReadChunk { request_id, .. } => Routing::FsStream(*request_id),
        ClientMsg::ViewerClosed {
            viewer_session_id, ..
        }
        | ClientMsg::ViewerTargets {
            viewer_session_id, ..
        } => Routing::Viewer(*viewer_session_id),
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
            viewer_sessions: DashMap::new(),
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
        });
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
        });
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
            error: Some("boom".to_string()),
        });
        assert!(!conn.fs_read_streams.contains_key(&rid));
        let got = rx.recv().await.expect("error chunk");
        assert!(matches!(
            got,
            ClientMsg::FsReadChunk { error: Some(ref e), .. } if e == "boom"
        ));
    }

    #[tokio::test]
    async fn screencast_frame_routes_to_viewer() {
        let conn = mk_conn();
        let vid = Uuid::new_v4();
        let (tx, mut rx) = mpsc::channel::<ViewerOut>(8);
        conn.register_viewer(vid, tx);

        // A TAG_SCREENCAST_FRAME keyed by viewer_session_id → viewer channel.
        let jpeg = [0xFFu8, 0xD8, 0x01, 0x02, 0x03];
        let frame = pack_pty_frame(TAG_SCREENCAST_FRAME, vid, &jpeg);
        conn.handle_binary_frame(&frame);

        match rx.recv().await.expect("viewer frame") {
            ViewerOut::Frame(got) => assert_eq!(&got[..], &jpeg[..]),
            other => panic!("expected ViewerOut::Frame, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn pty_output_still_routes_to_session() {
        let conn = mk_conn();
        let sid = Uuid::new_v4();
        let (tx, mut rx) = mpsc::channel::<PtyEventOut>(8);
        conn.register_session(sid, tx);

        let payload = [b'h', b'i'];
        let frame = pack_pty_frame(TAG_PTY_OUTPUT, sid, &payload);
        conn.handle_binary_frame(&frame);

        let got = rx.recv().await.expect("pty output");
        match got {
            PtyEventOut::Output(b) => assert_eq!(&b[..], &payload[..]),
            other => panic!("expected Output, got {other:?}"),
        }
    }

    #[test]
    fn unknown_binary_tag_does_not_panic() {
        let conn = mk_conn();
        let sid = Uuid::new_v4();
        // Unknown tag 0x7F: must be dropped with a warn, no panic.
        let frame = pack_pty_frame(0x7F, sid, &[1, 2, 3]);
        conn.handle_binary_frame(&frame);
        // A frame too short to even unpack: also no panic.
        conn.handle_binary_frame(&[0x03, 0x00]);
    }

    #[tokio::test]
    async fn viewer_closed_delivers_close_and_removes_viewer_route() {
        let conn = mk_conn();
        let vid = Uuid::new_v4();
        let (tx, mut rx) = mpsc::channel::<ViewerOut>(8);
        conn.register_viewer(vid, tx);
        assert!(conn.viewer_sessions.contains_key(&vid));

        conn.handle_text_frame(ClientMsg::ViewerClosed {
            viewer_session_id: vid,
            reason: Some("page closed".into()),
        });
        // The close is DELIVERED (so the relay breaks promptly with the
        // reason in hand), and the route is gone.
        match rx.recv().await.expect("close delivered") {
            ViewerOut::Closed(reason) => assert_eq!(reason.as_deref(), Some("page closed")),
            other => panic!("expected ViewerOut::Closed, got {other:?}"),
        }
        assert!(!conn.viewer_sessions.contains_key(&vid));
    }

    #[tokio::test]
    async fn viewer_targets_delivered_and_route_kept() {
        use crate::tunnel::TargetInfo;

        let conn = mk_conn();
        let vid = Uuid::new_v4();
        let (tx, mut rx) = mpsc::channel::<ViewerOut>(8);
        conn.register_viewer(vid, tx);

        conn.handle_text_frame(ClientMsg::ViewerTargets {
            viewer_session_id: vid,
            targets: vec![TargetInfo {
                id: "T1".into(),
                title: "Example".into(),
                url: "https://example.com/".into(),
                kind: "page".into(),
            }],
        });

        // Delivered as a pre-serialized Text payload…
        match rx.recv().await.expect("targets delivered") {
            ViewerOut::Targets(json) => {
                let v: serde_json::Value = serde_json::from_str(&json).unwrap();
                assert_eq!(v["kind"], "targets");
                assert_eq!(v["targets"][0]["id"], "T1");
            }
            other => panic!("expected ViewerOut::Targets, got {other:?}"),
        }
        // …and crucially NOT treated as a close signal: the route survives.
        assert!(conn.viewer_sessions.contains_key(&vid));
    }
}
