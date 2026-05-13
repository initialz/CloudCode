//! Admin server — JSON API for the React SPA in `admin-ui/`.
//!
//! Mounted by `serve()` in main.rs when `[admin].token_hash` is set.
//! Lives on its own HTTP listener (default 127.0.0.1:7101). The single
//! shared admin login token (argon2id-hashed in hub.toml) mints
//! in-memory session ids on `POST /admin/api/login`; the id rides in an
//! `HttpOnly` cookie and authenticates all other `/admin/api/*` calls.
//! Sessions don't survive a hub restart — operator re-logs in.
//!
//! `/admin` and `/admin/*` (anything non-/api) serves the SPA shell
//! (M8 will embed the Vite build; until then it's a placeholder).

mod api;

use crate::auth;
use crate::AppState;
use axum::{
    extract::{Request, State},
    http::{header::COOKIE, HeaderMap, StatusCode},
    middleware::{self, Next},
    response::{Html, IntoResponse, Response},
    routing::{delete, get, post},
    Json, Router,
};
use dashmap::DashMap;
use serde_json::json;
use std::sync::Arc;

pub const SESSION_COOKIE: &str = "cc_admin";

#[derive(Clone)]
pub struct AdminState {
    pub app: Arc<AppState>,
    pub auth: Arc<AdminAuth>,
}

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

/// Reject unauthenticated `/admin/api/*` traffic with a 401 JSON envelope
/// instead of redirecting (SPA handles redirect itself).
async fn require_admin(State(state): State<AdminState>, req: Request, next: Next) -> Response {
    if let Some(sid) = session_cookie(req.headers()) {
        if state.auth.is_valid(&sid) {
            return next.run(req).await;
        }
    }
    (
        StatusCode::UNAUTHORIZED,
        Json(json!({"error": "unauthenticated", "message": "login required"})),
    )
        .into_response()
}

pub fn router(state: AdminState) -> Router {
    let gate = middleware::from_fn_with_state(state.clone(), require_admin);

    Router::new()
        // -- auth (unauthenticated) --
        .route("/admin/api/login", post(api::login))
        .route("/admin/api/logout", post(api::logout))
        // -- protected api --
        .route("/admin/api/me", get(api::me).route_layer(gate.clone()))
        .route(
            "/admin/api/dashboard",
            get(api::dashboard).route_layer(gate.clone()),
        )
        .route(
            "/admin/api/sessions/hourly",
            get(api::sessions_hourly).route_layer(gate.clone()),
        )
        .route(
            "/admin/api/accounts",
            get(api::accounts_list)
                .post(api::accounts_create)
                .route_layer(gate.clone()),
        )
        .route(
            "/admin/api/accounts/:name/rotate",
            post(api::accounts_rotate).route_layer(gate.clone()),
        )
        .route(
            "/admin/api/accounts/:name/toggle",
            post(api::accounts_toggle).route_layer(gate.clone()),
        )
        .route(
            "/admin/api/accounts/:name",
            delete(api::accounts_delete).route_layer(gate.clone()),
        )
        .route(
            "/admin/api/audit",
            get(api::audit_list).route_layer(gate.clone()),
        )
        .route(
            "/admin/api/audit/kinds",
            get(api::audit_kinds).route_layer(gate.clone()),
        )
        .route(
            "/admin/api/sessions",
            get(api::sessions_list).route_layer(gate),
        )
        // -- SPA shell (placeholder until M8 embeds the Vite build) --
        .route("/admin", get(spa_placeholder))
        .route("/admin/", get(spa_placeholder))
        .route("/admin/*spa", get(spa_placeholder))
        .with_state(state)
}

async fn spa_placeholder() -> Html<&'static str> {
    Html(r#"<!doctype html>
<html lang="en"><head><meta charset="utf-8">
<title>cloudcode admin</title>
<style>body{font-family:system-ui,sans-serif;max-width:40rem;margin:4rem auto;padding:0 1rem;color-scheme:light dark;}</style>
</head><body>
<h1>cloudcode admin</h1>
<p>The new React-based admin UI is being built. The JSON API is already live at <code>/admin/api/*</code>.</p>
<p>Try: <code>curl -X POST http://127.0.0.1:7101/admin/api/login -H 'Content-Type: application/json' -d '{"token":"&lt;admin-token&gt;"}'</code></p>
</body></html>"#)
}
