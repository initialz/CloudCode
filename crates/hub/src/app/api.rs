//! User app JSON API. Backs the webterm SPA in `webterm/`. Every
//! endpoint lives under `/api/`.
//!
//! Response shape:
//!   - Success: 2xx with whatever JSON the endpoint advertises.
//!   - Error:   non-2xx with `{ "error": "code", "message": "..." }`.
//!
//! Cookie attributes: `Path=/` (the cookie is read on `/api/*` *and*
//! on the `/v1/pty/ws` WS upgrade — both endpoints live on the main
//! listener), `HttpOnly` (no JS access — XSS in webterm can't
//! exfiltrate it), `SameSite=Strict` (no cross-origin sends — a third
//! party can't trick the user's browser into spending the session).

use super::{AuthedAccount, USER_SESSION_COOKIE};
use crate::auth;
use crate::AppState;
use axum::{
    extract::{Extension, State},
    http::{
        header::{AUTHORIZATION, SET_COOKIE},
        HeaderMap, HeaderValue, StatusCode,
    },
    response::{IntoResponse, Response},
    Json,
};
use serde::Deserialize;
use serde_json::json;
use std::sync::Arc;

/// 12-hour TTL, matching the admin session. Webterm sessions are
/// interactive — long enough to survive a workday, short enough that
/// a stolen laptop doesn't grant indefinite access.
const SESSION_TTL_SECS: i64 = 60 * 60 * 12;

fn err(status: StatusCode, code: &str, message: impl Into<String>) -> Response {
    let body = json!({ "error": code, "message": message.into() });
    (status, Json(body)).into_response()
}

#[derive(Deserialize)]
pub struct LoginRequest {
    pub token: String,
}

/// `POST /api/login`
///
/// Body: `{"token":"cc_..."}`. We reuse `crate::auth::authenticate`
/// (same code path the CLI client and the pty WS Hello frame go
/// through), packing the body token into an Authorization header so
/// the helper sees it.
///
/// On success: set the session cookie and return the account name +
/// hub version (cuts a follow-up `/me` round-trip on first paint).
pub async fn login(State(state): State<Arc<AppState>>, Json(req): Json<LoginRequest>) -> Response {
    let token = req.token.trim().to_string();
    if token.is_empty() {
        return err(
            StatusCode::BAD_REQUEST,
            "invalid_input",
            "token is required",
        );
    }
    let mut headers = HeaderMap::new();
    let bearer = match HeaderValue::from_str(&format!("Bearer {}", token)) {
        Ok(v) => v,
        Err(_) => {
            return err(
                StatusCode::BAD_REQUEST,
                "invalid_input",
                "token contains invalid characters",
            );
        }
    };
    headers.insert(AUTHORIZATION, bearer);

    let account = match auth::authenticate(&state.db, &headers).await {
        Ok(a) => a,
        Err(reason) => {
            return err(StatusCode::UNAUTHORIZED, "invalid_token", reason);
        }
    };

    let sid = state.user_auth.login(account.name.clone()).await;
    let cookie = format!(
        "{name}={sid}; HttpOnly; SameSite=Strict; Path=/; Max-Age={ttl}",
        name = USER_SESSION_COOKIE,
        sid = sid,
        ttl = SESSION_TTL_SECS,
    );
    let mut out = HeaderMap::new();
    // unwrap: cookie value uses only URL-safe-base64 + ASCII format.
    out.insert(SET_COOKIE, cookie.parse().unwrap());
    (
        StatusCode::OK,
        out,
        Json(json!({
            "ok": true,
            "account": account.name,
            "hub_version": env!("CARGO_PKG_VERSION"),
        })),
    )
        .into_response()
}

/// `POST /api/logout`
///
/// Idempotent: best-effort remove the session from the store, always
/// emit a cookie with `Max-Age=0` so the browser drops it even if the
/// id was already gone (e.g. after a hub restart).
pub async fn logout(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    if let Some(sid) = super::parse_cookie(&headers, USER_SESSION_COOKIE) {
        state.user_auth.logout(&sid).await;
    }
    let cookie = format!(
        "{name}=; HttpOnly; SameSite=Strict; Path=/; Max-Age=0",
        name = USER_SESSION_COOKIE,
    );
    let mut out = HeaderMap::new();
    out.insert(SET_COOKIE, cookie.parse().unwrap());
    (StatusCode::NO_CONTENT, out).into_response()
}

/// `GET /api/me` — protected by `require_user`. Returns the
/// current account name and hub build version so the webterm can show
/// "you're logged in as X" without re-deriving from cookies.
pub async fn me(Extension(account): Extension<AuthedAccount>) -> Response {
    (
        StatusCode::OK,
        Json(json!({
            "account": account.0,
            "hub_version": env!("CARGO_PKG_VERSION"),
        })),
    )
        .into_response()
}

/// `GET /api/preferences` — return the raw JSON blob the webterm
/// last saved for this account. `preferences == null` means "never set"
/// (webterm then falls back to its built-in defaults).
pub async fn get_preferences(
    State(state): State<Arc<AppState>>,
    Extension(account): Extension<AuthedAccount>,
) -> Response {
    let blob = match state.db.get_user_preferences(&account.0).await {
        Ok(b) => b,
        Err(e) => {
            tracing::warn!(error = %e, "get_user_preferences failed");
            return err(StatusCode::INTERNAL_SERVER_ERROR, "db_error", "db error");
        }
    };
    // Re-parse so we hand the SPA back JSON instead of a string-of-JSON.
    // If the stored row is somehow malformed, surface that as null so
    // the client falls back to defaults rather than crashing.
    let parsed: Option<serde_json::Value> = blob
        .as_deref()
        .and_then(|s| serde_json::from_str(s).ok());
    (
        StatusCode::OK,
        Json(json!({ "preferences": parsed })),
    )
        .into_response()
}

/// `PUT /api/preferences` — replace this account's preferences
/// blob. Body must be a JSON object (we explicitly reject arrays /
/// primitives to keep the door open for partial-update semantics
/// later without an awkward type bump).
pub async fn put_preferences(
    State(state): State<Arc<AppState>>,
    Extension(account): Extension<AuthedAccount>,
    Json(body): Json<serde_json::Value>,
) -> Response {
    if !body.is_object() {
        return err(
            StatusCode::BAD_REQUEST,
            "invalid_input",
            "preferences body must be a JSON object",
        );
    }
    // Cap to a generous-but-finite size so a runaway client can't
    // turn this into a DoS vector. 32 KiB serialised JSON fits orders
    // of magnitude more than the realistic settings surface.
    let serialised = body.to_string();
    if serialised.len() > 32 * 1024 {
        return err(
            StatusCode::PAYLOAD_TOO_LARGE,
            "too_large",
            "preferences exceed 32 KiB",
        );
    }
    if let Err(e) = state.db.set_user_preferences(&account.0, &serialised).await {
        tracing::warn!(error = %e, "set_user_preferences failed");
        return err(StatusCode::INTERNAL_SERVER_ERROR, "db_error", "db error");
    }
    StatusCode::NO_CONTENT.into_response()
}
