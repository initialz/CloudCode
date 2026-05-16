//! User app JSON API. Backs the webterm SPA in `webterm/`. Every
//! endpoint lives under `/app/api/`.
//!
//! Response shape:
//!   - Success: 2xx with whatever JSON the endpoint advertises.
//!   - Error:   non-2xx with `{ "error": "code", "message": "..." }`.
//!
//! Cookie attributes: `Path=/` (the cookie is read on `/app/api/*`
//! *and* on the `/v1/pty/ws` WS upgrade — both endpoints live on the
//! main listener), `HttpOnly` (no JS access — XSS in webterm can't
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

/// `POST /app/api/login`
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

    let sid = state.user_auth.login(account.name.clone());
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

/// `POST /app/api/logout`
///
/// Idempotent: best-effort remove the session from the store, always
/// emit a cookie with `Max-Age=0` so the browser drops it even if the
/// id was already gone (e.g. after a hub restart).
pub async fn logout(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    if let Some(sid) = super::parse_cookie(&headers, USER_SESSION_COOKIE) {
        state.user_auth.logout(&sid);
    }
    let cookie = format!(
        "{name}=; HttpOnly; SameSite=Strict; Path=/; Max-Age=0",
        name = USER_SESSION_COOKIE,
    );
    let mut out = HeaderMap::new();
    out.insert(SET_COOKIE, cookie.parse().unwrap());
    (StatusCode::NO_CONTENT, out).into_response()
}

/// `GET /app/api/me` — protected by `require_user`. Returns the
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
