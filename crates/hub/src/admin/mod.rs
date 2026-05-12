//! Admin UI — login-gated HTTP frontend served on a separate listener.
//!
//! Mounted by `serve()` in main.rs when `[admin].token_hash` is set. The
//! whole UI lives under `/admin`. Authentication is a single shared
//! login token (argon2id-hashed in hub.toml); successful login mints a
//! random session id, stored in an in-memory map. The session id is
//! returned to the browser as an `HttpOnly` cookie. Sessions don't
//! survive a hub restart — operator re-logs in.

mod handlers;

use crate::auth;
use crate::AppState;
use axum::{
    extract::{Request, State},
    http::{header::COOKIE, HeaderMap},
    middleware::{self, Next},
    response::{IntoResponse, Redirect, Response},
    routing::{get, post},
    Router,
};
use dashmap::DashMap;
use std::sync::Arc;

pub const SESSION_COOKIE: &str = "cc_admin";

/// Combined state passed into admin handlers: the existing hub state
/// (db, registry, config) plus per-admin-server session bookkeeping.
#[derive(Clone)]
pub struct AdminState {
    pub app: Arc<AppState>,
    pub auth: Arc<AdminAuth>,
}

/// In-memory session table. Each successful login mints a session id and
/// inserts it here. Logout deletes the entry. Hub restart wipes
/// everything — that's intentional; admin re-logs in.
pub struct AdminAuth {
    sessions: DashMap<String, ()>,
    token_hash: String,
}

impl AdminAuth {
    pub fn new(token_hash: String) -> Self {
        Self {
            sessions: DashMap::new(),
            token_hash,
        }
    }

    /// Verify the plaintext token; on success allocate and return a
    /// fresh session id to set as a cookie.
    pub fn login(&self, plaintext: &str) -> Option<String> {
        if auth::verify_token(plaintext, &self.token_hash) {
            let sid = auth::generate_session_id();
            self.sessions.insert(sid.clone(), ());
            Some(sid)
        } else {
            None
        }
    }

    pub fn is_valid(&self, sid: &str) -> bool {
        self.sessions.contains_key(sid)
    }

    pub fn logout(&self, sid: &str) {
        self.sessions.remove(sid);
    }
}

/// Pull the `cc_admin` session id out of the Cookie request header, if
/// any. Cookies arrive as a single header value of the form
/// `name1=value1; name2=value2; ...`.
pub fn session_cookie(headers: &HeaderMap) -> Option<String> {
    let raw = headers.get(COOKIE).and_then(|v| v.to_str().ok())?;
    for part in raw.split(';') {
        let part = part.trim();
        if let Some(v) = part.strip_prefix(&format!("{}=", SESSION_COOKIE)) {
            return Some(v.to_string());
        }
    }
    None
}

/// Middleware applied to protected admin routes. Unauthenticated
/// requests get a 302 to the login page.
async fn require_admin(State(state): State<AdminState>, req: Request, next: Next) -> Response {
    if let Some(sid) = session_cookie(req.headers()) {
        if state.auth.is_valid(&sid) {
            return next.run(req).await;
        }
    }
    Redirect::to("/admin/login").into_response()
}

pub fn router(state: AdminState) -> Router {
    let gate = middleware::from_fn_with_state(state.clone(), require_admin);
    Router::new()
        .route(
            "/admin/login",
            get(handlers::login_page).post(handlers::login_submit),
        )
        .route("/admin/logout", post(handlers::logout))
        .route("/admin/", get(handlers::dashboard).route_layer(gate.clone()))
        .route(
            "/admin/accounts",
            get(handlers::accounts_list)
                .post(handlers::accounts_create)
                .route_layer(gate.clone()),
        )
        .route(
            "/admin/accounts/:name/rotate",
            post(handlers::accounts_rotate).route_layer(gate.clone()),
        )
        .route(
            "/admin/accounts/:name/toggle",
            post(handlers::accounts_toggle).route_layer(gate.clone()),
        )
        .route(
            "/admin/accounts/:name/delete",
            post(handlers::accounts_delete).route_layer(gate.clone()),
        )
        .route(
            "/admin/audit",
            get(handlers::audit_list).route_layer(gate),
        )
        .with_state(state)
}
