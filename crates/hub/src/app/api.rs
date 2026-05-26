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
use crate::tunnel::{ClientMsg, ServerMsg};
use crate::AppState;
use axum::{
    body::Body,
    extract::{Extension, Query, State},
    http::{
        header::{CONTENT_DISPOSITION, CONTENT_TYPE, SET_COOKIE},
        HeaderMap, StatusCode,
    },
    response::{IntoResponse, Response},
    Json,
};
use base64::Engine;
use bytes::Bytes;
use futures::stream::StreamExt;
use serde::Deserialize;
use serde_json::json;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{mpsc, oneshot};
use uuid::Uuid;

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
    pub username: String,
    pub token: String,
}

/// `POST /api/login`
///
/// Body: `{"username":"alice","token":"cc_..."}`. Looks the account
/// up by name, then argon2-verifies the token — single hash check
/// instead of the WS Hello frame's all-rows scan.
///
/// On success: set the session cookie and return the account name +
/// hub version (cuts a follow-up `/me` round-trip on first paint).
pub async fn login(State(state): State<Arc<AppState>>, Json(req): Json<LoginRequest>) -> Response {
    let username = req.username.trim().to_string();
    let token = req.token.trim().to_string();
    if username.is_empty() || token.is_empty() {
        return err(
            StatusCode::BAD_REQUEST,
            "invalid_input",
            "username and token are required",
        );
    }
    let account = match auth::authenticate_account(&state.db, &username, &token).await {
        Ok(a) => a,
        Err(reason) => {
            return err(StatusCode::UNAUTHORIZED, "invalid_credentials", reason);
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
/// current account name, hub build version, and optional real_name so
/// the webterm can show "you're logged in as X" without re-deriving
/// from cookies.
pub async fn me(
    State(state): State<Arc<AppState>>,
    Extension(account): Extension<AuthedAccount>,
) -> Response {
    let real_name = state
        .db
        .get_account(&account.0)
        .await
        .ok()
        .flatten()
        .and_then(|a| a.real_name);
    (
        StatusCode::OK,
        Json(json!({
            "account": account.0,
            "hub_version": env!("CARGO_PKG_VERSION"),
            "real_name": real_name,
        })),
    )
        .into_response()
}

/// `PUT /api/me` — lets the logged-in user update their own real_name.
#[derive(Deserialize)]
pub struct UpdateMeRequest {
    pub real_name: Option<String>,
}

pub async fn update_me(
    State(state): State<Arc<AppState>>,
    Extension(account): Extension<AuthedAccount>,
    Json(req): Json<UpdateMeRequest>,
) -> Response {
    if let Some(ref rn) = req.real_name {
        if rn.len() > 128 {
            return err(
                StatusCode::BAD_REQUEST,
                "invalid_input",
                "real_name too long (max 128)",
            );
        }
    }
    if let Err(e) = state
        .db
        .update_account_real_name(&account.0, req.real_name.as_deref())
        .await
    {
        tracing::warn!(error = %e, "update_account_real_name failed");
        return err(StatusCode::INTERNAL_SERVER_ERROR, "db_error", "db error");
    }
    StatusCode::NO_CONTENT.into_response()
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

// ---------------------------------------------------------------------------
// File-manager endpoints (v1.15 / protocol v9)
//
// Both /api/files/list and /api/files/download cookie-auth via the
// same `require_user` middleware as /api/me, then enforce two
// account-scoped checks before fanning out to the agent:
//   1. is_agent_allowed(account, agent): the account has been
//      whitelisted on this agent at all.
//   2. get_workspace_agent(account, agent, workspace): the workspace
//      exists for this (account, agent) pair.
// Without (2) a user with broad agent access could enumerate other
// users' workspaces by guessing names. Don't relax either check
// without re-reading this comment.
// ---------------------------------------------------------------------------

/// Wait at most this long for an `FsListResult` on the workspace
/// one-shot. Aligned with the existing workspace_request_timeout
/// constants used elsewhere; longer than the agent's typical 1-2s
/// fs walk, short enough to fail fast on a hung agent.
const FS_LIST_TIMEOUT: Duration = Duration::from_secs(10);

/// Idle timeout between consecutive `FsReadChunk` frames during a
/// download. Generous compared to the agent's ~5s ping interval, so
/// a healthy connection won't hit it; if it fires, the agent is
/// stuck and we'd rather short-read the response than hang on the
/// client forever.
const FS_READ_IDLE_TIMEOUT: Duration = Duration::from_secs(30);

/// mpsc capacity for the FsRead stream. Tunable — too small wastes
/// awakenings, too big lets the agent buffer a lot of memory if the
/// HTTP consumer is slow. 16 chunks * ~64 KiB ≈ 1 MiB max in-flight.
const FS_READ_CHANNEL_CAP: usize = 16;

#[derive(Deserialize)]
pub struct FsListQuery {
    pub agent: String,
    pub workspace: String,
    #[serde(default)]
    pub path: Option<String>,
    #[serde(default)]
    pub show_hidden: Option<String>,
}

#[derive(Deserialize)]
pub struct FsDownloadQuery {
    pub agent: String,
    pub workspace: String,
    pub path: String,
}

#[derive(Deserialize)]
pub struct FsArchiveQuery {
    pub agent: String,
    pub workspace: String,
    /// Comma-separated workspace-relative paths (files and/or dirs).
    pub paths: String,
}

/// Parse the truthy variants we accept for `show_hidden`. "1" /
/// "true" / "yes" all opt in; anything else (including absent) means
/// off. Lower-case'd before comparing so the query string can be
/// any-case.
fn truthy(s: &str) -> bool {
    matches!(s.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes")
}

/// Authorize an (account, agent, workspace) tuple. Returns `Ok(())`
/// on pass, or a ready-to-return error `Response` describing exactly
/// what failed. Centralised so list + download stay in lockstep.
async fn authorize_workspace(
    state: &Arc<AppState>,
    account: &str,
    agent: &str,
    workspace: &str,
) -> Result<(), Response> {
    match state.db.is_agent_allowed(account, agent).await {
        Ok(true) => {}
        Ok(false) => {
            return Err(err(
                StatusCode::FORBIDDEN,
                "forbidden",
                "account is not allowed on this agent",
            ));
        }
        Err(e) => {
            tracing::warn!(error = %e, "is_agent_allowed failed");
            return Err(err(StatusCode::INTERNAL_SERVER_ERROR, "db_error", "db error"));
        }
    }
    match state.db.get_workspace_agent(account, agent, workspace).await {
        Ok(Some(_)) => Ok(()),
        Ok(None) => Err(err(
            StatusCode::FORBIDDEN,
            "forbidden",
            "workspace not found for this account/agent",
        )),
        Err(e) => {
            tracing::warn!(error = %e, "get_workspace_agent failed");
            Err(err(StatusCode::INTERNAL_SERVER_ERROR, "db_error", "db error"))
        }
    }
}

/// `GET /api/files/list?agent=A&workspace=W&path=src/&show_hidden=1`
///
/// Lists one directory inside (account, agent, workspace). Returns a
/// 200 with `{ entries, error }` even when the agent surfaces an
/// error string (e.g. "path escapes workspace") so the SPA can
/// display it inline; non-200 status codes are reserved for auth /
/// transport / availability failures.
pub async fn files_list(
    State(state): State<Arc<AppState>>,
    Extension(account): Extension<AuthedAccount>,
    Query(q): Query<FsListQuery>,
) -> Response {
    let account = account.0;
    if let Err(resp) = authorize_workspace(&state, &account, &q.agent, &q.workspace).await {
        return resp;
    }
    let Some(conn) = state.registry.get(&q.agent) else {
        return err(StatusCode::NOT_FOUND, "agent_offline", "agent is not connected");
    };

    let request_id = Uuid::new_v4();
    let (tx, rx) = oneshot::channel();
    conn.register_workspace_request(request_id, tx);

    let path = q.path.unwrap_or_default();
    let show_hidden = q.show_hidden.as_deref().map(truthy).unwrap_or(false);
    if conn
        .send(ServerMsg::FsList {
            request_id,
            account: account.clone(),
            workspace: q.workspace.clone(),
            path,
            show_hidden,
        })
        .await
        .is_err()
    {
        return err(
            StatusCode::SERVICE_UNAVAILABLE,
            "agent_offline",
            "agent disconnected before request was sent",
        );
    }

    match tokio::time::timeout(FS_LIST_TIMEOUT, rx).await {
        Ok(Ok(ClientMsg::FsListResult { entries, error, .. })) => (
            StatusCode::OK,
            Json(json!({ "entries": entries, "error": error })),
        )
            .into_response(),
        Ok(Ok(_)) => err(
            StatusCode::BAD_GATEWAY,
            "unexpected_reply",
            "agent returned an unexpected frame",
        ),
        Ok(Err(_)) => err(
            StatusCode::SERVICE_UNAVAILABLE,
            "agent_offline",
            "agent disconnected before reply",
        ),
        Err(_) => err(
            StatusCode::BAD_GATEWAY,
            "agent_timeout",
            "agent did not reply in time",
        ),
    }
}

/// Build a safe `Content-Disposition` header value for a download.
/// Uses the RFC 6266 / RFC 5987 syntax with a `filename*=UTF-8''…`
/// parameter so non-ASCII names ("中文文件.txt") survive intact; the
/// plain `filename=` parameter falls back to a sanitised ASCII form
/// for legacy clients. Control chars / quotes / CR-LF that would
/// break the header are stripped from both.
fn content_disposition_for(path: &str) -> String {
    let basename = path
        .rsplit(|c| c == '/' || c == '\\')
        .next()
        .filter(|s| !s.is_empty())
        .unwrap_or("download");

    // Strip header-breaking bytes from the literal-ASCII fallback.
    // Quote, semicolon and backslash terminate / escape the filename
    // parameter in RFC 6266; CR/LF inject header smuggling; control
    // chars are forbidden in header values outright.
    let safe_ascii: String = basename
        .chars()
        .filter(|c| {
            !c.is_control()
                && *c != '"'
                && *c != '\\'
                && *c != ';'
                && *c != '\r'
                && *c != '\n'
                // Non-ASCII would also be unsafe inside the literal
                // `filename=`; the RFC 5987 form handles them
                // instead.
                && c.is_ascii()
        })
        .collect();
    let safe_ascii = if safe_ascii.is_empty() {
        "download".to_string()
    } else {
        safe_ascii
    };

    // RFC 5987 percent-encodes everything outside the small attr-char
    // set. Strip control chars *before* encoding so they can't slip
    // through as %XX in the header.
    let cleaned: String = basename
        .chars()
        .filter(|c| !c.is_control() && *c != '\r' && *c != '\n')
        .collect();
    let encoded = percent_encode_rfc5987(&cleaned);

    format!(
        "attachment; filename=\"{}\"; filename*=UTF-8''{}",
        safe_ascii, encoded
    )
}

/// RFC 5987 percent-encoding for `value-chars`. Encodes anything
/// that isn't an attr-char (`A-Z a-z 0-9 ! # $ & + - . ^ _ ` | ~`).
fn percent_encode_rfc5987(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for b in input.as_bytes() {
        let c = *b;
        let is_attr_char = c.is_ascii_alphanumeric()
            || matches!(
                c,
                b'!' | b'#' | b'$' | b'&' | b'+' | b'-' | b'.'
                    | b'^' | b'_' | b'`' | b'|' | b'~'
            );
        if is_attr_char {
            out.push(c as char);
        } else {
            out.push_str(&format!("%{:02X}", c));
        }
    }
    out
}

/// `GET /api/files/download?agent=A&workspace=W&path=src/main.rs`
///
/// Streams a workspace file back to the browser as
/// `application/octet-stream`. Backpressure is end-to-end: the agent
/// awaits on its WS send when this handler's mpsc fills, so a slow
/// HTTP consumer doesn't blow up agent memory.
///
/// Failure semantics:
///   - Auth / availability: non-2xx JSON envelope.
///   - Agent error mid-stream: response body is short — there's no
///     way to retroactively change the HTTP status once headers
///     have been sent. Clients should sha256 / size-check the result
///     against `/api/files/list` if integrity matters.
pub async fn files_download(
    State(state): State<Arc<AppState>>,
    Extension(account): Extension<AuthedAccount>,
    Query(q): Query<FsDownloadQuery>,
) -> Response {
    let account = account.0;
    if let Err(resp) = authorize_workspace(&state, &account, &q.agent, &q.workspace).await {
        return resp;
    }
    let Some(conn) = state.registry.get(&q.agent) else {
        return err(StatusCode::NOT_FOUND, "agent_offline", "agent is not connected");
    };

    let request_id = Uuid::new_v4();
    let (tx, rx) = mpsc::channel::<ClientMsg>(FS_READ_CHANNEL_CAP);
    conn.register_fs_read_stream(request_id, tx);

    if conn
        .send(ServerMsg::FsRead {
            request_id,
            account: account.clone(),
            workspace: q.workspace.clone(),
            path: q.path.clone(),
        })
        .await
        .is_err()
    {
        conn.unregister_fs_read_stream(request_id);
        return err(
            StatusCode::SERVICE_UNAVAILABLE,
            "agent_offline",
            "agent disconnected before request was sent",
        );
    }

    // Build the body stream. We carry the conn handle + request_id
    // into the stream so the mpsc entry is cleaned up when the
    // stream terminates (EOF, error, drop). Without this, an HTTP
    // client that hangs up mid-download would leak the entry until
    // the agent next sent a terminal frame.
    let body_stream = futures::stream::unfold(
        (rx, false),
        move |(mut rx, done)| async move {
            if done {
                return None;
            }
            match tokio::time::timeout(FS_READ_IDLE_TIMEOUT, rx.recv()).await {
                Ok(Some(ClientMsg::FsReadChunk {
                    data_b64,
                    eof,
                    error,
                    ..
                })) => {
                    if let Some(msg) = error {
                        tracing::warn!(error = %msg, "fs read agent error; short-reading");
                        return None;
                    }
                    // Decode this chunk. Empty data_b64 + eof = true
                    // is a valid "no tail bytes" terminator.
                    let decoded = if data_b64.is_empty() {
                        Bytes::new()
                    } else {
                        match base64::engine::general_purpose::STANDARD.decode(&data_b64) {
                            Ok(bytes) => Bytes::from(bytes),
                            Err(e) => {
                                tracing::warn!(error = %e, "agent sent invalid base64; short-reading");
                                return None;
                            }
                        }
                    };
                    let next_done = eof;
                    if decoded.is_empty() && next_done {
                        // Don't bother yielding an empty final chunk —
                        // the stream end alone tells axum to flush.
                        return None;
                    }
                    Some((
                        Ok::<Bytes, std::io::Error>(decoded),
                        (rx, next_done),
                    ))
                }
                Ok(Some(_)) => {
                    tracing::warn!("unexpected frame on fs read stream");
                    None
                }
                Ok(None) => None,
                Err(_) => {
                    tracing::warn!("fs read idle timeout; short-reading");
                    None
                }
            }
        },
    );
    // After the stream ends (for any reason), drop the routing
    // entry. Chain a no-op final step that runs cleanup as a side
    // effect of polling the stream to completion.
    let conn_for_cleanup = conn.clone();
    let cleanup = futures::stream::once(async move {
        conn_for_cleanup.unregister_fs_read_stream(request_id);
        // Yield no bytes — this entry exists purely for the side
        // effect of running the cleanup closure on stream poll.
        Ok::<Bytes, std::io::Error>(Bytes::new())
    });
    let body_stream = body_stream.chain(cleanup);

    let mut headers = HeaderMap::new();
    headers.insert(CONTENT_TYPE, "application/octet-stream".parse().unwrap());
    let disposition = content_disposition_for(&q.path);
    match disposition.parse() {
        Ok(v) => {
            headers.insert(CONTENT_DISPOSITION, v);
        }
        Err(e) => {
            // Should be impossible given the sanitiser, but if it
            // happens we'd rather drop the disposition than crash.
            tracing::warn!(error = %e, "could not encode Content-Disposition; falling back");
        }
    }

    (StatusCode::OK, headers, Body::from_stream(body_stream)).into_response()
}

/// `GET /api/files/archive?agent=A&workspace=W&paths=src,Cargo.toml`
///
/// Bundles one or more workspace paths (files and/or directories) into a
/// zip archive and streams it back to the browser. The agent produces
/// `FsReadChunk` frames identical to a single-file download, so the
/// hub's existing `fs_read_streams` routing handles both without changes.
///
/// Failure semantics match `files_download` — once the 200 headers are
/// sent, a mid-stream agent error simply truncates the body.
pub async fn files_archive(
    State(state): State<Arc<AppState>>,
    Extension(account): Extension<AuthedAccount>,
    Query(q): Query<FsArchiveQuery>,
) -> Response {
    let account = account.0;
    if let Err(resp) = authorize_workspace(&state, &account, &q.agent, &q.workspace).await {
        return resp;
    }

    let paths: Vec<String> = q
        .paths
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    if paths.is_empty() {
        return err(
            StatusCode::BAD_REQUEST,
            "invalid_input",
            "at least one path is required",
        );
    }

    let Some(conn) = state.registry.get(&q.agent) else {
        return err(StatusCode::NOT_FOUND, "agent_offline", "agent is not connected");
    };

    let request_id = Uuid::new_v4();
    let (tx, rx) = mpsc::channel::<ClientMsg>(FS_READ_CHANNEL_CAP);
    conn.register_fs_read_stream(request_id, tx);

    if conn
        .send(ServerMsg::FsArchive {
            request_id,
            account: account.clone(),
            workspace: q.workspace.clone(),
            paths: paths.clone(),
        })
        .await
        .is_err()
    {
        conn.unregister_fs_read_stream(request_id);
        return err(
            StatusCode::SERVICE_UNAVAILABLE,
            "agent_offline",
            "agent disconnected before request was sent",
        );
    }

    // Body stream — identical to files_download; FsReadChunk frames
    // carry the zip bytes instead of raw file bytes.
    let body_stream = futures::stream::unfold(
        (rx, false),
        move |(mut rx, done)| async move {
            if done {
                return None;
            }
            match tokio::time::timeout(FS_READ_IDLE_TIMEOUT, rx.recv()).await {
                Ok(Some(ClientMsg::FsReadChunk {
                    data_b64,
                    eof,
                    error,
                    ..
                })) => {
                    if let Some(msg) = error {
                        tracing::warn!(error = %msg, "fs archive agent error; short-reading");
                        return None;
                    }
                    let decoded = if data_b64.is_empty() {
                        Bytes::new()
                    } else {
                        match base64::engine::general_purpose::STANDARD.decode(&data_b64) {
                            Ok(bytes) => Bytes::from(bytes),
                            Err(e) => {
                                tracing::warn!(error = %e, "agent sent invalid base64; short-reading");
                                return None;
                            }
                        }
                    };
                    let next_done = eof;
                    if decoded.is_empty() && next_done {
                        return None;
                    }
                    Some((
                        Ok::<Bytes, std::io::Error>(decoded),
                        (rx, next_done),
                    ))
                }
                Ok(Some(_)) => {
                    tracing::warn!("unexpected frame on fs archive stream");
                    None
                }
                Ok(None) => None,
                Err(_) => {
                    tracing::warn!("fs archive idle timeout; short-reading");
                    None
                }
            }
        },
    );
    let conn_for_cleanup = conn.clone();
    let cleanup = futures::stream::once(async move {
        conn_for_cleanup.unregister_fs_read_stream(request_id);
        Ok::<Bytes, std::io::Error>(Bytes::new())
    });
    let body_stream = body_stream.chain(cleanup);

    // Derive the zip filename from the requested paths.
    let zip_filename = if paths.len() == 1 {
        let basename = paths[0]
            .rsplit(|c| c == '/' || c == '\\')
            .next()
            .filter(|s| !s.is_empty())
            .unwrap_or("archive");
        format!("{}.zip", basename)
    } else {
        "archive.zip".to_string()
    };

    let mut headers = HeaderMap::new();
    headers.insert(CONTENT_TYPE, "application/zip".parse().unwrap());
    let disposition = content_disposition_for(&zip_filename);
    match disposition.parse() {
        Ok(v) => {
            headers.insert(CONTENT_DISPOSITION, v);
        }
        Err(e) => {
            tracing::warn!(error = %e, "could not encode Content-Disposition; falling back");
        }
    }

    (StatusCode::OK, headers, Body::from_stream(body_stream)).into_response()
}

#[derive(Deserialize)]
pub struct FsUploadQuery {
    pub agent: String,
    pub workspace: String,
    #[serde(default)]
    pub path: String,
}

/// `POST /api/files/upload?agent=A&workspace=W&path=target/dir/`
///
/// Receives a multipart/form-data body with one or more file fields and
/// streams each file to the agent via `FsWriteInit` + `FsWriteChunk`
/// frames. Returns a JSON array of per-file results.
pub async fn files_upload(
    State(state): State<Arc<AppState>>,
    Extension(account): Extension<AuthedAccount>,
    Query(q): Query<FsUploadQuery>,
    mut multipart: axum::extract::Multipart,
) -> Response {
    let account = account.0;
    if let Err(resp) = authorize_workspace(&state, &account, &q.agent, &q.workspace).await {
        return resp;
    }
    let Some(conn) = state.registry.get(&q.agent) else {
        return err(StatusCode::NOT_FOUND, "agent_offline", "agent is not connected");
    };

    // Normalize target directory: ensure trailing '/' if non-empty.
    let dir = if q.path.is_empty() {
        String::new()
    } else if q.path.ends_with('/') {
        q.path.clone()
    } else {
        format!("{}/", q.path)
    };

    let mut results: Vec<serde_json::Value> = Vec::new();

    loop {
        let mut field = match multipart.next_field().await {
            Ok(Some(f)) => f,
            Ok(None) => break,
            Err(e) => {
                return err(
                    StatusCode::BAD_REQUEST,
                    "multipart_error",
                    format!("failed to read multipart field: {e}"),
                );
            }
        };

        let filename = match field.file_name() {
            Some(name) => name.to_string(),
            None => continue, // skip non-file fields
        };

        let target_path = format!("{dir}{filename}");
        let request_id = Uuid::new_v4();

        // Register a workspace-request oneshot for the FsWriteResult.
        let (tx, rx) = oneshot::channel();
        conn.register_workspace_request(request_id, tx);

        // Send FsWriteInit to agent.
        if conn
            .send(ServerMsg::FsWriteInit {
                request_id,
                account: account.clone(),
                workspace: q.workspace.clone(),
                path: target_path.clone(),
                size: 0, // content-length unknown from multipart
            })
            .await
            .is_err()
        {
            results.push(json!({
                "name": filename,
                "bytes_written": 0,
                "error": "agent disconnected before init was sent",
            }));
            continue;
        }

        // Stream chunks (64 KiB each) from the multipart field to the agent.
        let mut send_err: Option<String> = None;
        loop {
            match field.chunk().await {
                Ok(Some(chunk_bytes)) => {
                    let b64 =
                        base64::engine::general_purpose::STANDARD.encode(&chunk_bytes);
                    if conn
                        .send(ServerMsg::FsWriteChunk {
                            request_id,
                            data_b64: b64,
                            eof: false,
                        })
                        .await
                        .is_err()
                    {
                        send_err = Some("agent disconnected during upload".into());
                        break;
                    }
                }
                Ok(None) => {
                    // End of this file part — send terminal chunk.
                    if conn
                        .send(ServerMsg::FsWriteChunk {
                            request_id,
                            data_b64: String::new(),
                            eof: true,
                        })
                        .await
                        .is_err()
                    {
                        send_err = Some("agent disconnected at eof".into());
                    }
                    break;
                }
                Err(e) => {
                    // Multipart read error — still send eof so agent cleans up.
                    let _ = conn
                        .send(ServerMsg::FsWriteChunk {
                            request_id,
                            data_b64: String::new(),
                            eof: true,
                        })
                        .await;
                    send_err = Some(format!("multipart read error: {e}"));
                    break;
                }
            }
        }

        if let Some(e) = send_err {
            results.push(json!({
                "name": filename,
                "bytes_written": 0,
                "error": e,
            }));
            continue;
        }

        // Wait for agent FsWriteResult.
        match tokio::time::timeout(FS_READ_IDLE_TIMEOUT, rx).await {
            Ok(Ok(ClientMsg::FsWriteResult {
                bytes_written,
                error,
                ..
            })) => {
                results.push(json!({
                    "name": filename,
                    "bytes_written": bytes_written,
                    "error": error,
                }));
            }
            Ok(Ok(_)) => {
                results.push(json!({
                    "name": filename,
                    "bytes_written": 0,
                    "error": "agent returned unexpected frame",
                }));
            }
            Ok(Err(_)) => {
                results.push(json!({
                    "name": filename,
                    "bytes_written": 0,
                    "error": "agent disconnected before write result",
                }));
            }
            Err(_) => {
                results.push(json!({
                    "name": filename,
                    "bytes_written": 0,
                    "error": "agent did not reply in time",
                }));
            }
        }
    }

    (StatusCode::OK, Json(json!({ "results": results }))).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn content_disposition_plain_ascii() {
        let v = content_disposition_for("src/main.rs");
        assert!(
            v.contains("filename=\"main.rs\""),
            "ASCII fallback should be the bare basename: {v}"
        );
        assert!(
            v.contains("filename*=UTF-8''main.rs"),
            "RFC 5987 form should also carry the bare name: {v}"
        );
    }

    #[test]
    fn content_disposition_utf8() {
        let v = content_disposition_for("中文文件.txt");
        // RFC 5987-encoded form must use percent-encoding for UTF-8
        // bytes; the literal CJK chars must not appear inside the
        // ASCII `filename=` parameter.
        assert!(v.contains("filename*=UTF-8''"), "missing RFC 5987 form: {v}");
        assert!(
            v.contains("%E4%B8%AD%E6%96%87%E6%96%87%E4%BB%B6.txt"),
            "percent-encoding wrong for CJK basename: {v}"
        );
        // ASCII fallback should be a non-empty placeholder, not the
        // raw CJK (which would break parsers that only honor the
        // literal `filename=`).
        let ascii_part = v
            .split(';')
            .map(str::trim)
            .find(|p| p.starts_with("filename=\""))
            .expect("ASCII filename= param present");
        assert!(
            !ascii_part.contains('中'),
            "ASCII fallback leaked non-ASCII chars: {ascii_part}"
        );
    }

    #[test]
    fn content_disposition_strips_control_chars() {
        // CR / LF / NUL / quotes / semicolons all need to vanish so
        // an attacker controlling the path can't inject header
        // continuations or terminate the filename parameter early.
        let v = content_disposition_for("evil\r\n\"; X-Injected: 1.txt");
        for needle in ["\r", "\n", "\"; X-Injected", "\0"] {
            assert!(
                !v.contains(needle),
                "Content-Disposition leaked dangerous char {:?}: {v}",
                needle
            );
        }
        // Should also still be a valid HeaderValue (parseable).
        let parsed: Result<axum::http::HeaderValue, _> = v.parse();
        assert!(parsed.is_ok(), "result didn't parse as HeaderValue: {v}");
    }

    #[test]
    fn content_disposition_basename_only() {
        let v = content_disposition_for("a/b/c/deep/file.bin");
        assert!(
            v.contains("filename=\"file.bin\""),
            "basename extraction failed: {v}"
        );
        assert!(!v.contains("a/b/c"), "ancestors leaked: {v}");
    }

    #[test]
    fn content_disposition_empty_basename_falls_back() {
        // Trailing slash → no basename → "download" placeholder.
        let v = content_disposition_for("foo/");
        assert!(
            v.contains("filename=\"download\""),
            "fallback missing: {v}"
        );
    }

    #[test]
    fn truthy_accepts_common_forms() {
        assert!(truthy("1"));
        assert!(truthy("true"));
        assert!(truthy("TRUE"));
        assert!(truthy("yes"));
        assert!(!truthy("0"));
        assert!(!truthy("false"));
        assert!(!truthy(""));
    }
}
