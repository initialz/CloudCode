//! Resident localhost MCP HTTP/SSE endpoint. claude (the MCP client)
//! connects here; frames are tunneled to the bound CloudCode client
//! over the existing agent<->hub ws as BrowserRpc.

use dashmap::DashMap;
use std::sync::Arc;
use tokio::sync::mpsc;
use uuid::Uuid;

/// Per-session routing: maps a claude-facing token to its session_id
/// and the channel that delivers subprocess responses back to claude.
#[allow(dead_code)]
pub struct SessionRoute {
    pub session_id: Uuid,
    pub to_claude: mpsc::Sender<String>,
}

#[derive(Clone)]
pub struct EndpointState {
    routes: Arc<DashMap<String, SessionRoute>>,
    /// Set once the agent ws is up: lets the endpoint emit ClientMsg
    /// frames toward the hub.
    to_hub: Arc<tokio::sync::RwLock<Option<mpsc::Sender<crate::pty::OutFrame>>>>,
}

impl Default for EndpointState {
    fn default() -> Self {
        Self::new()
    }
}

impl EndpointState {
    pub fn new() -> Self {
        Self {
            routes: Arc::new(DashMap::new()),
            to_hub: Arc::new(tokio::sync::RwLock::new(None)),
        }
    }

    #[allow(dead_code)]
    pub fn register(&self, token: String, session_id: Uuid, to_claude: mpsc::Sender<String>) {
        self.routes
            .insert(token, SessionRoute { session_id, to_claude });
    }

    #[allow(dead_code)]
    pub fn unregister(&self, token: &str) {
        self.routes.remove(token);
    }

    #[allow(dead_code)]
    pub fn session_for(&self, token: &str) -> Option<Uuid> {
        self.routes.get(token).map(|r| r.session_id)
    }

    /// Deliver a response frame (from hub/client) to claude's stream for
    /// the given session_id. Returns false if no live route.
    #[allow(dead_code)]
    pub async fn deliver_to_claude(&self, session_id: Uuid, payload: String) -> bool {
        // Collect the sender first to avoid holding the DashMap guard across await.
        let target = self
            .routes
            .iter()
            .find(|e| e.session_id == session_id)
            .map(|e| e.to_claude.clone());
        if let Some(tx) = target {
            return tx.send(payload).await.is_ok();
        }
        false
    }

    #[allow(dead_code)]
    pub async fn set_hub_sender(&self, tx: mpsc::Sender<crate::pty::OutFrame>) {
        *self.to_hub.write().await = Some(tx);
    }

    #[allow(dead_code)]
    pub async fn send_to_hub(&self, frame: crate::pty::OutFrame) {
        if let Some(tx) = self.to_hub.read().await.as_ref() {
            let _ = tx.send(frame).await;
        }
    }
}

/// Bind the localhost MCP listener. Real MCP routes are added in Task 10;
/// for now only `/healthz` exists so we can verify the listener is up.
pub async fn serve(state: EndpointState, port: u16) -> std::io::Result<()> {
    use axum::routing::get;
    let app = axum::Router::new()
        .route("/healthz", get(|| async { "ok" }))
        .with_state(state);
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", port)).await?;
    axum::serve(listener, app)
        .await
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn endpoint_state_registers_and_resolves_token() {
        let state = EndpointState::new();
        let sid = Uuid::new_v4();
        let (tx, _rx) = mpsc::channel(4);
        state.register("tok-abc".into(), sid, tx);
        assert_eq!(state.session_for("tok-abc"), Some(sid));
        assert_eq!(state.session_for("nope"), None);
        state.unregister("tok-abc");
        assert_eq!(state.session_for("tok-abc"), None);
    }
}
