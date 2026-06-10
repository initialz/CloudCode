//! Resident localhost MCP HTTP endpoint. claude (the MCP client)
//! connects here; frames are tunneled to the bound CloudCode client
//! over the existing agent<->hub ws as BrowserRpc.
//!
//! Transport: Streamable HTTP with POST-blocking. claude POSTs a
//! JSON-RPC request; the endpoint forwards it to the bound client over
//! the hub, BLOCKS until the matching response comes back (correlated
//! by JSON-RPC `id`), then returns that response as the POST response
//! body. JSON-RPC notifications (no `id`) are forwarded and get an
//! immediate 202 with no body.
//!
//! The endpoint is a DUMB RELAY — it does NOT implement MCP semantics
//! (initialize handshake, tool schemas, etc.). Those flow end-to-end
//! between claude and the client's subprocess. The endpoint only:
//! routes by token→session_id, correlates request/response by JSON-RPC
//! `id`, and tunnels opaque JSON text.

use dashmap::DashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{mpsc, oneshot, RwLock};
use uuid::Uuid;

use crate::pty::OutFrame;
use crate::tunnel::ClientMsg;

/// Key correlating an in-flight claude request to its eventual response:
/// (session_id, json-rpc id rendered canonically).
type PendingKey = (Uuid, String);

#[derive(Clone)]
pub struct EndpointState {
    /// claude-facing token -> session_id.
    routes: Arc<DashMap<String, Uuid>>,
    /// In-flight POST requests awaiting their response, by (session, id).
    pending: Arc<DashMap<PendingKey, oneshot::Sender<String>>>,
    /// Set once the agent ws is up: lets the endpoint emit frames to the hub.
    to_hub: Arc<RwLock<Option<mpsc::Sender<OutFrame>>>>,
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
            pending: Arc::new(DashMap::new()),
            to_hub: Arc::new(RwLock::new(None)),
        }
    }

    /// Reserve a session route for a claude token (used at session open, Task 11).
    pub fn register(&self, token: String, session_id: Uuid) {
        self.routes.insert(token, session_id);
    }

    /// Reserve a session route for a not-yet-connected claude. Generates a
    /// random token, maps it to `session_id`, and returns the token.
    pub fn reserve(&self, session_id: Uuid) -> String {
        let token = Uuid::new_v4().simple().to_string();
        self.register(token.clone(), session_id);
        token
    }

    pub fn unregister(&self, token: &str) {
        self.routes.remove(token);
    }

    pub fn session_for(&self, token: &str) -> Option<Uuid> {
        self.routes.get(token).map(|r| *r.value())
    }

    pub async fn set_hub_sender(&self, tx: mpsc::Sender<OutFrame>) {
        *self.to_hub.write().await = Some(tx);
    }

    async fn send_to_hub(&self, frame: OutFrame) {
        // Clone the sender out, then drop the read guard BEFORE awaiting the
        // send — otherwise a concurrent reconnect taking the write lock in
        // set_hub_sender could be blocked behind an in-flight send.
        let tx = self.to_hub.read().await.as_ref().cloned();
        if let Some(tx) = tx {
            let _ = tx.send(frame).await;
        }
    }

    /// Resolve the pending request matching this response frame's id.
    /// Called from ws.rs when a ServerMsg::BrowserRpc arrives. Returns
    /// true if it matched an in-flight request; false otherwise (e.g. a
    /// server-initiated notification, which M1 drops).
    pub fn resolve_response(&self, session_id: Uuid, payload: String) -> bool {
        let id = extract_id_key(&payload);
        tracing::debug!(
            %session_id,
            has_id = id.is_some(),
            "browser MCP response from hub; looking for pending waiter"
        );
        let Some(id) = id else {
            return false;
        };
        if let Some((_, tx)) = self.pending.remove(&(session_id, id)) {
            let matched = tx.send(payload).is_ok();
            tracing::debug!(%session_id, matched, "browser MCP pending waiter resolved");
            return matched;
        }
        tracing::debug!(%session_id, "browser MCP response had no pending waiter");
        false
    }
}

/// Timeout for a blocking claude POST awaiting its response from the
/// client subprocess (via the hub). Factored out so tests can reason
/// about the timeout branch without blocking for the full duration.
/// Kept below claude's own 30s MCP connection timeout so our JSON-RPC
/// error reaches claude instead of racing its client-side timeout.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(25);

/// Build a JSON-RPC error response body for a given request id (raw id
/// string as captured by extract_id_key, e.g. "1" or "\"abc\"").
fn jsonrpc_error(id_raw: &str, code: i64, message: &str) -> String {
    format!(
        r#"{{"jsonrpc":"2.0","id":{id_raw},"error":{{"code":{code},"message":{msg}}}}}"#,
        msg = serde_json::to_string(message).unwrap_or_else(|_| "\"error\"".to_string())
    )
}

/// First ~8 chars of a token, for non-secret-leaking diagnostics.
fn token_prefix(token: &str) -> &str {
    let end = token.char_indices().nth(8).map(|(i, _)| i).unwrap_or(token.len());
    &token[..end]
}

/// Extract the JSON-RPC `id` from a frame as a canonical string key.
/// Returns None for notifications (no id) or unparseable bodies.
fn extract_id_key(body: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(body).ok()?;
    match v.get("id") {
        Some(serde_json::Value::Null) | None => None,
        Some(id) => Some(id.to_string()), // numbers -> "1", strings -> "\"abc\""
    }
}

/// Build the `--mcp-config` JSON claude should load for this session,
/// pointing at the resident endpoint via Streamable HTTP.
pub fn mcp_config_json(port: u16, token: &str) -> String {
    format!(
        r#"{{"mcpServers":{{"cc-browser":{{"type":"http","url":"http://127.0.0.1:{port}/mcp/{token}"}}}}}}"#
    )
}

/// Outcome of a claude POST. Mapped to HTTP status in the axum handler.
///
/// Transport-level problems for a JSON-RPC *request* (unknown token,
/// timeout) are returned as `Response` carrying a JSON-RPC error object at
/// HTTP 200 — NOT a bare non-2xx status. Claude Code treats ANY non-2xx on
/// the MCP POST as "authentication required" and kicks off an OAuth
/// discovery cascade that 404s and surfaces as a misleading
/// `SDK auth failed: HTTP 404`. Returning 200 + a JSON-RPC error lets the
/// real error through.
pub enum PostOutcome {
    /// A JSON-RPC body to return (application/json, 200). Either a real
    /// response forwarded from the client, or a JSON-RPC error object built
    /// here for a transport-level failure.
    Response(String),
    /// Notification accepted, no body (202).
    Accepted,
}

/// Core POST handler, factored out for unit testing.
pub async fn handle_post(token: &str, body: String, state: &EndpointState) -> PostOutcome {
    let id = extract_id_key(&body);
    let session = state.session_for(token);
    tracing::debug!(
        token = %token_prefix(token),
        is_request = id.is_some(),
        session = ?session,
        "browser MCP POST"
    );

    match (id, session) {
        // Request to an unknown token: return a JSON-RPC error at HTTP 200
        // instead of 404, so claude shows the real error (not an OAuth
        // misfire).
        (Some(id), None) => {
            tracing::warn!(token = %token_prefix(token), "browser MCP POST for unknown token");
            PostOutcome::Response(jsonrpc_error(
                &id,
                -32001,
                "browser MCP session not registered (token unknown or expired)",
            ))
        }
        // Request to a known session: forward and block for the response.
        (Some(id), Some(session_id)) => {
            let (tx, rx) = oneshot::channel();
            state.pending.insert((session_id, id.clone()), tx);
            state
                .send_to_hub(OutFrame::Text(ClientMsg::BrowserRpc {
                    session_id,
                    payload: body,
                }))
                .await;
            match tokio::time::timeout(REQUEST_TIMEOUT, rx).await {
                Ok(Ok(resp)) => PostOutcome::Response(resp),
                _ => {
                    state.pending.remove(&(session_id, id.clone()));
                    tracing::warn!(
                        token = %token_prefix(token),
                        %session_id,
                        "browser MCP request timed out awaiting client response"
                    );
                    PostOutcome::Response(jsonrpc_error(
                        &id,
                        -32000,
                        "browser MCP request timed out (client subprocess not responding)",
                    ))
                }
            }
        }
        // Notification to an unknown token: nothing to deliver and nothing
        // to return. Don't 404 a notification — just log and accept.
        (None, None) => {
            tracing::warn!(
                token = %token_prefix(token),
                "browser MCP notification for unknown token; dropping"
            );
            PostOutcome::Accepted
        }
        // Notification to a known session: forward, no response expected.
        (None, Some(session_id)) => {
            state
                .send_to_hub(OutFrame::Text(ClientMsg::BrowserRpc {
                    session_id,
                    payload: body,
                }))
                .await;
            PostOutcome::Accepted
        }
    }
}

/// Bind the localhost MCP listener. POST `/mcp/:token` is the
/// POST-blocking JSON-RPC relay; GET on the same path is rejected
/// (M1 does not support server-initiated SSE). `/healthz` stays so we
/// can verify the listener is up.
pub async fn serve(state: EndpointState, port: u16) -> std::io::Result<()> {
    use axum::extract::{Path, State};
    use axum::http::StatusCode;
    use axum::response::IntoResponse;
    use axum::routing::{get, post};

    let app = axum::Router::new()
        .route("/healthz", get(|| async { "ok" }))
        .route(
            "/mcp/:token",
            post(
                |Path(token): Path<String>, State(st): State<EndpointState>, body: String| async move {
                    match handle_post(&token, body, &st).await {
                        PostOutcome::Response(b) => (
                            [(axum::http::header::CONTENT_TYPE, "application/json")],
                            b,
                        )
                            .into_response(),
                        PostOutcome::Accepted => StatusCode::ACCEPTED.into_response(),
                    }
                },
            )
            // M1 does not support server-initiated SSE; reject GET cleanly.
            .get(|| async { StatusCode::METHOD_NOT_ALLOWED }),
        )
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(("127.0.0.1", port)).await?;
    tracing::info!(port, "browser MCP endpoint listening on 127.0.0.1");
    axum::serve(listener, app)
        .await
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn unknown_token_is_rejected() {
        // A REQUEST (has an id) to an unknown token must NOT be a bare
        // non-2xx (which would trigger claude's OAuth misfire). It returns
        // a JSON-RPC error body at HTTP 200.
        let state = EndpointState::new();
        let out = handle_post(
            "nope",
            r#"{"jsonrpc":"2.0","id":1,"method":"x"}"#.to_string(),
            &state,
        )
        .await;
        match out {
            PostOutcome::Response(body) => {
                assert!(body.contains("\"error\""), "carries an error object: {body}");
                assert!(body.contains("-32001"), "uses the unknown-token code: {body}");
                assert!(body.contains("\"id\":1"), "keyed to the request id: {body}");
                let v: serde_json::Value = serde_json::from_str(&body).expect("valid JSON");
                assert_eq!(v["jsonrpc"], "2.0");
            }
            _ => panic!("expected a Response carrying a JSON-RPC error"),
        }
    }

    #[tokio::test]
    async fn notification_to_unknown_token_is_accepted_not_404() {
        // A NOTIFICATION (no id) to an unknown token has nothing to return;
        // accept it (202) rather than 404.
        let state = EndpointState::new();
        let out = handle_post(
            "nope",
            r#"{"jsonrpc":"2.0","method":"notify"}"#.to_string(),
            &state,
        )
        .await;
        assert!(matches!(out, PostOutcome::Accepted));
    }

    #[test]
    fn jsonrpc_error_has_valid_shape() {
        // The timeout branch builds its body via jsonrpc_error; exercise it
        // directly so we don't need a 30s-blocking test for the timeout path.
        let body = jsonrpc_error("1", -32000, "browser MCP request timed out");
        let v: serde_json::Value = serde_json::from_str(&body).expect("valid JSON");
        assert_eq!(v["jsonrpc"], "2.0");
        assert_eq!(v["id"], 1);
        assert_eq!(v["error"]["code"], -32000);
        assert_eq!(v["error"]["message"], "browser MCP request timed out");

        // String ids round-trip too (id_raw is already a canonical token).
        let body = jsonrpc_error("\"abc\"", -32001, "x");
        let v: serde_json::Value = serde_json::from_str(&body).expect("valid JSON");
        assert_eq!(v["id"], "abc");

        // A message containing JSON-breaking chars stays valid (escaped).
        let body = jsonrpc_error("1", -1, "has \"quotes\" and \\ backslash");
        let v: serde_json::Value = serde_json::from_str(&body).expect("valid JSON");
        assert_eq!(v["error"]["message"], "has \"quotes\" and \\ backslash");
    }

    #[tokio::test]
    async fn timeout_returns_jsonrpc_error_not_http_error() {
        // We can't afford a real 30s wait. Instead verify the two facts that
        // make the timeout path correct without blocking: (1) the unknown-
        // token request path (instant) returns a JSON-RPC error rather than
        // a bare non-2xx, and (2) jsonrpc_error with the timeout code/message
        // builds a well-formed JSON-RPC error. Together these cover the shape
        // the timeout branch emits.
        let state = EndpointState::new();
        let out = handle_post(
            "nope",
            r#"{"jsonrpc":"2.0","id":9,"method":"tools/call"}"#.to_string(),
            &state,
        )
        .await;
        assert!(
            matches!(&out, PostOutcome::Response(b) if b.contains("\"error\"")),
            "request transport failures must be JSON-RPC errors, never bare non-2xx"
        );

        let body = jsonrpc_error(
            "9",
            -32000,
            "browser MCP request timed out (client subprocess not responding)",
        );
        let v: serde_json::Value = serde_json::from_str(&body).expect("valid JSON");
        assert_eq!(v["id"], 9);
        assert_eq!(v["error"]["code"], -32000);
    }

    #[tokio::test]
    async fn notification_is_accepted_without_response() {
        let state = EndpointState::new();
        let sid = Uuid::new_v4();
        state.register("t".into(), sid);
        let (hub_tx, mut hub_rx) = mpsc::channel(4);
        state.set_hub_sender(hub_tx).await;
        // notification: no id
        let out = handle_post(
            "t",
            r#"{"jsonrpc":"2.0","method":"notify"}"#.to_string(),
            &state,
        )
        .await;
        assert!(matches!(out, PostOutcome::Accepted));
        // it was still forwarded to the hub
        let f = hub_rx.recv().await.expect("forwarded");
        match f {
            OutFrame::Text(ClientMsg::BrowserRpc { session_id, .. }) => assert_eq!(session_id, sid),
            _ => panic!(),
        }
    }

    #[tokio::test]
    async fn request_blocks_then_resolves_on_matching_response() {
        let state = EndpointState::new();
        let sid = Uuid::new_v4();
        state.register("t".into(), sid);
        let (hub_tx, mut hub_rx) = mpsc::channel(4);
        state.set_hub_sender(hub_tx).await;

        let st2 = state.clone();
        let poster = tokio::spawn(async move {
            handle_post(
                "t",
                r#"{"jsonrpc":"2.0","id":42,"method":"tools/list"}"#.to_string(),
                &st2,
            )
            .await
        });

        // the request was forwarded to the hub
        let fwd = hub_rx.recv().await.expect("forwarded to hub");
        match fwd {
            OutFrame::Text(ClientMsg::BrowserRpc { session_id, .. }) => assert_eq!(session_id, sid),
            _ => panic!(),
        }

        // simulate the response coming back from the client via ws
        let resolved = state.resolve_response(
            sid,
            r#"{"jsonrpc":"2.0","id":42,"result":{"tools":[]}}"#.to_string(),
        );
        assert!(resolved);

        let outcome = poster.await.unwrap();
        match outcome {
            PostOutcome::Response(b) => assert!(b.contains("\"id\":42") && b.contains("tools")),
            _ => panic!("expected a Response"),
        }
    }

    #[test]
    fn config_has_http_url_with_token() {
        let s = mcp_config_json(7110, "abc123");
        assert!(s.contains("\"type\":\"http\""));
        assert!(s.contains("http://127.0.0.1:7110/mcp/abc123"));
        let _: serde_json::Value = serde_json::from_str(&s).unwrap(); // valid JSON
    }

    #[test]
    fn reserve_maps_token_to_session() {
        let st = EndpointState::new();
        let sid = uuid::Uuid::new_v4();
        let tok = st.reserve(sid);
        assert_eq!(st.session_for(&tok), Some(sid));
    }

    #[test]
    fn unregister_clears_route() {
        let st = EndpointState::new();
        let sid = uuid::Uuid::new_v4();
        let tok = st.reserve(sid);
        assert_eq!(st.session_for(&tok), Some(sid));
        st.unregister(&tok);
        assert_eq!(st.session_for(&tok), None);
    }

    #[tokio::test]
    async fn id_key_distinguishes_number_and_string() {
        assert_eq!(extract_id_key(r#"{"id":1}"#), Some("1".to_string()));
        assert_eq!(extract_id_key(r#"{"id":"a"}"#), Some("\"a\"".to_string()));
        assert_eq!(extract_id_key(r#"{"method":"x"}"#), None);
        assert_eq!(extract_id_key(r#"{"id":null}"#), None);
    }

    /// Grab a currently-free localhost TCP port by binding to :0, reading the
    /// assigned port, then dropping the listener so `serve` can rebind it.
    /// There's a tiny TOCTOU window, but it's vanishingly unlikely to matter
    /// for a single test process and is far more robust than a fixed port.
    fn free_port() -> u16 {
        let l = std::net::TcpListener::bind("127.0.0.1:0").expect("bind :0");
        l.local_addr().expect("local_addr").port()
    }

    /// Wait until `serve` has bound and `/healthz` answers, retrying on
    /// connection-refused. Returns the body once it's up.
    async fn wait_healthz(client: &reqwest::Client, base: &str) -> String {
        for _ in 0..50 {
            match client.get(format!("{base}/healthz")).send().await {
                Ok(resp) => return resp.text().await.unwrap(),
                Err(_) => tokio::time::sleep(Duration::from_millis(20)).await,
            }
        }
        panic!("endpoint never came up on {base}");
    }

    /// End-to-end over REAL HTTP: start the actual axum endpoint on a free
    /// localhost port, simulate the hub+client round-trip via the `to_hub`
    /// channel + `resolve_response`, and drive the whole thing with a real
    /// reqwest POST. This is the only test that exercises axum routing and a
    /// live TCP socket (the others call `handle_post` directly).
    #[tokio::test]
    async fn real_http_post_roundtrips_via_endpoint() {
        let state = EndpointState::new();
        let sid = Uuid::new_v4();
        let token = "tok-e2e";
        state.register(token.into(), sid);

        let (hub_tx, mut hub_rx) = mpsc::channel(4);
        state.set_hub_sender(hub_tx).await;

        let port = free_port();
        let serve_state = state.clone();
        tokio::spawn(async move {
            let _ = serve(serve_state, port).await;
        });

        // Simulate the client+hub: take the forwarded frame off `to_hub` and
        // feed back a matching JSON-RPC response (same id) via resolve_response.
        let resp_state = state.clone();
        tokio::spawn(async move {
            if let Some(OutFrame::Text(ClientMsg::BrowserRpc { session_id, payload })) =
                hub_rx.recv().await
            {
                assert_eq!(session_id, sid);
                // echo the id back in a synthetic tools/list result
                let id = extract_id_key(&payload).expect("request had an id");
                let body = format!(
                    r#"{{"jsonrpc":"2.0","id":{id},"result":{{"tools":[{{"name":"echo"}}]}}}}"#
                );
                assert!(resp_state.resolve_response(session_id, body));
            }
        });

        let client = reqwest::Client::new();
        let base = format!("http://127.0.0.1:{port}");

        // serve() needs a moment to bind; poll /healthz first.
        assert_eq!(wait_healthz(&client, &base).await, "ok");

        // The real POST: JSON-RPC tools/list to the known token.
        let resp = client
            .post(format!("{base}/mcp/{token}"))
            .body(r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#)
            .send()
            .await
            .expect("POST to endpoint");
        assert_eq!(resp.status(), reqwest::StatusCode::OK);
        let text = resp.text().await.unwrap();
        assert!(text.contains("\"id\":1"), "response keeps the request id: {text}");
        assert!(text.contains("echo"), "response carries the simulated result: {text}");

        // Unknown token (request): HTTP 200 + a JSON-RPC error, NOT 404.
        // A non-2xx here would trip claude's OAuth discovery and mask the
        // real error.
        let unknown = client
            .post(format!("{base}/mcp/does-not-exist"))
            .body(r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#)
            .send()
            .await
            .expect("POST unknown token");
        assert_eq!(unknown.status(), reqwest::StatusCode::OK);
        let body = unknown.text().await.unwrap();
        assert!(body.contains("\"error\""), "unknown-token request -> JSON-RPC error: {body}");
        assert!(body.contains("-32001"), "uses the unknown-token code: {body}");
        assert!(body.contains("\"id\":2"), "keyed to the request id: {body}");
    }
}
