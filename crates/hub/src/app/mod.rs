//! User-facing web app — JSON API + SPA shell for `webterm/`.
//!
//! Mounted at the root of the main hub listener (alongside
//! `/v1/pty/ws`) rather than the admin listener: end-users hit this
//! from the public internet. `POST /api/login` exchanges an account
//! token (the same one the CLI client uses) for a short-lived
//! session id; the id rides in an HttpOnly cookie and authenticates
//! `/api/me` and the `/v1/pty/ws` WebSocket upgrade. SPA assets are
//! served from `/`.
//!
//! Sessions live in the `user_sessions` SQLite table (TTL = 12 h),
//! so they survive hub restarts: a logged-in webterm tab keeps
//! working straight through a self-update without prompting the
//! user to re-authenticate.
//!
//! Lifecycle vs admin: deliberately separate state — the admin token
//! is a single shared operator credential, the user sessions are one
//! per account login. Sharing the cookie name with admin would let an
//! admin browser auto-login as a user (and vice-versa); they live on
//! disjoint cookie names so that can't happen.
//!
//! Browsers send cookies for *any* request to the origin, so the WS
//! upgrade for `/v1/pty/ws` picks up the same cookie set by `/api/login`.
//! That sharing is the whole point — without it the webterm couldn't
//! reach the existing pty endpoint without redoing the protocol's
//! Hello token exchange.

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
use serde_json::json;
use std::sync::Arc;

pub const USER_SESSION_COOKIE: &str = "cc_user_session";

/// Session TTL applied to both the cookie's Max-Age and the
/// `user_sessions` row. Matches the admin side (12 h).
pub const USER_SESSION_TTL_SECS: i64 = 12 * 60 * 60;

/// DB-backed session store. Cookies survive hub restarts because
/// the `user_sessions` table is the source of truth.
pub struct UserAuth {
    db: crate::db::Db,
}

impl UserAuth {
    pub fn new(db: crate::db::Db) -> Self {
        Self { db }
    }

    /// Mint a fresh session id for the given account. The caller has
    /// already verified the account's token — we don't double-check
    /// here.
    pub async fn login(&self, account: String) -> String {
        let sid = crate::auth::generate_session_id();
        let expires_at = chrono::Utc::now().timestamp() + USER_SESSION_TTL_SECS;
        if let Err(e) = self.db.insert_user_session(&sid, &account, expires_at).await {
            tracing::warn!(error = %e, "could not persist user session; cookie won't survive a hub restart");
        }
        sid
    }

    /// Resolve a session id back to its account. `None` if expired /
    /// unknown / logged out.
    pub async fn lookup(&self, sid: &str) -> Option<String> {
        self.db.user_session_account(sid).await.unwrap_or(None)
    }

    pub async fn logout(&self, sid: &str) {
        let _ = self.db.delete_user_session(sid).await;
    }
}

/// Parse a named cookie value out of a raw `Cookie:` header. Returns
/// `None` if the header is absent, malformed, or doesn't contain the
/// requested name. Shared between the `/api/*` middleware and the
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
/// `me`) can read it without re-hitting the session table. Unauthenticated
/// requests get a 401 JSON envelope (`{"error":"unauthenticated",...}`)
/// rather than a redirect — the SPA is in charge of routing.
pub async fn require_user(
    State(state): State<Arc<AppState>>,
    mut req: Request,
    next: Next,
) -> Response {
    if let Some(sid) = parse_cookie(req.headers(), USER_SESSION_COOKIE) {
        if let Some(account) = state.user_auth.lookup(&sid).await {
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
