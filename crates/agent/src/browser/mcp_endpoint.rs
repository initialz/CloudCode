//! Resident localhost MCP HTTP endpoint. claude (the MCP client) connects
//! here; each session is backed by a LOCAL per-session `@playwright/mcp`
//! subprocess that the endpoint owns directly (no hub, no remote client).
//!
//! Transport (UNCHANGED from M3, encodes hard-won lessons): Streamable HTTP
//! with POST-blocking. claude POSTs a JSON-RPC request; the endpoint feeds it
//! to the session's playwright-mcp subprocess over stdin, BLOCKS until the
//! matching response comes back (correlated by JSON-RPC `id`), then returns
//! that response as the POST response body. JSON-RPC notifications (no `id`)
//! are fed and get an immediate 202 with no body.
//!
//! CRITICAL: transport-level problems for a *request* (unknown token, timeout,
//! subprocess death) are returned as HTTP 200 + a JSON-RPC error body, NEVER a
//! bare non-2xx. Claude Code treats ANY non-2xx on the MCP POST as
//! "authentication required" and kicks off an OAuth discovery cascade that
//! 404s and surfaces as a misleading `SDK auth failed: HTTP 404`.
//!
//! The endpoint is still a DUMB RELAY w.r.t. MCP semantics (initialize
//! handshake, tool schemas, etc.) — those flow end-to-end between claude and
//! the playwright-mcp subprocess. The endpoint only: routes by
//! token→session_id, lazily spawns + pumps the per-session subprocess,
//! correlates request/response by JSON-RPC `id`, and tunnels opaque JSON text.

// Several entry points (register/reserve/unregister/mcp_config_json) are
// consumed by the pty/session wiring landing in Task 5; mirror the rest of the
// browser module's `#[allow(dead_code)]` so the not-yet-called surface doesn't
// trip the workspace's zero-warning bar.
#![allow(dead_code)]

use std::sync::Arc;
use std::time::Duration;

use dashmap::DashMap;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use uuid::Uuid;

use crate::browser::chrome::ChromeManager;
use crate::browser::subprocess::McpProcess;
use crate::config::BrowserConfig;

/// Default playwright-mcp version pin. Matches the spike validation; bump
/// deliberately. Overridable wholesale via `BrowserConfig.mcp_command`.
const PLAYWRIGHT_MCP_PIN: &str = "@playwright/mcp@0.0.76";

/// Key correlating an in-flight claude request to its eventual response:
/// (session_id, json-rpc id rendered canonically).
type PendingKey = (Uuid, String);

/// One session's local browser backend: the inbound side of its playwright-mcp
/// subprocess (write JSON-RPC frames here) plus the abort handle for the pump
/// task that drains the subprocess's stdout back into `resolve_response`.
struct SessionBrowser {
    /// Feed JSON-RPC request/notification frames to the subprocess stdin.
    inbound: tokio::sync::mpsc::Sender<String>,
    /// Pump task draining subprocess stdout → `resolve_response`. Aborted when
    /// the session entry is removed so we don't leak the task.
    pump: JoinHandle<()>,
}

impl Drop for SessionBrowser {
    fn drop(&mut self) {
        self.pump.abort();
    }
}

#[derive(Clone)]
pub struct EndpointState {
    /// claude-facing token -> session_id.
    routes: Arc<DashMap<String, Uuid>>,
    /// In-flight POST requests awaiting their response, by (session, id).
    pending: Arc<DashMap<PendingKey, oneshot::Sender<String>>>,
    /// Live per-session playwright-mcp subprocesses, lazily started.
    sessions: Arc<DashMap<Uuid, SessionBrowser>>,
    /// Per-session page hint for the viewer screencast (P4 Task 1): the CDP
    /// page **target id** (the `id` field of a `GET /json` entry) of the page
    /// belonging to this session's playwright-mcp. ViewerManager consults it via
    /// [`page_hint_for`](Self::page_hint_for) to screencast THAT session's page
    /// instead of "the active page" (closing the P2 cross-page leak).
    ///
    /// EMPTY by default: in the agent's current (non-`--isolated`)
    /// playwright-mcp config every session shares one browser context and one
    /// set of tabs, so there is no distinct per-session target to record and the
    /// viewer falls back to the active page (documented residual risk; see
    /// `docs/superpowers/plans/2026-06-10-p4-page-mapping-notes.md`). The map +
    /// [`record_page_hint`](Self::record_page_hint) are the plumbing that makes
    /// per-session targeting real the moment the stack runs `--isolated` (each
    /// session then owns a distinct context/target).
    page_hints: Arc<DashMap<Uuid, String>>,
    /// Resident Chrome handle: used to build the `--cdp-endpoint` URL the
    /// subprocess attaches to, and to consult readiness before first spawn.
    chrome: Arc<ChromeManager>,
    /// Browser config (mcp_command override, etc.).
    cfg: BrowserConfig,
}

impl EndpointState {
    pub fn new(chrome: Arc<ChromeManager>, cfg: BrowserConfig) -> Self {
        Self {
            routes: Arc::new(DashMap::new()),
            pending: Arc::new(DashMap::new()),
            sessions: Arc::new(DashMap::new()),
            page_hints: Arc::new(DashMap::new()),
            chrome,
            cfg,
        }
    }

    /// Map a claude token to a session route (used at session open, Task 5).
    /// Registering an already-known token OVERWRITES its route — this is how a
    /// stable per-workspace token gets re-pointed at the fresh session_id on
    /// every reattach.
    pub fn register(&self, token: String, session_id: Uuid) {
        self.routes.insert(token, session_id);
    }

    pub fn unregister(&self, token: &str) {
        self.routes.remove(token);
    }

    pub fn session_for(&self, token: &str) -> Option<Uuid> {
        self.routes.get(token).map(|r| *r.value())
    }

    /// Mint a fresh session_id for a token and register it (Task 5 helper).
    /// Returns the new session_id.
    pub fn reserve(&self, token: String) -> Uuid {
        let sid = Uuid::new_v4();
        self.routes.insert(token, sid);
        sid
    }

    /// Fail every in-flight request for `session_id` with a JSON-RPC error
    /// (e.g. after the subprocess died or was torn down).
    pub fn fail_pending(&self, session_id: Uuid, reason: &str) {
        let keys: Vec<PendingKey> = self
            .pending
            .iter()
            .filter(|e| e.key().0 == session_id)
            .map(|e| e.key().clone())
            .collect();
        for key in keys {
            if let Some((k, tx)) = self.pending.remove(&key) {
                let _ = tx.send(jsonrpc_error(&k.1, -32002, reason));
            }
        }
    }

    /// Tear down a session's browser subprocess (if any) and fail its
    /// in-flight requests. Idempotent. Called on claude session close,
    /// token re-point, and workspace delete/reset.
    pub fn end_session(&self, session_id: Uuid) {
        if self.sessions.remove(&session_id).is_some() {
            // Drop aborts the pump → McpProcess drops → kill_on_drop.
            tracing::debug!(%session_id, "ended browser session subprocess");
        }
        // Forget the screencast page hint so a later session reusing this id
        // (or a stale viewer attach) can't target a now-dead target.
        self.page_hints.remove(&session_id);
        self.fail_pending(session_id, "browser session ended");
    }

    /// Record the CDP page **target id** screencasts should use for this
    /// session (P4 Task 1). Called when the agent can attribute a specific page
    /// target to a session — i.e. when playwright-mcp runs `--isolated` and each
    /// session owns a distinct context/target (see the P4 page-mapping notes).
    /// In the current default config no distinct target exists, so this is not
    /// called and [`page_hint_for`](Self::page_hint_for) returns `None`.
    pub fn record_page_hint(&self, session_id: Uuid, target_id: String) {
        self.page_hints.insert(session_id, target_id);
    }

    /// The screencast page target hint for `session_id`, if known. ViewerManager
    /// passes this into [`pick_page_for_session`] so a viewer sees THAT session's
    /// page. `None` (the default-config case) means fall back to the active page.
    ///
    /// [`pick_page_for_session`]: crate::browser::screencast::pick_page_for_session
    pub fn page_hint_for(&self, session_id: Uuid) -> Option<String> {
        self.page_hints.get(&session_id).map(|r| r.value().clone())
    }

    /// Resolve the pending request matching this response frame's id. Called
    /// from the per-session pump when a frame arrives from the subprocess.
    /// Returns true if it matched an in-flight request; false otherwise (e.g. a
    /// server-initiated notification, which we drop).
    pub fn resolve_response(&self, session_id: Uuid, payload: String) -> bool {
        let id = extract_id_key(&payload);
        tracing::debug!(
            %session_id,
            has_id = id.is_some(),
            "browser MCP frame from subprocess; looking for pending waiter"
        );
        let Some(id) = id else {
            return false;
        };
        if let Some((_, tx)) = self.pending.remove(&(session_id, id)) {
            let matched = tx.send(payload).is_ok();
            tracing::debug!(%session_id, matched, "browser MCP pending waiter resolved");
            return matched;
        }
        tracing::debug!(%session_id, "browser MCP frame had no pending waiter");
        false
    }

    /// Build the playwright-mcp launch argv for this endpoint. If the operator
    /// set `mcp_command` (test / escape hatch) it's whitespace-split and used
    /// verbatim; otherwise we build the default
    /// `npx -y @playwright/mcp@<pin> --cdp-endpoint <chrome cdp url>`.
    fn mcp_argv(&self) -> Vec<String> {
        if let Some(cmd) = self.cfg.mcp_command.as_ref() {
            let parts: Vec<String> = cmd.split_whitespace().map(|s| s.to_string()).collect();
            if !parts.is_empty() {
                return parts;
            }
        }
        vec![
            "npx".to_string(),
            "-y".to_string(),
            PLAYWRIGHT_MCP_PIN.to_string(),
            "--cdp-endpoint".to_string(),
            self.chrome.cdp_http_url(),
        ]
    }

    /// Working directory for the subprocess: a scratch dir under the agent
    /// state dir so playwright-mcp's `.playwright-mcp/` output (traces,
    /// downloads) doesn't litter the cwd (spike finding #2). Best-effort — if
    /// the state dir can't be resolved/created we fall back to the process cwd.
    fn subprocess_cwd(&self) -> Option<std::path::PathBuf> {
        let dir = crate::paths::agent_state_dir()?.join("browser-scratch");
        if let Err(e) = std::fs::create_dir_all(&dir) {
            tracing::warn!(error = %e, dir = %dir.display(), "could not create browser scratch dir");
            return None;
        }
        Some(dir)
    }

    /// Ensure a live playwright-mcp subprocess exists for `session_id`,
    /// lazily spawning + pumping it on first use. Returns the inbound sender
    /// to feed frames, or an error string if the subprocess could not be
    /// spawned (turned into a JSON-RPC error by the caller).
    fn ensure_session(&self, session_id: Uuid) -> Result<tokio::sync::mpsc::Sender<String>, String> {
        // Fast path: already running.
        if let Some(s) = self.sessions.get(&session_id) {
            return Ok(s.inbound.clone());
        }

        if !self.chrome.is_ready() {
            // playwright-mcp attaches to the CDP endpoint lazily (only the first
            // tool call actually needs it), so spawning before Chrome is ready
            // is fine — the connect error, if any, surfaces as a tool error.
            tracing::warn!(
                %session_id,
                "spawning playwright-mcp before Chrome reported ready; \
                 attach happens lazily on first tool call"
            );
        }

        let argv = self.mcp_argv();
        let (program, rest) = argv.split_first().ok_or_else(|| "empty mcp command".to_string())?;
        let arg_refs: Vec<&str> = rest.iter().map(|s| s.as_str()).collect();

        let cwd = self.subprocess_cwd();
        let mut proc = McpProcess::spawn_with_cwd(program, &arg_refs, cwd.as_deref())
            .map_err(|e| format!("spawning playwright-mcp ({program}): {e}"))?;

        // Inbound channel: handle_post writes frames here; a small forwarder
        // task drains them onto the subprocess stdin. Keeping the writer in its
        // own task lets `handle_post` hand off without holding a lock on the
        // McpProcess across awaits.
        let (in_tx, mut in_rx) = tokio::sync::mpsc::channel::<String>(64);

        let state = self.clone();
        let pump = tokio::spawn(async move {
            loop {
                tokio::select! {
                    // Outbound: drain subprocess stdout → resolve pending.
                    frame = proc.next_frame() => {
                        match frame {
                            Some(line) => {
                                state.resolve_response(session_id, line);
                            }
                            None => {
                                tracing::warn!(%session_id, "playwright-mcp subprocess exited (EOF)");
                                state.fail_pending(session_id, "browser subprocess exited");
                                state.sessions.remove(&session_id);
                                return;
                            }
                        }
                    }
                    // Inbound: feed claude's frames to subprocess stdin.
                    msg = in_rx.recv() => {
                        match msg {
                            Some(payload) => {
                                if let Err(e) = proc.feed(&payload).await {
                                    tracing::warn!(%session_id, error = %e, "feeding playwright-mcp failed");
                                    state.fail_pending(session_id, "browser subprocess write failed");
                                    state.sessions.remove(&session_id);
                                    return;
                                }
                            }
                            None => {
                                // All senders dropped (session removed). Stop.
                                return;
                            }
                        }
                    }
                }
            }
        });

        // Race: another concurrent request for the same session may have
        // inserted first. DashMap::entry keeps exactly one; the loser's pump is
        // aborted via SessionBrowser::drop and its subprocess killed on drop.
        use dashmap::mapref::entry::Entry;
        match self.sessions.entry(session_id) {
            Entry::Occupied(e) => {
                // Someone beat us to it; discard ours.
                pump.abort();
                Ok(e.get().inbound.clone())
            }
            Entry::Vacant(slot) => {
                let inbound = in_tx.clone();
                slot.insert(SessionBrowser { inbound: in_tx, pump });
                Ok(inbound)
            }
        }
    }
}

/// Timeout for a blocking claude POST awaiting its response from the local
/// playwright-mcp subprocess. Handshake/metadata answers come back
/// immediately, so this stays short — kept below claude's own 30s MCP
/// connection timeout so our JSON-RPC error reaches claude instead of racing
/// its client-side timeout.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(25);

/// Tool calls can legitimately take long: the first one triggers the
/// browser/CDP attach, and pages can be slow. Keep this generous.
const CALL_TIMEOUT: Duration = Duration::from_secs(120);

/// The human-handoff tool (`request_handoff`) blocks while a person does manual
/// browser work — login flows take minutes. Harmless dead tier in P1 (no such
/// tool is wired yet); kept verbatim, costs nothing.
const HANDOFF_TIMEOUT: Duration = Duration::from_secs(600);

/// Method-aware timeout selection, three tiers: `tools/call` for
/// `request_handoff` gets the human-scale HANDOFF_TIMEOUT; any other
/// `tools/call` gets the generous CALL_TIMEOUT; everything else (handshake,
/// metadata, garbage) gets the short REQUEST_TIMEOUT.
fn timeout_for(body: &str) -> Duration {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(body) else {
        return REQUEST_TIMEOUT;
    };
    if v.get("method").and_then(|m| m.as_str()) != Some("tools/call") {
        return REQUEST_TIMEOUT;
    }
    let tool = v
        .get("params")
        .and_then(|p| p.get("name"))
        .and_then(|n| n.as_str());
    if tool == Some("request_handoff") {
        HANDOFF_TIMEOUT
    } else {
        CALL_TIMEOUT
    }
}

/// Build a JSON-RPC error response body for a given request id (raw id string
/// as captured by extract_id_key, e.g. "1" or "\"abc\"").
fn jsonrpc_error(id_raw: &str, code: i64, message: &str) -> String {
    format!(
        r#"{{"jsonrpc":"2.0","id":{id_raw},"error":{{"code":{code},"message":{msg}}}}}"#,
        msg = serde_json::to_string(message).unwrap_or_else(|_| "\"error\"".to_string())
    )
}

/// First ~8 chars of a token, for non-secret-leaking diagnostics.
fn token_prefix(token: &str) -> &str {
    let end = token
        .char_indices()
        .nth(8)
        .map(|(i, _)| i)
        .unwrap_or(token.len());
    &token[..end]
}

/// Extract the JSON-RPC `id` from a frame as a canonical string key. Returns
/// None for notifications (no id) or unparseable bodies.
fn extract_id_key(body: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(body).ok()?;
    match v.get("id") {
        Some(serde_json::Value::Null) | None => None,
        Some(id) => Some(id.to_string()), // numbers -> "1", strings -> "\"abc\""
    }
}

/// Build the `--mcp-config` JSON claude should load for this session, pointing
/// at the resident endpoint via Streamable HTTP.
pub fn mcp_config_json(port: u16, token: &str) -> String {
    format!(
        r#"{{"mcpServers":{{"cc-browser":{{"type":"http","url":"http://127.0.0.1:{port}/mcp/{token}"}}}}}}"#
    )
}

/// Pull the token back out of a previously written mcp-browser.json so an agent
/// restart re-adopts it instead of minting a fresh one.
pub fn extract_token_from_config(json: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(json).ok()?;
    let url = v
        .get("mcpServers")?
        .get("cc-browser")?
        .get("url")?
        .as_str()?;
    let token = url.rsplit('/').next()?;
    if token.is_empty() {
        return None;
    }
    Some(token.to_string())
}

/// True for a well-formed workspace browser token: exactly 32 ASCII hex chars —
/// the `Uuid::new_v4().simple()` format we mint. Guards the self-heal adoption
/// path against a tampered/corrupt mcp-browser.json smuggling an arbitrary
/// string into the endpoint's token map.
pub fn is_valid_token(token: &str) -> bool {
    token.len() == 32 && token.bytes().all(|b| b.is_ascii_hexdigit())
}

/// Outcome of a claude POST. Mapped to HTTP status in the axum handler.
///
/// Transport-level problems for a JSON-RPC *request* (unknown token, timeout,
/// subprocess death) are returned as `Response` carrying a JSON-RPC error
/// object at HTTP 200 — NOT a bare non-2xx status. Claude Code treats ANY
/// non-2xx on the MCP POST as "authentication required" and kicks off an OAuth
/// discovery cascade that 404s and surfaces as a misleading
/// `SDK auth failed: HTTP 404`. Returning 200 + a JSON-RPC error lets the real
/// error through.
pub enum PostOutcome {
    /// A JSON-RPC body to return (application/json, 200). Either a real
    /// response from the subprocess, or a JSON-RPC error object built here for
    /// a transport-level failure.
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
        // Request to a known session: ensure the subprocess is up, feed, then
        // block for the matching response. tools/call gets a generous timeout
        // (first call triggers the CDP attach); everything else keeps the short
        // handshake timeout.
        (Some(id), Some(session_id)) => {
            let inbound = match state.ensure_session(session_id) {
                Ok(tx) => tx,
                Err(e) => {
                    tracing::warn!(
                        token = %token_prefix(token),
                        %session_id,
                        error = %e,
                        "could not start browser subprocess for session"
                    );
                    return PostOutcome::Response(jsonrpc_error(
                        &id,
                        -32003,
                        &format!("browser subprocess failed to start: {e}"),
                    ));
                }
            };

            let timeout = timeout_for(&body);
            let (tx, rx) = oneshot::channel();
            state.pending.insert((session_id, id.clone()), tx);

            if inbound.send(body).await.is_err() {
                // Pump gone (subprocess died between ensure and feed). Clean up
                // the pending we just inserted and report a transport error.
                state.pending.remove(&(session_id, id.clone()));
                return PostOutcome::Response(jsonrpc_error(
                    &id,
                    -32002,
                    "browser subprocess exited before the request could be sent",
                ));
            }

            match tokio::time::timeout(timeout, rx).await {
                Ok(Ok(resp)) => PostOutcome::Response(resp),
                _ => {
                    state.pending.remove(&(session_id, id.clone()));
                    tracing::warn!(
                        token = %token_prefix(token),
                        %session_id,
                        timeout_secs = timeout.as_secs(),
                        "browser MCP request timed out awaiting subprocess response"
                    );
                    PostOutcome::Response(jsonrpc_error(
                        &id,
                        -32000,
                        "browser MCP request timed out (the browser may still be \
                         starting on the first call — retrying usually succeeds)",
                    ))
                }
            }
        }
        // Notification to an unknown token: nothing to deliver and nothing to
        // return. Don't 404 a notification — just log and accept.
        (None, None) => {
            tracing::warn!(
                token = %token_prefix(token),
                "browser MCP notification for unknown token; dropping"
            );
            PostOutcome::Accepted
        }
        // Notification to a known session: ensure the subprocess is up, feed,
        // no response expected.
        (None, Some(session_id)) => {
            match state.ensure_session(session_id) {
                Ok(inbound) => {
                    let _ = inbound.send(body).await;
                }
                Err(e) => {
                    tracing::warn!(%session_id, error = %e, "dropping notification; subprocess failed to start");
                }
            }
            PostOutcome::Accepted
        }
    }
}

/// Build the axum router for the localhost MCP relay. POST `/mcp/:token` is the
/// POST-blocking JSON-RPC relay; GET on the same path is rejected (no
/// server-initiated SSE). `/healthz` stays so we can verify the listener is up.
fn build_router(state: EndpointState) -> axum::Router {
    use axum::extract::{Path, State};
    use axum::http::StatusCode;
    use axum::response::IntoResponse;
    use axum::routing::{get, post};

    axum::Router::new()
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
            // No server-initiated SSE; reject GET cleanly.
            .get(|| async { StatusCode::METHOD_NOT_ALLOWED }),
        )
        .with_state(state)
}

/// Bind the localhost MCP listener on `port` and serve until shutdown.
pub async fn serve(state: EndpointState, port: u16) -> std::io::Result<()> {
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", port)).await?;
    tracing::info!(port, "browser MCP endpoint listening on 127.0.0.1");
    serve_on_listener(state, listener).await
}

/// Serve on an already-bound listener. Used by tests so they can bind `:0` and
/// hand the live listener straight to `serve` — eliminating the bind/close/
/// re-bind window that races a concurrently-assigned port across tests.
pub async fn serve_on_listener(
    state: EndpointState,
    listener: tokio::net::TcpListener,
) -> std::io::Result<()> {
    axum::serve(listener, build_router(state))
        .await
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    /// Best-effort check whether `node` is on PATH (capability probe).
    fn which_node() -> bool {
        let Some(path) = std::env::var_os("PATH") else {
            return false;
        };
        std::env::split_paths(&path).any(|dir| dir.join("node").is_file())
    }

    /// Absolute path to the echo-mcp stub fixture.
    fn echo_stub() -> &'static str {
        concat!(env!("CARGO_MANIFEST_DIR"), "/../../test-fixtures/echo-mcp.mjs")
    }

    /// Build an EndpointState whose subprocess command is the given override
    /// (whitespace-split). chrome is a non-started manager (we never spawn real
    /// Chrome in these tests; the echo stub ignores --cdp-endpoint).
    fn state_with_cmd(cmd: Option<String>) -> EndpointState {
        let cfg = BrowserConfig {
            enabled: true,
            chrome_path: None,
            cdp_port: 19222,
            mcp_port: 7110,
            mcp_command: cmd,
        };
        let chrome = Arc::new(ChromeManager::new(cfg.clone(), Path::new("/tmp/cc-test-state/agent")));
        EndpointState::new(chrome, cfg)
    }

    #[tokio::test]
    async fn unknown_token_returns_jsonrpc_error_200() {
        // A REQUEST (has an id) to an unknown token must NOT be a bare non-2xx
        // (which would trigger claude's OAuth misfire). It returns a JSON-RPC
        // error body at HTTP 200.
        let state = state_with_cmd(None);
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
        let state = state_with_cmd(None);
        let out = handle_post(
            "nope",
            r#"{"jsonrpc":"2.0","method":"notify"}"#.to_string(),
            &state,
        )
        .await;
        assert!(matches!(out, PostOutcome::Accepted));
    }

    #[tokio::test]
    async fn notification_accepted_202() {
        // Notification to a LIVE session (echo stub) is fed and accepted (202).
        if !which_node() {
            return; // no node -> skip
        }
        let state = state_with_cmd(Some(format!("node {}", echo_stub())));
        let sid = Uuid::new_v4();
        state.register("t".into(), sid);

        let out = handle_post(
            "t",
            r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#.to_string(),
            &state,
        )
        .await;
        assert!(matches!(out, PostOutcome::Accepted));
        // The session subprocess should have been lazily started.
        assert!(state.sessions.contains_key(&sid));
    }

    #[tokio::test]
    async fn request_roundtrips_via_local_subprocess() {
        // REAL local subprocess roundtrip: drive the actual axum endpoint over
        // real HTTP, backed by the node echo stub as the playwright-mcp stand-in.
        if !which_node() {
            return; // no node -> skip
        }
        let state = state_with_cmd(Some(format!("node {}", echo_stub())));
        let sid = Uuid::new_v4();
        let token = "tok-e2e";
        state.register(token.into(), sid);

        // Bind :0 and hand the LIVE listener to serve — no close/re-bind window,
        // so a concurrent test can never steal this port out from under us.
        let (listener, base) = bound_listener().await;
        let serve_state = state.clone();
        tokio::spawn(async move {
            let _ = serve_on_listener(serve_state, listener).await;
        });

        let client = reqwest::Client::new();
        assert_eq!(wait_healthz(&client, &base).await, "ok");

        // initialize handshake -> echo stub answers with serverInfo.
        let init = client
            .post(format!("{base}/mcp/{token}"))
            .body(r#"{"jsonrpc":"2.0","id":1,"method":"initialize"}"#)
            .send()
            .await
            .expect("POST initialize");
        assert_eq!(init.status(), reqwest::StatusCode::OK);
        let init_body = init.text().await.unwrap();
        assert!(init_body.contains("\"id\":1"), "keeps request id: {init_body}");
        assert!(init_body.contains("echo-mcp"), "echo stub serverInfo: {init_body}");

        // tools/list -> echo stub lists the `echo` tool.
        let list = client
            .post(format!("{base}/mcp/{token}"))
            .body(r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#)
            .send()
            .await
            .expect("POST tools/list");
        assert_eq!(list.status(), reqwest::StatusCode::OK);
        let list_body = list.text().await.unwrap();
        assert!(list_body.contains("\"id\":2"), "keeps request id: {list_body}");
        assert!(list_body.contains("echo"), "carries the echo tool: {list_body}");

        // Unknown token (request): HTTP 200 + a JSON-RPC error, NOT 404.
        let unknown = client
            .post(format!("{base}/mcp/does-not-exist"))
            .body(r#"{"jsonrpc":"2.0","id":3,"method":"tools/list"}"#)
            .send()
            .await
            .expect("POST unknown token");
        assert_eq!(unknown.status(), reqwest::StatusCode::OK);
        let body = unknown.text().await.unwrap();
        assert!(body.contains("\"error\""), "unknown-token request -> JSON-RPC error: {body}");
        assert!(body.contains("-32001"), "uses the unknown-token code: {body}");
    }

    #[tokio::test]
    async fn subprocess_death_fails_pending() {
        // A subprocess that exits immediately must fail the in-flight request
        // with a JSON-RPC error (the EOF path), within a bounded time — NOT
        // hang until the 25s timeout, and NEVER a bare non-2xx.
        if !which_node() {
            return; // no node -> skip
        }
        let state = state_with_cmd(Some("node -e process.exit(0)".to_string()));
        let sid = Uuid::new_v4();
        state.register("t".into(), sid);

        let started = std::time::Instant::now();
        let out = tokio::time::timeout(
            Duration::from_secs(10),
            handle_post(
                "t",
                r#"{"jsonrpc":"2.0","id":7,"method":"initialize"}"#.to_string(),
                &state,
            ),
        )
        .await
        .expect("handle_post must resolve well before the 25s request timeout");

        assert!(
            started.elapsed() < Duration::from_secs(10),
            "subprocess-death path should fail fast, took {:?}",
            started.elapsed()
        );
        match out {
            PostOutcome::Response(b) => {
                assert!(b.contains("\"error\""), "must be a JSON-RPC error: {b}");
                assert!(b.contains("-32002"), "uses the subprocess-gone code: {b}");
                assert!(b.contains("\"id\":7"), "keyed to the request id: {b}");
            }
            _ => panic!("expected a JSON-RPC error Response, not Accepted"),
        }
    }

    #[tokio::test]
    async fn fail_pending_fails_session_requests_and_leaves_other_session_intact() {
        let state = state_with_cmd(None);
        let sid_a = Uuid::new_v4();
        let sid_b = Uuid::new_v4();

        let (tx_a1, rx_a1) = oneshot::channel::<String>();
        let (tx_a2, rx_a2) = oneshot::channel::<String>();
        state.pending.insert((sid_a, "1".to_string()), tx_a1);
        state.pending.insert((sid_a, "2".to_string()), tx_a2);

        let (tx_b, _rx_b) = oneshot::channel::<String>();
        state.pending.insert((sid_b, "3".to_string()), tx_b);

        state.fail_pending(sid_a, "denied by user");

        let body_a1 = rx_a1.await.expect("a1 oneshot received");
        assert!(body_a1.contains("-32002"), "expected -32002 in: {body_a1}");
        assert!(body_a1.contains("denied by user"), "expected reason in: {body_a1}");

        let body_a2 = rx_a2.await.expect("a2 oneshot received");
        assert!(body_a2.contains("-32002"), "expected -32002 in: {body_a2}");

        assert!(!state.pending.contains_key(&(sid_a, "1".to_string())));
        assert!(!state.pending.contains_key(&(sid_a, "2".to_string())));
        assert!(state.pending.contains_key(&(sid_b, "3".to_string())));
    }

    #[tokio::test]
    async fn end_session_removes_subprocess_and_fails_pending() {
        // With node available we can lazily start a real (echo-stub) subprocess
        // for the session, then prove end_session removes the live entry AND
        // fails an in-flight request. Without node we still cover the
        // fail_pending half (no live entry to remove).
        let cmd = which_node().then(|| format!("node {}", echo_stub()));
        let state = state_with_cmd(cmd.clone());
        let sid = Uuid::new_v4();
        state.register("t".into(), sid);

        if cmd.is_some() {
            // Lazily start the subprocess by feeding a notification.
            let out = handle_post(
                "t",
                r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#.to_string(),
                &state,
            )
            .await;
            assert!(matches!(out, PostOutcome::Accepted));
            assert!(
                state.sessions.contains_key(&sid),
                "subprocess should be live before end_session"
            );
        }

        // An in-flight request for this session must be failed by end_session.
        let (tx, rx) = oneshot::channel::<String>();
        state.pending.insert((sid, "1".to_string()), tx);

        state.end_session(sid);

        // Session subprocess entry gone (Drop aborts pump → kill_on_drop).
        assert!(
            !state.sessions.contains_key(&sid),
            "end_session must remove the live session entry"
        );
        // The pending request received a JSON-RPC error (fail_pending path).
        let body = rx.await.expect("pending oneshot received an error frame");
        assert!(body.contains("-32002"), "expected error code in: {body}");
        assert!(
            body.contains("browser session ended"),
            "expected end-session reason in: {body}"
        );
        assert!(
            !state.pending.contains_key(&(sid, "1".to_string())),
            "end_session must drain the pending entry"
        );
        // Full subprocess-kill is also covered by the #[ignore]d full-stack
        // integration test; here we assert the bookkeeping is consistent.
    }

    #[tokio::test]
    async fn end_session_on_unknown_id_is_noop() {
        // Idempotent / safe: ending a session that was never started must not
        // panic and must leave state untouched.
        let state = state_with_cmd(None);
        let unknown = Uuid::new_v4();
        state.end_session(unknown); // no panic
        assert!(!state.sessions.contains_key(&unknown));
        // Second call is equally harmless.
        state.end_session(unknown);
    }

    #[test]
    fn jsonrpc_error_has_valid_shape() {
        let body = jsonrpc_error("1", -32000, "browser MCP request timed out");
        let v: serde_json::Value = serde_json::from_str(&body).expect("valid JSON");
        assert_eq!(v["jsonrpc"], "2.0");
        assert_eq!(v["id"], 1);
        assert_eq!(v["error"]["code"], -32000);
        assert_eq!(v["error"]["message"], "browser MCP request timed out");

        let body = jsonrpc_error("\"abc\"", -32001, "x");
        let v: serde_json::Value = serde_json::from_str(&body).expect("valid JSON");
        assert_eq!(v["id"], "abc");

        let body = jsonrpc_error("1", -1, "has \"quotes\" and \\ backslash");
        let v: serde_json::Value = serde_json::from_str(&body).expect("valid JSON");
        assert_eq!(v["error"]["message"], "has \"quotes\" and \\ backslash");
    }

    #[test]
    fn timeout_for_is_method_aware() {
        assert_eq!(
            timeout_for(
                r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"request_handoff","arguments":{"reason":"login"}}}"#
            ),
            HANDOFF_TIMEOUT
        );
        assert_eq!(HANDOFF_TIMEOUT, Duration::from_secs(600));
        assert_eq!(
            timeout_for(
                r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"browser_navigate"}}"#
            ),
            CALL_TIMEOUT
        );
        assert_eq!(
            timeout_for(r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{}}"#),
            CALL_TIMEOUT
        );
        assert_eq!(CALL_TIMEOUT, Duration::from_secs(120));
        assert_eq!(
            timeout_for(r#"{"jsonrpc":"2.0","id":1,"method":"initialize"}"#),
            REQUEST_TIMEOUT
        );
        assert_eq!(
            timeout_for(r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#),
            REQUEST_TIMEOUT
        );
        assert_eq!(timeout_for("not json"), REQUEST_TIMEOUT);
        assert_eq!(REQUEST_TIMEOUT, Duration::from_secs(25));
    }

    #[test]
    fn config_has_http_url_with_token() {
        let s = mcp_config_json(7110, "abc123");
        assert!(s.contains("\"type\":\"http\""));
        assert!(s.contains("http://127.0.0.1:7110/mcp/abc123"));
        let _: serde_json::Value = serde_json::from_str(&s).unwrap();
    }

    #[test]
    fn extract_token_roundtrip() {
        let json = mcp_config_json(7110, "abc123");
        assert_eq!(extract_token_from_config(&json), Some("abc123".to_string()));
    }

    #[test]
    fn extract_token_garbage_json_returns_none() {
        assert_eq!(extract_token_from_config("not json at all"), None);
        assert_eq!(extract_token_from_config(""), None);
        assert_eq!(extract_token_from_config("{]"), None);
    }

    #[test]
    fn extract_token_missing_fields_returns_none() {
        assert_eq!(extract_token_from_config(r#"{"other":"value"}"#), None);
        assert_eq!(
            extract_token_from_config(r#"{"mcpServers":{"other":{}}}"#),
            None
        );
        assert_eq!(
            extract_token_from_config(r#"{"mcpServers":{"cc-browser":{"type":"http"}}}"#),
            None
        );
        assert_eq!(
            extract_token_from_config(r#"{"mcpServers":{"cc-browser":{"url":42}}}"#),
            None
        );
    }

    #[test]
    fn extract_token_empty_token_returns_none() {
        let json = r#"{"mcpServers":{"cc-browser":{"url":"http://127.0.0.1:7110/mcp/"}}}"#;
        assert_eq!(extract_token_from_config(json), None);
    }

    #[test]
    fn token_validation_accepts_minted_format() {
        let minted = uuid::Uuid::new_v4().simple().to_string();
        assert!(is_valid_token(&minted));
        assert!(is_valid_token("ABCDEF0123456789abcdef0123456789"));
    }

    #[test]
    fn token_validation_rejects_malformed() {
        assert!(!is_valid_token(""));
        assert!(!is_valid_token("abc123"));
        assert!(!is_valid_token(&"a".repeat(31)));
        assert!(!is_valid_token(&"a".repeat(33)));
        assert!(!is_valid_token("g".repeat(32).as_str()));
        assert!(!is_valid_token("123e4567-e89b-12d3-a456-42661417"));
        assert!(!is_valid_token("../../../../etc/passwd00000000000"));
    }

    #[test]
    fn register_maps_token_to_session() {
        let st = state_with_cmd(None);
        let sid = uuid::Uuid::new_v4();
        st.register("tok-a".to_string(), sid);
        assert_eq!(st.session_for("tok-a"), Some(sid));
    }

    #[test]
    fn register_overwrite_repoints_token_at_new_session() {
        let st = state_with_cmd(None);
        let sid1 = Uuid::new_v4();
        let sid2 = Uuid::new_v4();
        let tok = "stable-workspace-token".to_string();
        st.register(tok.clone(), sid1);
        st.register(tok.clone(), sid2);
        assert_eq!(st.session_for(&tok), Some(sid2));
    }

    #[test]
    fn unregister_clears_route() {
        let st = state_with_cmd(None);
        let sid = uuid::Uuid::new_v4();
        st.register("tok-b".to_string(), sid);
        assert_eq!(st.session_for("tok-b"), Some(sid));
        st.unregister("tok-b");
        assert_eq!(st.session_for("tok-b"), None);
    }

    #[test]
    fn page_hint_absent_by_default_then_recorded_then_cleared() {
        // P4 Task 1: the screencast page hint is None until recorded (the
        // default-config case), readable once recorded, and cleared on
        // end_session so a reused id can't target a dead page.
        let st = state_with_cmd(None);
        let sid = Uuid::new_v4();
        assert_eq!(st.page_hint_for(sid), None, "no hint before recording");

        st.record_page_hint(sid, "TARGET_ABC".to_string());
        assert_eq!(st.page_hint_for(sid), Some("TARGET_ABC".to_string()));

        // Re-record overwrites (session navigated to a new target).
        st.record_page_hint(sid, "TARGET_XYZ".to_string());
        assert_eq!(st.page_hint_for(sid), Some("TARGET_XYZ".to_string()));

        st.end_session(sid);
        assert_eq!(st.page_hint_for(sid), None, "hint cleared on end_session");

        // Unknown session is always None.
        assert_eq!(st.page_hint_for(Uuid::new_v4()), None);
    }

    #[test]
    fn id_key_distinguishes_number_and_string() {
        assert_eq!(extract_id_key(r#"{"id":1}"#), Some("1".to_string()));
        assert_eq!(extract_id_key(r#"{"id":"a"}"#), Some("\"a\"".to_string()));
        assert_eq!(extract_id_key(r#"{"method":"x"}"#), None);
        assert_eq!(extract_id_key(r#"{"id":null}"#), None);
    }

    /// Full-stack P1 integration: REAL headless Chrome + REAL per-session
    /// `@playwright/mcp` subprocess, driven end-to-end through the actual axum
    /// endpoint over HTTP — exactly the path claude takes, minus claude itself.
    ///
    /// Run manually (it shells out to npx + launches Chrome, so it's #[ignore]d
    /// out of the default suite):
    /// ```text
    /// cargo test -p cloudcode-agent full_stack_real_chrome -- --ignored --nocapture
    /// ```
    /// Prereqs: a real Chrome/Chromium install, `node`/`npx` on PATH, and (for
    /// the example.com hop) internet. If there's no internet the test falls back
    /// to a `data:` URL so it still exercises the full Chrome+playwright path.
    #[tokio::test]
    #[ignore = "requires real Chrome + npx playwright-mcp (+ internet); run manually"]
    async fn full_stack_real_chrome_and_playwright() {
        // --- 1. Real resident Chrome on a random CDP port. ---
        let cdp_port = free_port();
        let mcp_port = free_port();
        let cfg = BrowserConfig {
            enabled: true,
            chrome_path: None, // auto-detect a real Chrome/Chromium
            cdp_port,
            mcp_port,
            mcp_command: None, // -> default: npx -y @playwright/mcp@PIN --cdp-endpoint <chrome>
        };
        let tmp = tempfile::tempdir().expect("tempdir for Chrome profile");
        let chrome = Arc::new(ChromeManager::new(cfg.clone(), tmp.path()));
        chrome
            .start()
            .await
            .expect("real Chrome should start and become ready");
        assert!(chrome.is_ready(), "Chrome must report ready after start()");

        // --- 2. Endpoint backed by that Chrome + the real playwright-mcp. ---
        let state = EndpointState::new(Arc::clone(&chrome), cfg);
        let token = uuid::Uuid::new_v4().simple().to_string();
        let sid = Uuid::new_v4();
        state.register(token.clone(), sid);

        let serve_state = state.clone();
        let serve_task = tokio::spawn(async move {
            let _ = serve(serve_state, mcp_port).await;
        });

        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(150))
            .build()
            .expect("reqwest client");
        let base = format!("http://127.0.0.1:{mcp_port}");
        assert_eq!(wait_healthz(&client, &base).await, "ok");

        let url = format!("{base}/mcp/{token}");
        let post = |body: &'static str| {
            let client = client.clone();
            let url = url.clone();
            async move {
                client
                    .post(&url)
                    .body(body)
                    .send()
                    .await
                    .expect("MCP POST")
            }
        };

        // --- 3. MCP handshake. ---
        let init = post(
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"cc-fullstack-test","version":"0"}}}"#,
        )
        .await;
        assert_eq!(init.status(), reqwest::StatusCode::OK);
        let init_body = init.text().await.unwrap();
        assert!(
            init_body.contains("\"id\":1") && !init_body.contains("\"error\""),
            "initialize must succeed (no JSON-RPC error): {init_body}"
        );

        // notifications/initialized -> 202, no body.
        let notif = post(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#).await;
        assert_eq!(notif.status(), reqwest::StatusCode::ACCEPTED);

        // --- 4. Navigate. Prefer example.com; fall back to a self-contained
        //        data: URL if the network is unavailable so the test still
        //        proves the full Chrome + playwright path. ---
        let online = reqwest::Client::builder()
            .timeout(Duration::from_secs(8))
            .build()
            .unwrap()
            .get("https://example.com")
            .send()
            .await
            .map(|r| r.status().is_success())
            .unwrap_or(false);

        let (nav_body, expect_in_snapshot): (&'static str, &str) = if online {
            (
                r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"browser_navigate","arguments":{"url":"https://example.com"}}}"#,
                "Example",
            )
        } else {
            (
                r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"browser_navigate","arguments":{"url":"data:text/html,<title>CcFullStackProbe</title><h1>CcFullStackProbe</h1>"}}}"#,
                "CcFullStackProbe",
            )
        };

        let nav = post(nav_body).await;
        assert_eq!(nav.status(), reqwest::StatusCode::OK);
        let nav_resp = nav.text().await.unwrap();
        assert!(
            !nav_resp.contains("\"error\""),
            "navigate must not return a JSON-RPC error (online={online}): {nav_resp}"
        );

        // --- 5. Snapshot must contain the page's identifying text. ---
        let snap = post(
            r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"browser_snapshot","arguments":{}}}"#,
        )
        .await;
        assert_eq!(snap.status(), reqwest::StatusCode::OK);
        let snap_resp = snap.text().await.unwrap();
        assert!(
            !snap_resp.contains("\"error\""),
            "snapshot must not return a JSON-RPC error: {snap_resp}"
        );
        assert!(
            snap_resp.contains(expect_in_snapshot),
            "snapshot should contain {expect_in_snapshot:?} (online={online}): {snap_resp}"
        );

        // --- 6. Cleanup. Dropping the endpoint state aborts the per-session
        //        pump -> drops McpProcess -> playwright-mcp + its browser tab go
        //        away; dropping the ChromeManager kills resident Chrome
        //        (kill_on_drop). serve_task ends when its state Arc is gone. ---
        serve_task.abort();
        drop(state);
        drop(chrome);
    }

    /// Grab a currently-free localhost TCP port by binding to :0.
    ///
    /// NOTE: this closes the listener, so the returned port is only *probably*
    /// free — there's a window where another binder can take it. Use it only
    /// where a bare port number is unavoidable (e.g. a CDP port handed to
    /// Chrome). For our own `serve`, prefer [`bound_listener`], which keeps the
    /// listener open and races nothing.
    fn free_port() -> u16 {
        let l = std::net::TcpListener::bind("127.0.0.1:0").expect("bind :0");
        l.local_addr().expect("local_addr").port()
    }

    /// Bind `127.0.0.1:0` and return the LIVE listener plus its `http://` base
    /// URL. The listener stays open and is meant to be handed straight to
    /// [`serve_on_listener`], so there is no bind/close/re-bind window for a
    /// concurrent test to slip into — this is the race-free way to stand up the
    /// endpoint in tests.
    async fn bound_listener() -> (tokio::net::TcpListener, String) {
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0u16))
            .await
            .expect("bind 127.0.0.1:0");
        let port = listener.local_addr().expect("local_addr").port();
        (listener, format!("http://127.0.0.1:{port}"))
    }

    /// Wait until `serve` has bound and `/healthz` answers.
    async fn wait_healthz(client: &reqwest::Client, base: &str) -> String {
        for _ in 0..100 {
            match client.get(format!("{base}/healthz")).send().await {
                Ok(resp) => return resp.text().await.unwrap(),
                Err(_) => tokio::time::sleep(Duration::from_millis(20)).await,
            }
        }
        panic!("endpoint never came up on {base}");
    }
}
