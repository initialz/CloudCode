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
mod assets;

pub use api::ReleasesCache;

use crate::auth;
use crate::AppState;
use axum::{
    extract::{Request, State},
    http::{header::COOKIE, HeaderMap, StatusCode},
    middleware::{self, Next},
    response::{IntoResponse, Redirect, Response},
    routing::{delete, get, post, put},
    Json, Router,
};
use serde_json::json;
use std::sync::Arc;

pub const SESSION_COOKIE: &str = "cc_admin";

#[derive(Clone)]
pub struct AdminState {
    pub app: Arc<AppState>,
    pub auth: Arc<AdminAuth>,
    /// Cached GitHub releases for the agent self-update flow. Refreshed
    /// lazily on first hit + every 30 min after that. `None` until the
    /// first fetch; an outer Result lets us surface fetch errors without
    /// trampling the previous cached value.
    pub releases: Arc<api::ReleasesCache>,
}

/// Session TTL applied both to the cookie's Max-Age and the DB row.
/// Operator workday-friendly, short enough that a stolen cookie
/// doesn't grant indefinite access.
pub const ADMIN_SESSION_TTL_SECS: i64 = 12 * 60 * 60;

pub struct AdminAuth {
    db: crate::db::Db,
    token_hash: String,
}

impl AdminAuth {
    pub fn new(db: crate::db::Db, token_hash: String) -> Self {
        Self { db, token_hash }
    }

    /// Verify the supplied plaintext token against the stored hash;
    /// on success mint a fresh sid, persist it in `admin_sessions`,
    /// and hand it back to the caller (who then sets the cookie).
    /// Returns `None` on token-mismatch or DB error.
    pub async fn login(&self, plaintext: &str) -> Option<String> {
        if !auth::verify_token(plaintext, &self.token_hash) {
            return None;
        }
        let sid = auth::generate_session_id();
        let expires_at = chrono::Utc::now().timestamp() + ADMIN_SESSION_TTL_SECS;
        if let Err(e) = self.db.insert_admin_session(&sid, expires_at).await {
            tracing::warn!(error = %e, "could not persist admin session; user must re-login");
            return None;
        }
        Some(sid)
    }

    pub async fn is_valid(&self, sid: &str) -> bool {
        self.db.valid_admin_session(sid).await.unwrap_or(false)
    }

    pub async fn logout(&self, sid: &str) {
        let _ = self.db.delete_admin_session(sid).await;
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
        if state.auth.is_valid(&sid).await {
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
            "/admin/api/accounts/:name/sandbox",
            post(api::accounts_sandbox_toggle).route_layer(gate.clone()),
        )
        .route(
            "/admin/api/accounts/:name/disconnect",
            post(api::accounts_disconnect).route_layer(gate.clone()),
        )
        .route(
            "/admin/api/accounts/:name/real-name",
            put(api::accounts_update_real_name).route_layer(gate.clone()),
        )
        .route(
            "/admin/api/accounts/:name",
            delete(api::accounts_delete).route_layer(gate.clone()),
        )
        .route(
            "/admin/api/accounts/:name/allowed-agents",
            get(api::account_allowed_agents_get)
                .put(api::account_allowed_agents_set)
                .route_layer(gate.clone()),
        )
        // -- invite links (admin issues shareable URLs that mint
        // brand-new accounts on accept; see api.rs) --
        .route(
            "/admin/api/invites",
            get(api::invites_list)
                .post(api::invites_create)
                .route_layer(gate.clone()),
        )
        .route(
            "/admin/api/invites/:id",
            axum::routing::patch(api::invites_patch)
                .delete(api::invites_delete)
                .route_layer(gate.clone()),
        )
        .route(
            "/admin/api/invites/:id/acceptances",
            get(api::invites_acceptances).route_layer(gate.clone()),
        )
        .route(
            "/admin/api/agents",
            get(api::agents_list).route_layer(gate.clone()),
        )
        .route(
            "/admin/api/agents/releases",
            get(api::agents_releases).route_layer(gate.clone()),
        )
        .route(
            "/admin/api/agents/:name/allowed-accounts",
            get(api::agent_allowed_accounts_get)
                .put(api::agent_allowed_accounts_set)
                .route_layer(gate.clone()),
        )
        .route(
            "/admin/api/agents/:name/update",
            post(api::agent_update).route_layer(gate.clone()),
        )
        .route(
            "/admin/api/hub/update",
            post(api::hub_update).route_layer(gate.clone()),
        )
        // Public on purpose — the admin SPA needs to poll this
        // while the hub is restarting (in-memory sessions are gone,
        // so gated endpoints 401 forever).
        .route("/admin/api/hub-version", get(api::hub_version))
        .route(
            "/admin/api/agents/:name",
            delete(api::agent_delete).route_layer(gate.clone()),
        )
        .route(
            "/admin/api/workspaces",
            get(api::workspaces_list).route_layer(gate.clone()),
        )
        .route(
            "/admin/api/workspaces/delete",
            post(api::workspace_delete).route_layer(gate.clone()),
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
            "/admin/api/interactions",
            get(api::interactions_list).route_layer(gate.clone()),
        )
        .route(
            "/admin/api/interactions/:id/reveal",
            post(api::interactions_reveal).route_layer(gate.clone()),
        )
        .route(
            "/admin/api/activity",
            get(api::activity_list).route_layer(gate.clone()),
        )
        .route(
            "/admin/api/activity/kinds",
            get(api::activity_kinds).route_layer(gate.clone()),
        )
        .route(
            "/admin/api/sessions",
            get(api::sessions_list).route_layer(gate.clone()),
        )
        .route(
            "/admin/api/sessions/:id",
            get(api::session_detail).route_layer(gate.clone()),
        )
        .route(
            "/admin/api/sessions/:id/messages",
            get(api::session_messages).route_layer(gate.clone()),
        )
        // -- stats --
        .route(
            "/admin/api/stats/leaderboard",
            get(api::stats_leaderboard).route_layer(gate.clone()),
        )
        .route(
            "/admin/api/stats/session-duration",
            get(api::stats_session_duration).route_layer(gate.clone()),
        )
        .route(
            "/admin/api/stats/messages-daily",
            get(api::stats_messages_daily).route_layer(gate.clone()),
        )
        .route(
            "/admin/api/stats/messages-per-session",
            get(api::stats_messages_per_session).route_layer(gate.clone()),
        )
        .route(
            "/admin/api/stats/tokens-daily",
            get(api::stats_tokens_daily).route_layer(gate),
        )
        // -- SPA bundle (built by `cd admin-ui && npm run build`) --
        // /admin/assets/<hash>.{js,css,...} → long-cache hashed file
        // anything else under /admin → index.html, SPA router handles
        .route("/admin/assets/*path", get(assets::serve_asset))
        // React Router uses basename="/admin", and when the location
        // is exactly the basename (no trailing slash) it can't strip
        // it to a real path — the SPA loads but the index route
        // doesn't match, so any in-browser reload at /admin lands
        // on a blank screen. Force the canonical trailing slash.
        .route("/admin", get(|| async { Redirect::permanent("/admin/") }))
        .route("/admin/", get(assets::serve_index))
        .route("/admin/*spa", get(assets::serve_spa))
        .with_state(state)
}
