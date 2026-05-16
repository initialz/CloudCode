//! User-facing web app — JSON API + SPA shell for `webterm/`.
//!
//! Mounted on the main hub listener (alongside `/v1/pty/ws`) rather
//! than the admin listener: end-users hit this from the public
//! internet. `POST /app/api/login` exchanges an account token (the
//! same one the CLI client uses) for a short-lived session id; the id
//! rides in an HttpOnly cookie and authenticates `/app/api/me` and
//! the `/v1/pty/ws` WebSocket upgrade. SPA assets live under `/app/`.
//!
//! Sessions don't survive a hub restart (in-memory `DashMap`); the
//! webterm SPA detects the 401 and prompts re-login.
//!
//! Lifecycle vs admin: deliberately separate state — the admin token
//! is a single shared operator credential, the user sessions are one
//! per account login. Sharing the cookie name with admin would let an
//! admin browser auto-login as a user (and vice-versa); they live on
//! disjoint cookie names so that can't happen.
//!
//! Browsers send cookies for *any* request to the origin, so the WS
//! upgrade for `/v1/pty/ws` is the only place where cookie auth bleeds
//! out of `/app/*`. That bleed is the whole point — without it the
//! webterm couldn't reach the existing pty endpoint without redoing
//! the protocol's Hello token exchange.

pub mod api;
pub mod assets;

use crate::AppState;
use axum::{
    extract::{Request, State},
    http::{header::COOKIE, HeaderMap, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
    Json,
};
use dashmap::DashMap;
use serde_json::json;
use std::sync::Arc;

pub const USER_SESSION_COOKIE: &str = "cc_user_session";

/// In-memory session store. Maps the opaque session id (URL-safe
/// random) → account name. Cleared on hub restart.
pub struct UserAuth {
    sessions: DashMap<String, String>,
}

impl UserAuth {
    pub fn new() -> Self {
        Self {
            sessions: DashMap::new(),
        }
    }

    /// Mint a fresh session id for the given account. The caller has
    /// already verified the account's token — we don't double-check
    /// here.
    pub fn login(&self, account: String) -> String {
        let sid = crate::auth::generate_session_id();
        self.sessions.insert(sid.clone(), account);
        sid
    }

    /// Resolve a session id back to its account. None if expired /
    /// unknown / logged out.
    pub fn lookup(&self, sid: &str) -> Option<String> {
        self.sessions.get(sid).map(|r| r.value().clone())
    }

    pub fn logout(&self, sid: &str) {
        self.sessions.remove(sid);
    }
}

impl Default for UserAuth {
    fn default() -> Self {
        Self::new()
    }
}

/// Parse a named cookie value out of a raw `Cookie:` header. Returns
/// `None` if the header is absent, malformed, or doesn't contain the
/// requested name. Shared between the `/app/api/*` middleware and the
/// `/v1/pty/ws` upgrade path.
pub fn parse_cookie(headers: &HeaderMap, name: &str) -> Option<String> {
    let raw = headers.get(COOKIE).and_then(|v| v.to_str().ok())?;
    let prefix = format!("{}=", name);
    for part in raw.split(';') {
        let part = part.trim();
        if let Some(v) = part.strip_prefix(&prefix) {
            return Some(v.to_string());
        }
    }
    None
}

/// Pull the user session id out of cookies, look it up, and on a hit
/// stash the account name in the request extensions so handlers (like
/// `me`) can read it without going back to the DashMap. Unauthenticated
/// requests get a 401 JSON envelope (`{"error":"unauthenticated",...}`)
/// rather than a redirect — the SPA is in charge of routing.
pub async fn require_user(
    State(state): State<Arc<AppState>>,
    mut req: Request,
    next: Next,
) -> Response {
    if let Some(sid) = parse_cookie(req.headers(), USER_SESSION_COOKIE) {
        if let Some(account) = state.user_auth.lookup(&sid) {
            req.extensions_mut().insert(AuthedAccount(account));
            return next.run(req).await;
        }
    }
    (
        StatusCode::UNAUTHORIZED,
        Json(json!({"error": "unauthenticated", "message": "login required"})),
    )
        .into_response()
}

/// Newtype so handlers can pull the account name out of request
/// extensions unambiguously.
#[derive(Clone)]
pub struct AuthedAccount(pub String);
