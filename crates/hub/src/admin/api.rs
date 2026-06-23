//! Admin JSON API. Backs the React SPA in `admin-ui/`. Every endpoint
//! lives under `/admin/api/`. Authentication is the session cookie set
//! by `POST /admin/api/login`; unauthenticated callers hit
//! `require_admin` and get a 401 JSON envelope.
//!
//! Response shape:
//!   - Success: 2xx with whatever JSON the endpoint advertises.
//!   - Error:   non-2xx with `{ "error": "code", "message": "..." }`.

use super::{AdminState, SESSION_COOKIE};
use crate::auth;
use crate::db::{AuditFilter, SessionsFilter};
use axum::{
    extract::{Path, Query, State},
    http::{header::SET_COOKIE, HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use serde::{Deserialize, Serialize};
use serde_json::json;

const SESSION_TTL_SECS: i64 = 60 * 60 * 12;

// ---------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------

fn err(status: StatusCode, code: &str, message: impl Into<String>) -> Response {
    let body = json!({ "error": code, "message": message.into() });
    (status, Json(body)).into_response()
}

fn internal(e: impl std::fmt::Display) -> Response {
    tracing::error!(error = %e, "admin api: internal error");
    err(StatusCode::INTERNAL_SERVER_ERROR, "internal", "internal error")
}

fn valid_account_name(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 64
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

fn valid_agent_name(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 64
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

fn token_prefix(token: &str) -> String {
    let n = token.chars().count();
    if n <= 6 {
        token.to_string()
    } else {
        token.chars().skip(n - 6).collect()
    }
}

fn parse_datetime_local(s: &str) -> Option<i64> {
    if let Ok(dt) = chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M") {
        return Some(dt.and_utc().timestamp());
    }
    if let Ok(dt) = chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S") {
        return Some(dt.and_utc().timestamp());
    }
    if let Ok(date) = chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d") {
        return date
            .and_hms_opt(0, 0, 0)
            .map(|dt| dt.and_utc().timestamp());
    }
    None
}

fn norm(v: &Option<String>) -> Option<String> {
    v.as_ref()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

// ---------------------------------------------------------------------
// Auth — login / logout / me
// ---------------------------------------------------------------------

/// The admin login form gained a username field for visual parity
/// with the user-facing login, but the hub still has exactly one
/// admin identity. We require the literal `"admin"` here so a typo
/// fails fast instead of the user wondering why their non-`admin`
/// username silently "worked".
const ADMIN_USERNAME: &str = "admin";

#[derive(Deserialize)]
pub struct LoginRequest {
    pub username: String,
    pub token: String,
}

pub async fn login(
    State(state): State<AdminState>,
    Json(req): Json<LoginRequest>,
) -> Response {
    let username = req.username.trim();
    let token = req.token.trim();
    if username != ADMIN_USERNAME || token.is_empty() {
        return err(
            StatusCode::UNAUTHORIZED,
            "invalid_credentials",
            "invalid admin credentials",
        );
    }
    let Some(sid) = state.auth.login(token).await else {
        return err(
            StatusCode::UNAUTHORIZED,
            "invalid_credentials",
            "invalid admin credentials",
        );
    };
    let cookie = format!(
        "{name}={sid}; HttpOnly; SameSite=Strict; Path=/admin; Max-Age={ttl}",
        name = SESSION_COOKIE,
        sid = sid,
        ttl = SESSION_TTL_SECS,
    );
    let mut headers = HeaderMap::new();
    headers.insert(SET_COOKIE, cookie.parse().unwrap());
    (StatusCode::OK, headers, Json(json!({"ok": true}))).into_response()
}

pub async fn logout(State(state): State<AdminState>, headers: HeaderMap) -> Response {
    if let Some(sid) = super::session_cookie(&headers) {
        state.auth.logout(&sid).await;
    }
    let cookie = format!(
        "{name}=; HttpOnly; SameSite=Strict; Path=/admin; Max-Age=0",
        name = SESSION_COOKIE,
    );
    let mut out = HeaderMap::new();
    out.insert(SET_COOKIE, cookie.parse().unwrap());
    (StatusCode::NO_CONTENT, out).into_response()
}

pub async fn me() -> Response {
    // Reaching this handler at all means require_admin let us through.
    // Surface the hub's own build version so the admin UI can show
    // which hub instance is talking to it (helps narrow down "did the
    // hub actually upgrade?" questions during a self-update flow).
    (
        StatusCode::OK,
        Json(json!({
            "ok": true,
            "hub_version": env!("CARGO_PKG_VERSION"),
        })),
    )
        .into_response()
}

// ---------------------------------------------------------------------
// Dashboard
// ---------------------------------------------------------------------

#[derive(Serialize)]
struct DashboardResponse {
    accounts: i64,
    active_sessions: i64,
    sessions_24h: i64,
    online_agents: Vec<String>,
}

pub async fn dashboard(State(state): State<AdminState>) -> Response {
    let accounts = state.app.db.account_count().await.unwrap_or(0);
    let active_sessions = state.app.db.count_active_sessions().await.unwrap_or(0);
    let sessions_24h = state.app.db.count_sessions_since(86400).await.unwrap_or(0);
    let online_agents = state.app.registry.list_active();
    Json(DashboardResponse {
        accounts,
        active_sessions,
        sessions_24h,
        online_agents,
    })
    .into_response()
}

/// Hourly session-start buckets for the dashboard chart.
/// `?hours=24` (default), values are sparse — frontend fills empty
/// hours with 0 for nicer rendering.
#[derive(Deserialize)]
pub struct HourlyQuery {
    #[serde(default)]
    pub hours: Option<i64>,
}

#[derive(Serialize)]
struct HourlyBucket {
    ts: i64,
    count: i64,
}

pub async fn sessions_hourly(
    State(state): State<AdminState>,
    Query(q): Query<HourlyQuery>,
) -> Response {
    let hours = q.hours.unwrap_or(24).clamp(1, 24 * 30);
    let cutoff = chrono::Utc::now().timestamp() - hours * 3600;
    let rows = match sqlx::query(
        "SELECT (started_at / 3600) * 3600 AS bucket, COUNT(*) AS n
           FROM sessions WHERE started_at >= ?1
          GROUP BY bucket ORDER BY bucket",
    )
    .bind(cutoff)
    .fetch_all(&state.app.db.pool)
    .await
    {
        Ok(r) => r,
        Err(e) => return internal(e),
    };
    use sqlx::Row;
    let buckets: Vec<HourlyBucket> = rows
        .into_iter()
        .map(|r| HourlyBucket {
            ts: r.get::<i64, _>("bucket"),
            count: r.get::<i64, _>("n"),
        })
        .collect();
    Json(buckets).into_response()
}

// ---------------------------------------------------------------------
// Accounts
// ---------------------------------------------------------------------

#[derive(Serialize)]
struct AccountDto {
    name: String,
    real_name: Option<String>,
    token_prefix: Option<String>,
    created_at: i64,
    disabled: bool,
    /// Agents whitelisted for this account. Empty = locked out (strict
    /// whitelist semantics; admin must grant access from the editor).
    allowed_agents: Vec<String>,
    /// Wall-clock of this account's most recent session start, or None
    /// if it has never opened one.
    last_used_at: Option<i64>,
    /// True iff this account has at least one session currently live
    /// (ended_at IS NULL).
    online: bool,
    /// True iff this account has at least one live `/v1/pty/ws`
    /// connection — webterm tab open or CLI dialled in, regardless
    /// of whether they're sitting in a workspace yet. Drives the
    /// admin "Disconnect" button (which kicks the WebSocket, not
    /// the PTY).
    connected: bool,
    /// Per-account sandbox mode: "strict" / "permissive" / "off".
    sandbox_mode: String,
}

pub async fn accounts_list(State(state): State<AdminState>) -> Response {
    let rows = match state.app.db.list_accounts().await {
        Ok(r) => r,
        Err(e) => return internal(e),
    };
    let activity = state
        .app
        .db
        .account_activity_index()
        .await
        .unwrap_or_default();
    let mut activity_map: std::collections::HashMap<String, (Option<i64>, i64)> =
        std::collections::HashMap::with_capacity(activity.len());
    for (name, last_used, active_count) in activity {
        activity_map.insert(name, (last_used, active_count));
    }
    let mut dto: Vec<AccountDto> = Vec::with_capacity(rows.len());
    for a in rows {
        let allowed = state
            .app
            .db
            .list_allowed_agents(&a.name)
            .await
            .unwrap_or_default();
        let (last_used_at, active_count) =
            activity_map.get(&a.name).copied().unwrap_or((None, 0));
        let connected = state.app.user_is_connected(&a.name);
        dto.push(AccountDto {
            name: a.name,
            real_name: a.real_name,
            token_prefix: a.token_prefix,
            created_at: a.created_at,
            disabled: a.disabled,
            allowed_agents: allowed,
            last_used_at,
            online: active_count > 0,
            connected,
            sandbox_mode: a.sandbox_mode,
        });
    }
    Json(dto).into_response()
}

#[derive(Deserialize)]
pub struct CreateAccountRequest {
    pub name: String,
    #[serde(default)]
    pub real_name: Option<String>,
}

#[derive(Serialize)]
struct TokenResponse {
    name: String,
    token: String,
}

pub async fn accounts_create(
    State(state): State<AdminState>,
    Json(req): Json<CreateAccountRequest>,
) -> Response {
    let name = req.name.trim().to_string();
    if !valid_account_name(&name) {
        return err(
            StatusCode::BAD_REQUEST,
            "invalid_input",
            "account name must match [A-Za-z0-9_-]{1,64}",
        );
    }
    match state.app.db.account_exists(&name).await {
        Ok(true) => {
            return err(
                StatusCode::CONFLICT,
                "conflict",
                format!("account '{}' already exists", name),
            )
        }
        Ok(false) => {}
        Err(e) => return internal(e),
    }
    let token = auth::generate_token();
    let hash = match auth::hash_token(&token) {
        Ok(h) => h,
        Err(e) => return internal(e),
    };
    let prefix = token_prefix(&token);
    if let Err(e) = state
        .app
        .db
        .insert_account(&name, &hash, Some(&prefix), req.real_name.as_deref())
        .await
    {
        return internal(e);
    }
    (
        StatusCode::CREATED,
        Json(TokenResponse { name, token }),
    )
        .into_response()
}

pub async fn accounts_rotate(
    State(state): State<AdminState>,
    Path(name): Path<String>,
) -> Response {
    if !valid_account_name(&name) {
        return err(StatusCode::BAD_REQUEST, "invalid_input", "invalid account name");
    }
    let token = auth::generate_token();
    let hash = match auth::hash_token(&token) {
        Ok(h) => h,
        Err(e) => return internal(e),
    };
    let prefix = token_prefix(&token);
    if let Err(e) = state.app.db.update_account_token(&name, &hash, &prefix).await {
        return err(StatusCode::NOT_FOUND, "not_found", e.to_string());
    }
    Json(TokenResponse { name, token }).into_response()
}

pub async fn accounts_toggle(
    State(state): State<AdminState>,
    Path(name): Path<String>,
) -> Response {
    let accounts = match state.app.db.list_accounts().await {
        Ok(a) => a,
        Err(e) => return internal(e),
    };
    let Some(current) = accounts.iter().find(|a| a.name == name) else {
        return err(StatusCode::NOT_FOUND, "not_found", "account not found");
    };
    let new_disabled = !current.disabled;
    if let Err(e) = state.app.db.set_account_disabled(&name, new_disabled).await {
        return err(StatusCode::NOT_FOUND, "not_found", e.to_string());
    }
    StatusCode::NO_CONTENT.into_response()
}

#[derive(Deserialize)]
pub struct SetSandboxModeRequest {
    pub sandbox_mode: String,
}

pub async fn accounts_set_sandbox_mode(
    State(state): State<AdminState>,
    Path(name): Path<String>,
    Json(req): Json<SetSandboxModeRequest>,
) -> Response {
    if !matches!(req.sandbox_mode.as_str(), "strict" | "permissive" | "off") {
        return err(
            StatusCode::BAD_REQUEST,
            "invalid_input",
            "sandbox_mode must be one of: strict, permissive, off",
        );
    }
    if let Err(e) = state
        .app
        .db
        .set_account_sandbox_mode(&name, &req.sandbox_mode)
        .await
    {
        return err(StatusCode::NOT_FOUND, "not_found", e.to_string());
    }
    StatusCode::NO_CONTENT.into_response()
}

pub async fn accounts_delete(
    State(state): State<AdminState>,
    Path(name): Path<String>,
) -> Response {
    if let Err(e) = state.app.db.delete_account(&name).await {
        return err(StatusCode::NOT_FOUND, "not_found", e.to_string());
    }
    StatusCode::NO_CONTENT.into_response()
}

/// Forcibly close every live user WS connection for this account.
/// The connection receives a terminal `Rejected { reason }` frame
/// before the socket is shut. New logins keep working — this does
/// NOT rotate the token or disable the account. 204 even when no
/// session was online, so the UI doesn't have to guess.
pub async fn accounts_disconnect(
    State(state): State<AdminState>,
    Path(name): Path<String>,
) -> Response {
    match state.app.db.account_exists(&name).await {
        Ok(true) => {}
        Ok(false) => return err(StatusCode::NOT_FOUND, "not_found", "account not found"),
        Err(e) => return internal(e),
    }
    // send() returns Err(_) when there are no subscribers; that's
    // the "account is currently offline" case and is not an error.
    let _ = state.app.user_kick_sender(&name).send(());
    StatusCode::NO_CONTENT.into_response()
}

#[derive(Deserialize)]
pub struct UpdateRealNameRequest {
    pub real_name: Option<String>,
}

pub async fn accounts_update_real_name(
    State(state): State<AdminState>,
    Path(name): Path<String>,
    Json(req): Json<UpdateRealNameRequest>,
) -> Response {
    if let Err(e) = state
        .app
        .db
        .update_account_real_name(&name, req.real_name.as_deref())
        .await
    {
        return err(StatusCode::NOT_FOUND, "not_found", e.to_string());
    }
    StatusCode::NO_CONTENT.into_response()
}

// ---------------------------------------------------------------------
// Account → Agent allowlist
// ---------------------------------------------------------------------

#[derive(Serialize)]
struct AllowedAgentsResponse {
    /// Agents currently whitelisted for this account.
    allowed: Vec<String>,
    /// Every agent name the admin UI should let the operator pick from:
    /// historically-seen + currently-online + already-allowed, deduped.
    known: Vec<String>,
    /// Subset of `known` that's connected to the hub right now.
    online: Vec<String>,
}

pub async fn account_allowed_agents_get(
    State(state): State<AdminState>,
    Path(name): Path<String>,
) -> Response {
    if !valid_account_name(&name) {
        return err(StatusCode::BAD_REQUEST, "invalid_input", "invalid account name");
    }
    match state.app.db.account_exists(&name).await {
        Ok(true) => {}
        Ok(false) => return err(StatusCode::NOT_FOUND, "not_found", "account not found"),
        Err(e) => return internal(e),
    }
    let allowed = match state.app.db.list_allowed_agents(&name).await {
        Ok(v) => v,
        Err(e) => return internal(e),
    };
    let mut known = match state.app.db.distinct_known_agents().await {
        Ok(v) => v,
        Err(e) => return internal(e),
    };
    let online = state.app.registry.list_active();
    // Make sure currently-online agents always show up even if they
    // haven't yet been seen in sessions/allowlist.
    for n in &online {
        if !known.iter().any(|k| k == n) {
            known.push(n.clone());
        }
    }
    known.sort();
    known.dedup();
    Json(AllowedAgentsResponse {
        allowed,
        known,
        online,
    })
    .into_response()
}

#[derive(Deserialize)]
pub struct SetAllowedAgentsRequest {
    pub agents: Vec<String>,
}

pub async fn account_allowed_agents_set(
    State(state): State<AdminState>,
    Path(name): Path<String>,
    Json(req): Json<SetAllowedAgentsRequest>,
) -> Response {
    if !valid_account_name(&name) {
        return err(StatusCode::BAD_REQUEST, "invalid_input", "invalid account name");
    }
    match state.app.db.account_exists(&name).await {
        Ok(true) => {}
        Ok(false) => return err(StatusCode::NOT_FOUND, "not_found", "account not found"),
        Err(e) => return internal(e),
    }
    // Light dedup + trim; leave name-shape validation to the agent
    // (we may have historically-named agents that don't match a
    // hypothetical stricter rule).
    let mut agents: Vec<String> = req
        .agents
        .into_iter()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    agents.sort();
    agents.dedup();
    if let Err(e) = state.app.db.set_allowed_agents(&name, &agents).await {
        return internal(e);
    }
    StatusCode::NO_CONTENT.into_response()
}

// ---------------------------------------------------------------------
// Agents — admin view of allow-list from the agent side
// ---------------------------------------------------------------------

#[derive(Serialize)]
struct AgentRowDto {
    name: String,
    online: bool,
    allowed_account_count: i64,
    /// Self-reported agent build version from the most recent hello frame.
    /// `None` if the agent is offline or it's a pre-v1.6 build that
    /// doesn't yet send `agent_version`.
    #[serde(skip_serializing_if = "Option::is_none")]
    version: Option<String>,
    /// Latest agent release available from GitHub at the time of this
    /// call. Used by the admin UI to surface an "update available" badge.
    /// Always serialized (as `null` when unknown) so the client sees the
    /// field per its `string | null` contract — omitting it made the UI read
    /// `undefined` and render "Update to undefined".
    latest_version: Option<String>,
}

pub async fn agents_list(State(state): State<AdminState>) -> Response {
    let known = match state.app.db.distinct_known_agents().await {
        Ok(v) => v,
        Err(e) => return internal(e),
    };
    let online_list = state.app.registry.list_active();
    let online: std::collections::HashSet<String> = online_list.iter().cloned().collect();
    let mut names: Vec<String> = known;
    for n in &online_list {
        if !names.iter().any(|k| k == n) {
            names.push(n.clone());
        }
    }
    names.sort();
    names.dedup();
    let counts = match state.app.db.count_allowed_accounts_per_agent().await {
        Ok(v) => v,
        Err(e) => return internal(e),
    };
    let count_map: std::collections::HashMap<String, i64> = counts.into_iter().collect();
    // Best-effort latest version lookup: don't fail the whole listing if
    // GitHub is unreachable — the agents table is still useful without it.
    let latest_version = state.releases.latest_cached_or_refresh().await;
    let dto: Vec<AgentRowDto> = names
        .into_iter()
        .map(|n| {
            let allowed_account_count = count_map.get(&n).copied().unwrap_or(0);
            let is_online = online.contains(&n);
            let version = if is_online {
                state
                    .app
                    .registry
                    .get(&n)
                    .and_then(|c| c.agent_version.clone())
            } else {
                None
            };
            AgentRowDto {
                name: n,
                online: is_online,
                allowed_account_count,
                version,
                latest_version: latest_version.clone(),
            }
        })
        .collect();
    Json(dto).into_response()
}

// ---------------------------------------------------------------------
// Agent releases + self-update
// ---------------------------------------------------------------------

// Cache GitHub releases for 10 minutes. Keep this comfortably long: we hit the
// GitHub API UNAUTHENTICATED (60 req/hour/IP), so a short TTL plus admin-UI
// polling exhausts the rate limit, the fetch starts returning 403, and
// `latest_version` collapses to None (the "Update to undefined" bug). At 10 min
// that's ~6 req/hour, well under the limit. The admin UI's manual refresh
// button can still force a fresh fetch on demand (see `refresh_now` / ?force=1).
const RELEASES_TTL: std::time::Duration = std::time::Duration::from_secs(10 * 60);
const GITHUB_RELEASES_URL: &str =
    "https://api.github.com/repos/initialz/cloudcode/releases";
const UPDATE_REPLY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(600);
const VERSION_RE_HINT: &str = "vX.Y.Z";

#[derive(Serialize, Clone)]
pub struct ReleaseDto {
    pub tag: String,
    /// Publish date in ISO format (YYYY-MM-DD). Empty when GitHub didn't
    /// supply `published_at` (draft / unpublished releases).
    pub date: String,
}

#[derive(Serialize, Clone)]
pub struct ReleasesResponse {
    pub releases: Vec<ReleaseDto>,
    pub latest: Option<String>,
}

/// Cached release listing. We keep both the public DTO (returned to
/// admin UI) and the full asset map (used by the update endpoint to
/// resolve the right download URL).
#[derive(Clone)]
struct ReleasesCacheEntry {
    fetched_at: std::time::Instant,
    public: ReleasesResponse,
    /// For each tag, the asset map keyed by asset filename.
    assets: std::collections::HashMap<String, std::collections::HashMap<String, String>>,
}

pub struct ReleasesCache {
    inner: tokio::sync::RwLock<Option<ReleasesCacheEntry>>,
}

impl ReleasesCache {
    pub fn new() -> Self {
        Self {
            inner: tokio::sync::RwLock::new(None),
        }
    }

    /// Return the cached entry if present and fresh, otherwise refresh.
    /// On refresh failure with a stale cache, prefer the stale data over
    /// a hard error so the admin UI degrades gracefully.
    async fn get_fresh(&self) -> Result<ReleasesCacheEntry, String> {
        if let Some(entry) = self.inner.read().await.clone() {
            if entry.fetched_at.elapsed() < RELEASES_TTL {
                return Ok(entry);
            }
        }
        match fetch_releases().await {
            Ok(fresh) => {
                let mut w = self.inner.write().await;
                *w = Some(fresh.clone());
                Ok(fresh)
            }
            Err(e) => {
                if let Some(entry) = self.inner.read().await.clone() {
                    tracing::warn!(error = %e, "releases refresh failed; serving stale cache");
                    return Ok(entry);
                }
                Err(e)
            }
        }
    }

    /// Force a fresh GitHub fetch regardless of TTL and update the cache.
    /// Backs the admin UI's manual refresh button so an operator can see
    /// GitHub's real state on demand without waiting out the TTL. On fetch
    /// failure, fall back to a stale cache if present (same graceful degrade
    /// as `get_fresh`) so a transient GitHub hiccup / rate-limit doesn't
    /// blank the badge.
    async fn refresh_now(&self) -> Result<ReleasesCacheEntry, String> {
        match fetch_releases().await {
            Ok(fresh) => {
                let mut w = self.inner.write().await;
                *w = Some(fresh.clone());
                Ok(fresh)
            }
            Err(e) => {
                if let Some(entry) = self.inner.read().await.clone() {
                    tracing::warn!(error = %e, "forced releases refresh failed; serving stale cache");
                    return Ok(entry);
                }
                Err(e)
            }
        }
    }

    /// Best-effort "latest tag" lookup used by callers that don't care if
    /// the cache is empty (e.g. agents_list). Returns None if there's
    /// nothing cached and a fresh fetch fails.
    pub async fn latest_cached_or_refresh(&self) -> Option<String> {
        self.get_fresh().await.ok().and_then(|e| e.public.latest)
    }
}

impl Default for ReleasesCache {
    fn default() -> Self {
        Self::new()
    }
}

async fn fetch_releases() -> Result<ReleasesCacheEntry, String> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .user_agent(format!("cloudcode-hub/{}", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(|e| format!("build client: {e}"))?;
    let resp = client
        .get(GITHUB_RELEASES_URL)
        .header("Accept", "application/vnd.github+json")
        .send()
        .await
        .map_err(|e| format!("GET releases: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("GitHub releases returned HTTP {}", resp.status()));
    }
    let raw: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| format!("parse releases JSON: {e}"))?;
    let arr = raw
        .as_array()
        .ok_or_else(|| "releases response was not a JSON array".to_string())?;

    let mut entries: Vec<(String, String, std::collections::HashMap<String, String>)> = Vec::new();
    for r in arr {
        let Some(tag) = r.get("tag_name").and_then(|v| v.as_str()) else {
            continue;
        };
        let published_at = r
            .get("published_at")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let date = published_at.get(..10).unwrap_or("").to_string();
        let mut asset_map: std::collections::HashMap<String, String> =
            std::collections::HashMap::new();
        if let Some(assets) = r.get("assets").and_then(|v| v.as_array()) {
            for a in assets {
                let Some(name) = a.get("name").and_then(|v| v.as_str()) else {
                    continue;
                };
                let Some(url) = a.get("browser_download_url").and_then(|v| v.as_str()) else {
                    continue;
                };
                asset_map.insert(name.to_string(), url.to_string());
            }
        }
        entries.push((tag.to_string(), date, asset_map));
    }
    // Sort by published date desc; ties keep GitHub's order.
    entries.sort_by(|a, b| b.1.cmp(&a.1));

    let public_releases: Vec<ReleaseDto> = entries
        .iter()
        .map(|(tag, date, _)| ReleaseDto {
            tag: tag.clone(),
            date: date.clone(),
        })
        .collect();
    let latest = public_releases.first().map(|r| r.tag.clone());
    let mut asset_table: std::collections::HashMap<
        String,
        std::collections::HashMap<String, String>,
    > = std::collections::HashMap::new();
    for (tag, _, assets) in entries {
        asset_table.insert(tag, assets);
    }
    Ok(ReleasesCacheEntry {
        fetched_at: std::time::Instant::now(),
        public: ReleasesResponse {
            releases: public_releases,
            latest,
        },
        assets: asset_table,
    })
}

/// `GET /admin/api/hub-version`
///
/// Unauthenticated, returns the current hub binary version. Used by
/// the admin SPA to poll for "hub came back after a self-update":
/// during the restart the in-memory cookie sessions are wiped, so
/// `/me` 401s forever from the browser's POV. This endpoint stays
/// reachable without a session so the poll loop can actually
/// observe the new version come up. Auth-gating it would defeat
/// the whole purpose; the same version string is already in the
/// hub's user-agent when it talks to GitHub Releases.
pub async fn hub_version() -> Response {
    (
        StatusCode::OK,
        Json(json!({ "version": env!("CARGO_PKG_VERSION") })),
    )
        .into_response()
}

pub async fn agents_releases(
    State(state): State<AdminState>,
    Query(q): Query<std::collections::HashMap<String, String>>,
) -> Response {
    // The admin UI's manual refresh button passes ?force=1 to bypass the TTL
    // cache and check GitHub's real state on demand; the 60s auto-poll omits
    // it and rides the 10-minute cache. The button is disabled while a refresh
    // is in flight (frontend), so force can't be spammed faster than a
    // round-trip, and on rate-limit we still fall back to the stale cache.
    let force = q
        .get("force")
        .map(|v| v == "1" || v == "true")
        .unwrap_or(false);
    let fetched = if force {
        state.releases.refresh_now().await
    } else {
        state.releases.get_fresh().await
    };
    match fetched {
        Ok(entry) => Json(entry.public.clone()).into_response(),
        // Degrade gracefully instead of 503-ing the whole agents page: the
        // release list is auxiliary, and the only way we get here is a GitHub
        // fetch failure with an empty cache (e.g. rate-limited right after a
        // hub restart). Return an empty list so the page still loads; a later
        // refresh picks the releases up once the fetch succeeds.
        Err(e) => {
            tracing::warn!(error = %e, "releases fetch failed with empty cache; returning empty list");
            Json(ReleasesResponse {
                releases: Vec::new(),
                latest: None,
            })
            .into_response()
        }
    }
}

#[derive(Deserialize)]
pub struct UpdateAgentRequest {
    pub version: String,
}

pub async fn agent_update(
    State(state): State<AdminState>,
    Path(name): Path<String>,
    Json(req): Json<UpdateAgentRequest>,
) -> Response {
    if !valid_agent_name(&name) {
        return err(StatusCode::BAD_REQUEST, "invalid_input", "invalid agent name");
    }
    let target_version = req.version.trim().to_string();
    if !is_valid_version_tag(&target_version) {
        return err(
            StatusCode::BAD_REQUEST,
            "invalid_input",
            format!("version must match {}", VERSION_RE_HINT),
        );
    }

    // Resolve the live connection. We don't hold a registry lock across
    // the await below — `get` returns an Arc<AgentConn>.
    let Some(conn) = state.app.registry.get(&name) else {
        return err(
            StatusCode::NOT_FOUND,
            "agent_offline",
            format!("agent '{}' is not connected", name),
        );
    };
    let Some(target_triple) = conn.target_triple.clone() else {
        return err(
            StatusCode::BAD_REQUEST,
            "target_unknown",
            "agent did not report its target_triple in the hello frame; \
             upgrade the agent to v1.6+ before driving a remote update",
        );
    };
    let asset_os = match map_target_to_release_os(&target_triple) {
        Some(s) => s,
        None => {
            return err(
                StatusCode::BAD_REQUEST,
                "unsupported_target",
                format!("no release asset mapping for target {}", target_triple),
            );
        }
    };

    // Look up release + assets.
    let entry = match state.releases.get_fresh().await {
        Ok(e) => e,
        Err(e) => {
            return err(StatusCode::SERVICE_UNAVAILABLE, "upstream_unavailable", e);
        }
    };
    let Some(assets) = entry.assets.get(&target_version) else {
        return err(
            StatusCode::NOT_FOUND,
            "release_not_found",
            format!("no release tagged {}", target_version),
        );
    };
    let download_name = format!("cloudcode-{}-{}.tar.gz", target_version, asset_os);
    let sha256_name = format!("cloudcode-{}-{}.sha256", target_version, asset_os);
    let download_url = match assets.get(&download_name) {
        Some(u) => u.clone(),
        None => {
            return err(
                StatusCode::BAD_GATEWAY,
                "missing_asset",
                format!(
                    "release {} has no asset {} for target {}",
                    target_version, download_name, target_triple
                ),
            );
        }
    };
    let sha256_url = match assets.get(&sha256_name) {
        Some(u) => u.clone(),
        None => {
            return err(
                StatusCode::BAD_GATEWAY,
                "missing_asset",
                format!(
                    "release {} has no sha256 manifest {} for target {}",
                    target_version, sha256_name, target_triple
                ),
            );
        }
    };

    // Register a one-shot reply slot, fire the request, await with a
    // generous timeout (downloads can be slow on small VPSes).
    let request_id = uuid::Uuid::new_v4();
    let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
    conn.register_workspace_request(request_id, reply_tx);
    if conn
        .send(crate::tunnel::ServerMsg::UpdateAgent {
            request_id,
            target_version: target_version.clone(),
            download_url,
            sha256_url,
        })
        .await
        .is_err()
    {
        return err(
            StatusCode::SERVICE_UNAVAILABLE,
            "agent_offline",
            "agent disconnected before update request was sent",
        );
    }
    match tokio::time::timeout(UPDATE_REPLY_TIMEOUT, reply_rx).await {
        Ok(Ok(crate::tunnel::ClientMsg::UpdateAgentResult {
            error: Some(error),
            ..
        })) => err(StatusCode::UNPROCESSABLE_ENTITY, "agent_update_failed", error),
        Ok(Ok(crate::tunnel::ClientMsg::UpdateAgentResult { error: None, .. })) => (
            StatusCode::ACCEPTED,
            Json(json!({"ok": true})),
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
            StatusCode::GATEWAY_TIMEOUT,
            "agent_timeout",
            "agent did not reply within 10 minutes",
        ),
    }
}

/// `POST /admin/api/hub/update`
///
/// Self-update the hub itself. Always pulls the latest release (no
/// version picker) because rolling back across hub schema migrations
/// is unsafe — for that, manually reinstall an older binary and run
/// `cloudcode-hub daemon restart`. Returns ACCEPTED before the
/// process exits so the frontend can switch into its "waiting for
/// hub to come back" poll. The supervisor (`cloudcode-hub
/// supervise`) sees the clean exit and re-execs through the
/// freshly-flipped `hub/current` symlink.
pub async fn hub_update(State(state): State<AdminState>) -> Response {
    let target_triple = crate::update::target_triple();
    let asset_os = match map_target_to_release_os(target_triple) {
        Some(s) => s,
        None => {
            return err(
                StatusCode::BAD_REQUEST,
                "unsupported_target",
                format!("no release asset mapping for target {}", target_triple),
            );
        }
    };

    let entry = match state.releases.get_fresh().await {
        Ok(e) => e,
        Err(e) => return err(StatusCode::SERVICE_UNAVAILABLE, "upstream_unavailable", e),
    };
    let latest_tag = match entry.public.latest.clone() {
        Some(t) => t,
        None => {
            return err(
                StatusCode::SERVICE_UNAVAILABLE,
                "no_release",
                "upstream has no current release tag",
            );
        }
    };
    if !is_valid_version_tag(&latest_tag) {
        return err(
            StatusCode::BAD_GATEWAY,
            "bad_release_tag",
            format!("upstream returned malformed tag {:?}", latest_tag),
        );
    }
    if latest_tag.trim_start_matches('v') == env!("CARGO_PKG_VERSION") {
        return err(
            StatusCode::CONFLICT,
            "already_latest",
            format!("hub is already on {}", latest_tag),
        );
    }
    let Some(assets) = entry.assets.get(&latest_tag) else {
        return err(
            StatusCode::NOT_FOUND,
            "release_not_found",
            format!("no release tagged {}", latest_tag),
        );
    };
    let download_name = format!("cloudcode-{}-{}.tar.gz", latest_tag, asset_os);
    let sha256_name = format!("cloudcode-{}-{}.sha256", latest_tag, asset_os);
    let Some(download_url) = assets.get(&download_name).cloned() else {
        return err(
            StatusCode::BAD_GATEWAY,
            "missing_asset",
            format!(
                "release {} has no asset {} for target {}",
                latest_tag, download_name, target_triple
            ),
        );
    };
    let Some(sha256_url) = assets.get(&sha256_name).cloned() else {
        return err(
            StatusCode::BAD_GATEWAY,
            "missing_asset",
            format!(
                "release {} has no sha256 manifest {} for target {}",
                latest_tag, sha256_name, target_triple
            ),
        );
    };

    // Run the update synchronously inside the request — frontend will
    // see the 202 response, then immediately fail to reach /admin/api
    // for a few seconds while the supervisor re-execs the new binary,
    // then succeed again with the new build.
    if let Err(e) = crate::update::perform_update(crate::update::UpdateRequest {
        target_version: latest_tag.clone(),
        download_url,
        sha256_url,
    })
    .await
    {
        return err(StatusCode::UNPROCESSABLE_ENTITY, "hub_update_failed", e);
    }

    tracing::info!(version = %latest_tag, "hub update installed; scheduling exit");
    // Spawn the actual exit on a short delay so axum has a chance to
    // flush the response we're about to return. Without this the
    // browser may observe the connection reset before the JSON body
    // arrives, and the SPA's poll logic can't tell "succeeded then
    // restarting" from "request failed".
    tokio::spawn(async {
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        tracing::info!("hub exiting to apply update");
        std::process::exit(0);
    });
    (
        StatusCode::ACCEPTED,
        Json(json!({"ok": true, "installed": latest_tag})),
    )
        .into_response()
}

fn is_valid_version_tag(v: &str) -> bool {
    let Some(rest) = v.strip_prefix('v') else {
        return false;
    };
    let parts: Vec<&str> = rest.split('.').collect();
    if parts.len() != 3 {
        return false;
    }
    parts
        .iter()
        .all(|p| !p.is_empty() && p.chars().all(|c| c.is_ascii_digit()))
}

fn map_target_to_release_os(target: &str) -> Option<&'static str> {
    match target {
        "aarch64-apple-darwin" => Some("macos-aarch64"),
        "aarch64-unknown-linux-musl" => Some("linux-aarch64"),
        "x86_64-unknown-linux-musl" => Some("linux-x86_64"),
        _ => None,
    }
}

#[derive(Serialize)]
struct AllowedAccountsResponse {
    allowed: Vec<String>,
    accounts: Vec<String>,
    online: bool,
}

pub async fn agent_allowed_accounts_get(
    State(state): State<AdminState>,
    Path(name): Path<String>,
) -> Response {
    if !valid_agent_name(&name) {
        return err(StatusCode::BAD_REQUEST, "invalid_input", "invalid agent name");
    }
    let allowed = match state.app.db.list_allowed_accounts_for_agent(&name).await {
        Ok(v) => v,
        Err(e) => return internal(e),
    };
    let accounts = match state.app.db.list_accounts().await {
        Ok(rows) => rows.into_iter().map(|a| a.name).collect::<Vec<_>>(),
        Err(e) => return internal(e),
    };
    let online = state
        .app
        .registry
        .list_active()
        .iter()
        .any(|n| n == &name);
    Json(AllowedAccountsResponse {
        allowed,
        accounts,
        online,
    })
    .into_response()
}

#[derive(Deserialize)]
pub struct SetAllowedAccountsRequest {
    pub accounts: Vec<String>,
}

pub async fn agent_allowed_accounts_set(
    State(state): State<AdminState>,
    Path(name): Path<String>,
    Json(req): Json<SetAllowedAccountsRequest>,
) -> Response {
    if !valid_agent_name(&name) {
        return err(StatusCode::BAD_REQUEST, "invalid_input", "invalid agent name");
    }
    let mut accounts: Vec<String> = req
        .accounts
        .into_iter()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    accounts.sort();
    accounts.dedup();
    if let Err(e) = state
        .app
        .db
        .set_allowed_accounts_for_agent(&name, &accounts)
        .await
    {
        return internal(e);
    }
    StatusCode::NO_CONTENT.into_response()
}

/// Retire an agent name: drop every ACL row mentioning it. Refused
/// for currently-online agents so the admin can't accidentally cut
/// off everyone using a live agent. Sessions/audit history is left
/// untouched (it still references the old name as part of the
/// record of what happened).
pub async fn agent_delete(
    State(state): State<AdminState>,
    Path(name): Path<String>,
) -> Response {
    if !valid_agent_name(&name) {
        return err(StatusCode::BAD_REQUEST, "invalid_input", "invalid agent name");
    }
    if state.app.registry.list_active().iter().any(|n| n == &name) {
        return err(
            StatusCode::CONFLICT,
            "agent_online",
            format!(
                "agent '{}' is online — disconnect it before deleting (rename / retire on the agent host)",
                name
            ),
        );
    }
    if let Err(e) = state.app.db.delete_agent_acl(&name).await {
        return internal(e);
    }
    StatusCode::NO_CONTENT.into_response()
}

// ---------------------------------------------------------------------
// Workspaces inventory
// ---------------------------------------------------------------------

#[derive(Serialize)]
struct WorkspaceRowDto {
    agent: String,
    account: String,
    workspace: String,
    /// "active" — a cloudcode client is attached right now.
    /// "saved"  — tmux still has state but nobody is connected.
    /// "fresh"  — directory exists but no tmux state (or agent offline).
    status: &'static str,
    has_client: bool,
    tmux_alive: bool,
    agent_online: bool,
    /// `started_at` of the most recent session in this slot, if any.
    last_started_at: Option<i64>,
}

const WORKSPACES_REQUEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(8);

#[derive(Deserialize)]
pub struct WorkspaceDeleteRequest {
    pub agent: String,
    pub account: String,
    pub workspace: String,
}

/// `POST /admin/api/workspaces/delete`
///
/// Admin-driven workspace removal: route a `WorkspaceDelete` to the
/// owning agent (mirroring what the user-facing client would do),
/// then drop the hub-side binding so the picker stops advertising
/// it. If the agent is offline we still drop the binding — the
/// admin's intent is "stop tracking this", and the dir on the dead
/// agent's disk would be cleaned up either by re-onboarding or by
/// the agent operator manually. We refuse if a client is currently
/// attached so the admin doesn't yank a workspace out from under a
/// live claude session.
pub async fn workspace_delete(
    State(state): State<AdminState>,
    Json(req): Json<WorkspaceDeleteRequest>,
) -> Response {
    use crate::tunnel::{ClientMsg, ServerMsg};
    let key = (req.agent.clone(), req.account.clone(), req.workspace.clone());
    if state.app.workspaces.contains_key(&key) {
        return err(
            StatusCode::CONFLICT,
            "in_use",
            format!(
                "workspace '{}' on agent '{}' is currently in use",
                req.workspace, req.agent
            ),
        );
    }
    let agent_conn = state.app.registry.get(&req.agent);
    if let Some(conn) = agent_conn {
        let request_id = uuid::Uuid::new_v4();
        let (tx, rx) = tokio::sync::oneshot::channel();
        conn.register_workspace_request(request_id, tx);
        if conn
            .send(ServerMsg::WorkspaceDelete {
                request_id,
                account: req.account.clone(),
                name: req.workspace.clone(),
            })
            .await
            .is_err()
        {
            return err(
                StatusCode::SERVICE_UNAVAILABLE,
                "agent_disconnected",
                "agent disconnected before delete acked",
            );
        }
        match tokio::time::timeout(WORKSPACES_REQUEST_TIMEOUT, rx).await {
            Ok(Ok(ClientMsg::WorkspaceDeleteResult { error, .. })) => {
                if let Some(e) = error {
                    return err(StatusCode::BAD_GATEWAY, "agent_error", e);
                }
            }
            _ => {
                return err(
                    StatusCode::GATEWAY_TIMEOUT,
                    "agent_timeout",
                    "agent did not ack the delete in time",
                );
            }
        }
    } else {
        tracing::info!(
            agent = %req.agent,
            account = %req.account,
            workspace = %req.workspace,
            "workspace_delete: agent offline; dropping DB binding only"
        );
    }
    if let Err(e) = state
        .app
        .db
        .delete_workspace_binding(&req.account, &req.agent, &req.workspace)
        .await
    {
        return err(
            StatusCode::INTERNAL_SERVER_ERROR,
            "db_error",
            format!("delete binding: {e}"),
        );
    }
    state.app.audit.write(crate::audit::AuditEvent {
        account: Some(req.account.clone()),
        agent: Some(req.agent.clone()),
        workspace: Some(req.workspace.clone()),
        status: Some(200),
        reason: Some("admin deleted workspace".into()),
        ..crate::audit::AuditEvent::new("workspace_deleted_admin")
    });
    (StatusCode::NO_CONTENT).into_response()
}

pub async fn workspaces_list(State(state): State<AdminState>) -> Response {
    use crate::registry::AgentConn;
    use crate::tunnel::{ClientMsg, ServerMsg};

    let conns = state.app.registry.list_conns();
    let last_started_rows = state
        .app
        .db
        .last_started_per_workspace()
        .await
        .unwrap_or_default();
    let mut last_started: std::collections::HashMap<(String, String, String), i64> =
        std::collections::HashMap::new();
    for (agent, account, workspace, ts) in last_started_rows {
        last_started.insert((agent, account, workspace), ts);
    }

    // Fan-out to every online agent in parallel.
    let online_names: std::collections::HashSet<String> =
        conns.iter().map(|c| c.name.clone()).collect();
    type FanoutResult = (String, Result<Vec<crate::tunnel::WorkspaceFullItem>, String>);
    let mut tasks: Vec<tokio::task::JoinHandle<FanoutResult>> = Vec::new();
    for conn in conns {
        let conn: std::sync::Arc<AgentConn> = conn;
        tasks.push(tokio::spawn(async move {
            let request_id = uuid::Uuid::new_v4();
            let (tx, rx) = tokio::sync::oneshot::channel();
            conn.register_workspace_request(request_id, tx);
            if conn
                .send(ServerMsg::WorkspaceListAll { request_id })
                .await
                .is_err()
            {
                return (conn.name.clone(), Err("agent disconnected".into()));
            }
            match tokio::time::timeout(WORKSPACES_REQUEST_TIMEOUT, rx).await {
                Ok(Ok(ClientMsg::WorkspaceListAllResult { items, error, .. })) => match error {
                    Some(e) => (conn.name.clone(), Err(e)),
                    None => (conn.name.clone(), Ok(items)),
                },
                Ok(Ok(_)) => (conn.name.clone(), Err("unexpected reply".into())),
                _ => (conn.name.clone(), Err("timeout".into())),
            }
        }));
    }

    let mut rows: Vec<WorkspaceRowDto> = Vec::new();
    let mut seen: std::collections::HashSet<(String, String, String)> =
        std::collections::HashSet::new();
    for t in tasks {
        let Ok((agent_name, result)) = t.await else {
            continue;
        };
        match result {
            Ok(items) => {
                for it in items {
                    let key = (agent_name.clone(), it.account.clone(), it.name.clone());
                    let has_client = state.app.workspaces.contains_key(&key);
                    let status = if has_client {
                        "active"
                    } else if it.tmux_alive {
                        "saved"
                    } else {
                        "fresh"
                    };
                    let ts = last_started.get(&key).copied();
                    seen.insert(key.clone());
                    rows.push(WorkspaceRowDto {
                        agent: agent_name.clone(),
                        account: it.account,
                        workspace: it.name,
                        status,
                        has_client,
                        tmux_alive: it.tmux_alive,
                        agent_online: true,
                        last_started_at: ts,
                    });
                }
            }
            Err(e) => {
                tracing::debug!(agent = %agent_name, error = %e, "list_all failed");
            }
        }
    }

    // Surface DB-tracked workspaces whose owning agent is offline:
    // they still belong on the inventory page, just shown as fresh
    // with agent_online=false. We deliberately *don't* fall back to
    // the sessions table here — using session history to reconstruct
    // the inventory would resurrect rows the admin just deleted via
    // the Delete button, because audit records (correctly) outlive a
    // binding deletion.
    let db_bindings = state
        .app
        .db
        .list_all_workspace_bindings()
        .await
        .unwrap_or_default();
    for b in db_bindings {
        let key = (b.agent.clone(), b.account.clone(), b.name.clone());
        if seen.contains(&key) {
            continue;
        }
        if online_names.contains(&b.agent) {
            // Agent is online but its list didn't include this
            // workspace — it was likely deleted on the agent side
            // out-of-band. Skip; the next refresh will catch up
            // either way.
            continue;
        }
        rows.push(WorkspaceRowDto {
            agent: b.agent,
            account: b.account,
            workspace: b.name,
            status: "fresh",
            has_client: false,
            tmux_alive: false,
            agent_online: false,
            last_started_at: last_started.get(&key).copied(),
        });
    }

    rows.sort_by(|a, b| {
        a.agent
            .cmp(&b.agent)
            .then_with(|| a.account.cmp(&b.account))
            .then_with(|| a.workspace.cmp(&b.workspace))
    });
    Json(rows).into_response()
}

// ---------------------------------------------------------------------
// Audit
// ---------------------------------------------------------------------

#[derive(Deserialize, Default)]
pub struct AuditQuery {
    #[serde(default)]
    pub account: Option<String>,
    #[serde(default)]
    pub agent: Option<String>,
    #[serde(default)]
    pub kind: Option<String>,
    #[serde(default)]
    pub since: Option<String>,
    #[serde(default)]
    pub until: Option<String>,
    #[serde(default)]
    pub page: Option<i64>,
    #[serde(default)]
    pub limit: Option<i64>,
}

#[derive(Serialize)]
struct AuditEventDto {
    id: i64,
    ts: i64,
    kind: String,
    account: Option<String>,
    agent: Option<String>,
    session_id: Option<String>,
    workspace: Option<String>,
    detail: Option<serde_json::Value>,
}

#[derive(Serialize)]
struct AuditPage {
    events: Vec<AuditEventDto>,
    total: i64,
    page: i64,
    page_size: i64,
}

pub async fn audit_list(
    State(state): State<AdminState>,
    Query(q): Query<AuditQuery>,
) -> Response {
    let page_size = q.limit.unwrap_or(50).clamp(1, 500);
    let page = q.page.unwrap_or(1).max(1);
    let offset = (page - 1) * page_size;
    let filter = AuditFilter {
        account: norm(&q.account),
        agent: norm(&q.agent),
        kind: norm(&q.kind),
        since: norm(&q.since).as_deref().and_then(parse_datetime_local),
        until: norm(&q.until).as_deref().and_then(parse_datetime_local),
    };
    let rows = match state
        .app
        .db
        .list_audit_events(&filter, page_size, offset)
        .await
    {
        Ok(r) => r,
        Err(e) => return internal(e),
    };
    let total = state
        .app
        .db
        .count_audit_events(&filter)
        .await
        .unwrap_or(rows.len() as i64);
    let events: Vec<AuditEventDto> = rows
        .into_iter()
        .map(|r| AuditEventDto {
            id: r.id,
            ts: r.ts,
            kind: r.kind,
            account: r.account,
            agent: r.agent,
            session_id: r.session_id,
            workspace: r.workspace,
            detail: r
                .detail
                .as_deref()
                .and_then(|s| serde_json::from_str(s).ok()),
        })
        .collect();
    Json(AuditPage {
        events,
        total,
        page,
        page_size,
    })
    .into_response()
}

pub async fn audit_kinds(State(state): State<AdminState>) -> Response {
    match state.app.db.distinct_audit_kinds().await {
        Ok(k) => Json(k).into_response(),
        Err(e) => internal(e),
    }
}

/// Union of distinct kinds from both backing tables of the activity
/// view. Populates the multi-select in the admin SPA so the operator
/// doesn't have to remember kind strings.
pub async fn activity_kinds(State(state): State<AdminState>) -> Response {
    match state.app.db.distinct_activity_kinds().await {
        Ok(k) => Json(k).into_response(),
        Err(e) => internal(e),
    }
}

// ---------------------------------------------------------------------
// User interactions (captured claude prompts; content hidden by default)
// ---------------------------------------------------------------------

#[derive(Deserialize, Default)]
pub struct InteractionsQuery {
    #[serde(default)]
    pub account: Option<String>,
    #[serde(default)]
    pub workspace: Option<String>,
    #[serde(default)]
    pub kind: Option<String>,
    /// Milliseconds since epoch — matches the wire format the agent
    /// already ships, so the admin UI can round-trip values from a
    /// row's `ts_ms` directly into a filter.
    #[serde(default)]
    pub since_ms: Option<i64>,
    #[serde(default)]
    pub until_ms: Option<i64>,
    #[serde(default)]
    pub limit: Option<i64>,
    #[serde(default)]
    pub offset: Option<i64>,
}

#[derive(Serialize)]
struct InteractionListItem {
    id: i64,
    account: String,
    agent: String,
    workspace: String,
    claude_session_id: String,
    prompt_id: Option<String>,
    parent_uuid: Option<String>,
    cwd: Option<String>,
    git_branch: Option<String>,
    ts_ms: i64,
    kind: String,
    /// Full prompt text, surfaced inline. (Earlier revision masked
    /// this to `[hidden]` and required a separate `/reveal` call;
    /// operator decided the masking wasn't worth the extra click
    /// since the interactions table is admin-gated already. The
    /// `/reveal` endpoint stays in place for callers that want the
    /// audit-write side effect.)
    content: String,
}

#[derive(Serialize)]
struct InteractionPage {
    items: Vec<InteractionListItem>,
    total: i64,
}

pub async fn interactions_list(
    State(state): State<AdminState>,
    Query(q): Query<InteractionsQuery>,
) -> Response {
    let limit = q.limit.unwrap_or(50).clamp(1, 500);
    let offset = q.offset.unwrap_or(0).max(0);
    let filter = crate::db::UserInteractionFilter {
        account: norm(&q.account),
        workspace: norm(&q.workspace),
        kind: norm(&q.kind),
        since_ms: q.since_ms,
        until_ms: q.until_ms,
    };
    let rows = match state.app.db.list_user_interactions(&filter, limit, offset).await {
        Ok(r) => r,
        Err(e) => return internal(e),
    };
    let total = state
        .app
        .db
        .count_user_interactions(&filter)
        .await
        .unwrap_or(rows.len() as i64);
    let items = rows
        .into_iter()
        .map(|r| InteractionListItem {
            id: r.id,
            account: r.account,
            agent: r.agent,
            workspace: r.workspace,
            claude_session_id: r.claude_session_id,
            prompt_id: r.prompt_id,
            parent_uuid: r.parent_uuid,
            cwd: r.cwd,
            git_branch: r.git_branch,
            ts_ms: r.ts_ms,
            kind: r.kind,
            content: r.content,
        })
        .collect();
    Json(InteractionPage { items, total }).into_response()
}

#[derive(Serialize)]
struct InteractionReveal {
    id: i64,
    content: String,
}

pub async fn interactions_reveal(
    State(state): State<AdminState>,
    headers: HeaderMap,
    Path(id): Path<i64>,
) -> Response {
    let row = match state.app.db.get_user_interaction(id).await {
        Ok(Some(r)) => r,
        Ok(None) => {
            return err(StatusCode::NOT_FOUND, "not_found", "no such interaction")
        }
        Err(e) => return internal(e),
    };

    // Forensic trail: who pulled which row's plaintext, when. The sid
    // is the admin session cookie — opaque, but the audit log + admin
    // session table together let an operator narrow down to a login.
    let admin_sid = super::session_cookie(&headers);
    let mut detail = serde_json::Map::new();
    detail.insert("interaction_id".into(), serde_json::Value::from(id));
    if let Some(sid) = admin_sid.as_deref() {
        detail.insert("admin_sid".into(), serde_json::Value::from(sid));
    }
    state.app.audit.write(crate::audit::AuditEvent {
        account: Some(row.account.clone()),
        agent: Some(row.agent.clone()),
        workspace: Some(row.workspace.clone()),
        reason: serde_json::to_string(&detail).ok(),
        ..crate::audit::AuditEvent::new("interaction_revealed")
    });

    Json(InteractionReveal {
        id: row.id,
        content: row.content,
    })
    .into_response()
}

// ---------------------------------------------------------------------
// Unified activity view (audit_events ∪ user_interactions)
// ---------------------------------------------------------------------
//
// Single feed for the admin "activity" page. Both source tables are
// projected to a common shape inside the DB layer (see
// `db::build_activity_select`); here we just normalise the query
// string, clamp paging, parse `detail` from text → JSON value, and
// keep the older `audit_list` / `interactions_list` endpoints
// untouched so existing pages don't regress.

#[derive(Deserialize, Default)]
pub struct ActivityQuery {
    /// "audit" | "interaction" | "all" (default). Anything else
    /// normalises to "all" — surfacing a 400 here is more annoying
    /// than helpful for a viewer endpoint.
    #[serde(default)]
    pub source: Option<String>,
    #[serde(default)]
    pub account: Option<String>,
    #[serde(default)]
    pub agent: Option<String>,
    #[serde(default)]
    pub workspace: Option<String>,
    #[serde(default)]
    pub kind: Option<String>,
    /// Millisecond epoch; inclusive lower bound.
    #[serde(default)]
    pub since_ms: Option<i64>,
    /// Millisecond epoch; inclusive upper bound.
    #[serde(default)]
    pub until_ms: Option<i64>,
    #[serde(default)]
    pub limit: Option<i64>,
    #[serde(default)]
    pub offset: Option<i64>,
}

#[derive(Serialize)]
struct ActivityItem {
    id: i64,
    source: String,
    ts_ms: i64,
    kind: String,
    account: Option<String>,
    agent: Option<String>,
    workspace: Option<String>,
    session_id: Option<String>,
    /// Parsed JSON object for both branches:
    /// - audit row: the raw `audit_events.detail` text parsed (null
    ///   if absent or unparseable — we log a warn but do not fail
    ///   the whole list).
    /// - interaction row: SQLite's `json_object(...)` text we built
    ///   in the union, parsed back into a `Value` so the wire is
    ///   uniform.
    detail: Option<serde_json::Value>,
}

#[derive(Serialize)]
struct ActivityPage {
    items: Vec<ActivityItem>,
    total: i64,
}

/// Parse the comma-separated `kind` query param into a Vec.
/// Empty / whitespace-only / missing → None (no filter). Values
/// are trimmed; empty tokens between commas are skipped so a
/// trailing/duplicate comma doesn't break the filter.
fn parse_kinds(raw: &Option<String>) -> Option<Vec<String>> {
    let s = raw.as_deref()?.trim();
    if s.is_empty() {
        return None;
    }
    let kinds: Vec<String> = s
        .split(',')
        .map(|p| p.trim().to_string())
        .filter(|p| !p.is_empty())
        .collect();
    if kinds.is_empty() { None } else { Some(kinds) }
}

/// Normalise the `source` query param. Anything outside the known
/// set collapses to `"all"`. We keep a lowercase canonical form so
/// the DB layer can compare with a single literal.
fn norm_source(raw: &Option<String>) -> Option<String> {
    match raw.as_deref().map(|s| s.trim().to_ascii_lowercase()) {
        Some(ref s) if s == "audit" || s == "interaction" => Some(s.clone()),
        Some(ref s) if s == "all" || s.is_empty() => None,
        // Unknown source string → treat as "all" (None). Logged at
        // debug so curious operators can tell why their filter
        // didn't take effect.
        Some(other) => {
            tracing::debug!(source = %other, "activity: unknown source value, falling back to 'all'");
            None
        }
        None => None,
    }
}

pub async fn activity_list(
    State(state): State<AdminState>,
    Query(q): Query<ActivityQuery>,
) -> Response {
    let limit = q.limit.unwrap_or(50).clamp(1, 500);
    let offset = q.offset.unwrap_or(0).max(0);
    let filter = crate::db::ActivityFilter {
        source: norm_source(&q.source),
        account: norm(&q.account),
        agent: norm(&q.agent),
        workspace: norm(&q.workspace),
        kind: parse_kinds(&q.kind),
        since_ms: q.since_ms,
        until_ms: q.until_ms,
    };
    let rows = match state.app.db.list_activity(&filter, limit, offset).await {
        Ok(r) => r,
        Err(e) => return internal(e),
    };
    // count fallbacks to rows.len() if the count query fails — same
    // pattern as audit_list / interactions_list, so the UI still
    // renders something useful instead of erroring out the whole page.
    let total = state
        .app
        .db
        .count_activity(&filter)
        .await
        .unwrap_or(rows.len() as i64);
    let items = rows
        .into_iter()
        .map(|r| {
            let detail = r.detail.as_deref().and_then(|s| {
                match serde_json::from_str::<serde_json::Value>(s) {
                    Ok(v) => Some(v),
                    Err(e) => {
                        tracing::warn!(
                            source = %r.source,
                            id = r.id,
                            error = %e,
                            "activity: detail JSON parse failed, returning null"
                        );
                        None
                    }
                }
            });
            ActivityItem {
                id: r.id,
                source: r.source,
                ts_ms: r.ts_ms,
                kind: r.kind,
                account: r.account,
                agent: r.agent,
                workspace: r.workspace,
                session_id: r.session_id,
                detail,
            }
        })
        .collect();
    Json(ActivityPage { items, total }).into_response()
}

// ---------------------------------------------------------------------
// Sessions
// ---------------------------------------------------------------------

#[derive(Deserialize, Default)]
pub struct SessionsQuery {
    #[serde(default)]
    pub account: Option<String>,
    #[serde(default)]
    pub agent: Option<String>,
    #[serde(default)]
    pub workspace: Option<String>,
    #[serde(default)]
    pub active: Option<bool>,
    #[serde(default)]
    pub page: Option<i64>,
    #[serde(default)]
    pub limit: Option<i64>,
}

#[derive(Serialize)]
struct SessionDto {
    session_id: String,
    account: String,
    agent: String,
    workspace: String,
    started_at: i64,
    ended_at: Option<i64>,
    ended_reason: Option<String>,
}

#[derive(Serialize)]
struct SessionsPage {
    sessions: Vec<SessionDto>,
    total: i64,
    page: i64,
    page_size: i64,
}

// ---------------------------------------------------------------------
// Session detail + messages
// ---------------------------------------------------------------------

#[derive(Serialize)]
struct SessionDetailDto {
    session_id: String,
    account: String,
    agent: String,
    workspace: String,
    started_at: i64,
    ended_at: Option<i64>,
    ended_reason: Option<String>,
    message_count: i64,
}

pub async fn session_detail(
    State(state): State<AdminState>,
    Path(session_id): Path<String>,
) -> Response {
    match state.app.db.get_session(&session_id).await {
        Ok(Some(s)) => {
            let count = state
                .app
                .db
                .count_messages_for_session(&session_id)
                .await
                .unwrap_or(0);
            Json(SessionDetailDto {
                session_id: s.session_id,
                account: s.account,
                agent: s.agent,
                workspace: s.workspace,
                started_at: s.started_at,
                ended_at: s.ended_at,
                ended_reason: s.ended_reason,
                message_count: count,
            })
            .into_response()
        }
        Ok(None) => err(StatusCode::NOT_FOUND, "not_found", "session not found"),
        Err(e) => internal(e),
    }
}

#[derive(Serialize)]
struct MessageDto {
    id: i64,
    ts: i64,
    kind: String,
    body: serde_json::Value,
}

#[derive(Deserialize, Default)]
pub struct MessagesQuery {
    #[serde(default)]
    pub limit: Option<i64>,
}

pub async fn session_messages(
    State(state): State<AdminState>,
    Path(session_id): Path<String>,
    Query(q): Query<MessagesQuery>,
) -> Response {
    let limit = q.limit.unwrap_or(500).clamp(1, 5000);
    let rows = match state
        .app
        .db
        .list_messages_for_session(&session_id, limit)
        .await
    {
        Ok(r) => r,
        Err(e) => return internal(e),
    };
    let dto: Vec<MessageDto> = rows
        .into_iter()
        .map(|r| MessageDto {
            id: r.id,
            ts: r.ts,
            kind: r.kind,
            body: serde_json::from_str(&r.body).unwrap_or(serde_json::Value::Null),
        })
        .collect();
    Json(dto).into_response()
}

pub async fn sessions_list(
    State(state): State<AdminState>,
    Query(q): Query<SessionsQuery>,
) -> Response {
    let page_size = q.limit.unwrap_or(50).clamp(1, 500);
    let page = q.page.unwrap_or(1).max(1);
    let offset = (page - 1) * page_size;
    let filter = SessionsFilter {
        account: norm(&q.account),
        agent: norm(&q.agent),
        workspace: norm(&q.workspace),
        active_only: q.active.unwrap_or(false),
        since: None,
    };
    let rows = match state
        .app
        .db
        .list_sessions(&filter, page_size, offset)
        .await
    {
        Ok(r) => r,
        Err(e) => return internal(e),
    };
    let total = state
        .app
        .db
        .count_sessions(&filter)
        .await
        .unwrap_or(rows.len() as i64);
    let sessions: Vec<SessionDto> = rows
        .into_iter()
        .map(|r| SessionDto {
            session_id: r.session_id,
            account: r.account,
            agent: r.agent,
            workspace: r.workspace,
            started_at: r.started_at,
            ended_at: r.ended_at,
            ended_reason: r.ended_reason,
        })
        .collect();
    Json(SessionsPage {
        sessions,
        total,
        page,
        page_size,
    })
    .into_response()
}

// ---------------------------------------------------------------------
// stats
// ---------------------------------------------------------------------
//
// Five admin-only analytics endpoints powering the dashboard charts.
// All time-window inputs accept `window=7d|30d` (default 7d on missing
// or unknown); date-window inputs accept `days=N` clamped to 1..=180.
// Endpoints never error on empty data — they return zeros / empty
// buckets so the frontend can render without special-casing.

fn parse_window_secs(s: Option<&String>) -> i64 {
    match s.map(String::as_str) {
        Some("30d") => 30 * 86_400,
        _ => 7 * 86_400,
    }
}

fn parse_days(s: Option<&String>) -> i64 {
    s.and_then(|v| v.parse::<i64>().ok())
        .unwrap_or(30)
        .clamp(1, 180)
}

#[derive(Serialize)]
struct LeaderboardRow {
    name: String,
    session_count: i64,
    total_duration_seconds: i64,
    message_count: i64,
}

pub async fn stats_leaderboard(
    State(state): State<AdminState>,
    Query(q): Query<std::collections::HashMap<String, String>>,
) -> Response {
    let window = parse_window_secs(q.get("window"));
    let now = chrono::Utc::now().timestamp();
    let cutoff = now - window;
    let group = match q.get("group").map(String::as_str) {
        Some("agent") => "agent",
        _ => "account",
    };

    // Two grouped queries against the same cutoff; we merge in Rust to
    // avoid a JOIN that explodes per-message rows then collapses.
    let sess_sql = format!(
        "SELECT {group} AS name, COUNT(*) AS sess_n,
                SUM(COALESCE(ended_at, ?1) - started_at) AS dur
           FROM sessions
          WHERE started_at >= ?2
          GROUP BY {group}"
    );
    let sess_rows = match sqlx::query(&sess_sql)
        .bind(now)
        .bind(cutoff)
        .fetch_all(&state.app.db.pool)
        .await
    {
        Ok(r) => r,
        Err(e) => return internal(e),
    };

    let msg_sql = format!(
        "SELECT s.{group} AS name, COUNT(m.id) AS msg_n
           FROM sessions s
           JOIN messages m ON m.cc_session_id = s.session_id
          WHERE s.started_at >= ?1
          GROUP BY s.{group}"
    );
    let msg_rows = match sqlx::query(&msg_sql)
        .bind(cutoff)
        .fetch_all(&state.app.db.pool)
        .await
    {
        Ok(r) => r,
        Err(e) => return internal(e),
    };

    use sqlx::Row;
    let mut merged: std::collections::HashMap<String, LeaderboardRow> =
        std::collections::HashMap::new();
    for row in sess_rows {
        let name: Option<String> = row.get("name");
        let Some(name) = name else { continue };
        let sess_n: i64 = row.get("sess_n");
        let dur: i64 = row.try_get("dur").unwrap_or(0);
        merged.insert(
            name.clone(),
            LeaderboardRow {
                name,
                session_count: sess_n,
                total_duration_seconds: dur,
                message_count: 0,
            },
        );
    }
    for row in msg_rows {
        let name: Option<String> = row.get("name");
        let Some(name) = name else { continue };
        let msg_n: i64 = row.get("msg_n");
        if let Some(e) = merged.get_mut(&name) {
            e.message_count = msg_n;
        } else {
            // Shouldn't happen — session group covers all sessions in
            // the window, and messages reference sessions via cc_session_id
            // joined against the same window. Be defensive.
            merged.insert(
                name.clone(),
                LeaderboardRow {
                    name,
                    session_count: 0,
                    total_duration_seconds: 0,
                    message_count: msg_n,
                },
            );
        }
    }

    let mut out: Vec<LeaderboardRow> = merged.into_values().collect();
    out.sort_by(|a, b| b.total_duration_seconds.cmp(&a.total_duration_seconds));
    out.truncate(20);
    Json(out).into_response()
}

#[derive(Serialize)]
struct DurationBucket {
    label: &'static str,
    from_seconds: i64,
    to_seconds: Option<i64>,
    count: i64,
}

#[derive(Serialize)]
struct SessionDurationStats {
    count: i64,
    mean_seconds: f64,
    median_seconds: i64,
    p95_seconds: i64,
    max_seconds: i64,
    buckets: Vec<DurationBucket>,
}

fn percentile_i64(sorted: &[i64], p: f64) -> i64 {
    if sorted.is_empty() {
        return 0;
    }
    let idx = ((sorted.len() as f64 - 1.0) * p).round() as usize;
    sorted[idx.min(sorted.len() - 1)]
}

pub async fn stats_session_duration(
    State(state): State<AdminState>,
    Query(q): Query<std::collections::HashMap<String, String>>,
) -> Response {
    let window = parse_window_secs(q.get("window"));
    let now = chrono::Utc::now().timestamp();
    let cutoff = now - window;

    let rows = match sqlx::query(
        "SELECT COALESCE(ended_at, ?1) - started_at AS d
           FROM sessions
          WHERE started_at >= ?2",
    )
    .bind(now)
    .bind(cutoff)
    .fetch_all(&state.app.db.pool)
    .await
    {
        Ok(r) => r,
        Err(e) => return internal(e),
    };

    use sqlx::Row;
    let mut durations: Vec<i64> = rows
        .into_iter()
        .map(|r| r.get::<i64, _>("d").max(0))
        .collect();
    durations.sort_unstable();

    let edges: [(&'static str, i64, Option<i64>); 7] = [
        ("<1m", 0, Some(60)),
        ("1-5m", 60, Some(300)),
        ("5-30m", 300, Some(1800)),
        ("30m-1h", 1800, Some(3600)),
        ("1-6h", 3600, Some(21_600)),
        ("6-24h", 21_600, Some(86_400)),
        (">24h", 86_400, None),
    ];
    let mut buckets: Vec<DurationBucket> = edges
        .iter()
        .map(|(label, from, to)| DurationBucket {
            label,
            from_seconds: *from,
            to_seconds: *to,
            count: 0,
        })
        .collect();
    for d in &durations {
        for b in buckets.iter_mut() {
            let in_range = *d >= b.from_seconds
                && match b.to_seconds {
                    Some(to) => *d < to,
                    None => true,
                };
            if in_range {
                b.count += 1;
                break;
            }
        }
    }

    let count = durations.len() as i64;
    let mean = if durations.is_empty() {
        0.0
    } else {
        durations.iter().sum::<i64>() as f64 / durations.len() as f64
    };
    let median = percentile_i64(&durations, 0.50);
    let p95 = percentile_i64(&durations, 0.95);
    let max = durations.last().copied().unwrap_or(0);

    Json(SessionDurationStats {
        count,
        mean_seconds: mean,
        median_seconds: median,
        p95_seconds: p95,
        max_seconds: max,
        buckets,
    })
    .into_response()
}

#[derive(Serialize)]
struct MessagesDailyRow {
    date: String,
    user: i64,
    assistant: i64,
    other: i64,
}

pub async fn stats_messages_daily(
    State(state): State<AdminState>,
    Query(q): Query<std::collections::HashMap<String, String>>,
) -> Response {
    let days = parse_days(q.get("days"));
    let now = chrono::Utc::now().timestamp();
    let cutoff = now - days * 86_400;

    let rows = match sqlx::query(
        "SELECT date(ts, 'unixepoch') AS day, kind, COUNT(*) AS n
           FROM messages
          WHERE ts >= ?1
          GROUP BY day, kind",
    )
    .bind(cutoff)
    .fetch_all(&state.app.db.pool)
    .await
    {
        Ok(r) => r,
        Err(e) => return internal(e),
    };

    use sqlx::Row;
    let mut by_day: std::collections::HashMap<String, (i64, i64, i64)> =
        std::collections::HashMap::new();
    for row in rows {
        let day: String = row.get("day");
        let kind: String = row.get("kind");
        let n: i64 = row.get("n");
        let e = by_day.entry(day).or_insert((0, 0, 0));
        match kind.as_str() {
            "user" => e.0 += n,
            "assistant" => e.1 += n,
            _ => e.2 += n,
        }
    }

    // Fill zero days. We anchor at "today UTC" and walk back days-1 days
    // so the response always has exactly `days` entries.
    let today = chrono::DateTime::<chrono::Utc>::from_timestamp(now, 0)
        .map(|d| d.date_naive())
        .unwrap_or_else(|| chrono::NaiveDate::from_ymd_opt(1970, 1, 1).unwrap());
    let mut out: Vec<MessagesDailyRow> = Vec::with_capacity(days as usize);
    for i in (0..days).rev() {
        let date = today - chrono::Duration::days(i);
        let key = date.format("%Y-%m-%d").to_string();
        let (u, a, o) = by_day.get(&key).copied().unwrap_or((0, 0, 0));
        out.push(MessagesDailyRow {
            date: key,
            user: u,
            assistant: a,
            other: o,
        });
    }
    Json(out).into_response()
}

#[derive(Serialize)]
struct CountBucket {
    label: &'static str,
    from: i64,
    to: Option<i64>,
    count: i64,
}

#[derive(Serialize)]
struct MessagesPerSessionStats {
    count: i64,
    mean: f64,
    median: i64,
    p95: i64,
    max: i64,
    buckets: Vec<CountBucket>,
}

pub async fn stats_messages_per_session(
    State(state): State<AdminState>,
    Query(q): Query<std::collections::HashMap<String, String>>,
) -> Response {
    let window = parse_window_secs(q.get("window"));
    let cutoff = chrono::Utc::now().timestamp() - window;

    let rows = match sqlx::query(
        "SELECT COUNT(m.id) AS n
           FROM sessions s
           LEFT JOIN messages m ON m.cc_session_id = s.session_id
          WHERE s.started_at >= ?1
          GROUP BY s.session_id",
    )
    .bind(cutoff)
    .fetch_all(&state.app.db.pool)
    .await
    {
        Ok(r) => r,
        Err(e) => return internal(e),
    };

    use sqlx::Row;
    let mut counts: Vec<i64> = rows.into_iter().map(|r| r.get::<i64, _>("n")).collect();
    counts.sort_unstable();

    let edges: [(&'static str, i64, Option<i64>); 6] = [
        ("0", 0, Some(0)),
        ("1-5", 1, Some(5)),
        ("6-20", 6, Some(20)),
        ("21-50", 21, Some(50)),
        ("51-100", 51, Some(100)),
        (">100", 101, None),
    ];
    let mut buckets: Vec<CountBucket> = edges
        .iter()
        .map(|(label, from, to)| CountBucket {
            label,
            from: *from,
            to: *to,
            count: 0,
        })
        .collect();
    for c in &counts {
        for b in buckets.iter_mut() {
            let in_range = *c >= b.from
                && match b.to {
                    Some(to) => *c <= to,
                    None => true,
                };
            if in_range {
                b.count += 1;
                break;
            }
        }
    }

    let count = counts.len() as i64;
    let mean = if counts.is_empty() {
        0.0
    } else {
        counts.iter().sum::<i64>() as f64 / counts.len() as f64
    };
    let median = percentile_i64(&counts, 0.50);
    let p95 = percentile_i64(&counts, 0.95);
    let max = counts.last().copied().unwrap_or(0);

    Json(MessagesPerSessionStats {
        count,
        mean,
        median,
        p95,
        max,
        buckets,
    })
    .into_response()
}

#[derive(Serialize)]
struct TokensDailyRow {
    date: String,
    input_tokens: i64,
    output_tokens: i64,
    cache_creation_tokens: i64,
    cache_read_tokens: i64,
}

pub async fn stats_tokens_daily(
    State(state): State<AdminState>,
    Query(q): Query<std::collections::HashMap<String, String>>,
) -> Response {
    let days = parse_days(q.get("days"));
    let now = chrono::Utc::now().timestamp();
    let cutoff = now - days * 86_400;

    // Sums the precomputed token columns (populated at insert + backfilled
    // for old rows). With the partial index on assistant rows covering
    // (ts, input_tokens, …), this is an index-only scan — no body reads, no
    // json_extract — which is what fixes the previously slow / 502 query.
    let rows = match sqlx::query(
        "SELECT date(ts, 'unixepoch') AS day,
                SUM(input_tokens) AS i,
                SUM(output_tokens) AS o,
                SUM(cache_creation_tokens) AS cc,
                SUM(cache_read_tokens) AS cr
           FROM messages
          WHERE ts >= ?1 AND kind = 'assistant'
          GROUP BY day",
    )
    .bind(cutoff)
    .fetch_all(&state.app.db.pool)
    .await
    {
        Ok(r) => r,
        Err(e) => return internal(e),
    };

    use sqlx::Row;
    let mut by_day: std::collections::HashMap<String, (i64, i64, i64, i64)> =
        std::collections::HashMap::new();
    for row in rows {
        let day: String = row.get("day");
        let i: i64 = row.try_get("i").unwrap_or(0);
        let o: i64 = row.try_get("o").unwrap_or(0);
        let cc: i64 = row.try_get("cc").unwrap_or(0);
        let cr: i64 = row.try_get("cr").unwrap_or(0);
        by_day.insert(day, (i, o, cc, cr));
    }

    let today = chrono::DateTime::<chrono::Utc>::from_timestamp(now, 0)
        .map(|d| d.date_naive())
        .unwrap_or_else(|| chrono::NaiveDate::from_ymd_opt(1970, 1, 1).unwrap());
    let mut out: Vec<TokensDailyRow> = Vec::with_capacity(days as usize);
    for i in (0..days).rev() {
        let date = today - chrono::Duration::days(i);
        let key = date.format("%Y-%m-%d").to_string();
        let (it, ot, cct, crt) = by_day.get(&key).copied().unwrap_or((0, 0, 0, 0));
        out.push(TokensDailyRow {
            date: key,
            input_tokens: it,
            output_tokens: ot,
            cache_creation_tokens: cct,
            cache_read_tokens: crt,
        });
    }
    Json(out).into_response()
}

// ---------------------------------------------------------------------
// Data maintenance — retention cleanup
// ---------------------------------------------------------------------
//
// Deletes old rows from the prunable time-series tables (messages,
// sessions, user_interactions) older than a chosen window; the audit
// trail is kept. GET previews the row counts; POST commits the delete
// and optionally VACUUMs to reclaim disk.

/// Resolve a retention window (in whole months: 1/3/6/12) to a cutoff
/// unix-seconds timestamp. Returns None for an unsupported value so the
/// handler can 400 rather than delete an unexpected range.
fn cleanup_cutoff(months: u32) -> Option<i64> {
    if !matches!(months, 1 | 3 | 6 | 12) {
        return None;
    }
    chrono::Utc::now()
        .checked_sub_months(chrono::Months::new(months))
        .map(|d| d.timestamp())
}

#[derive(Serialize)]
struct CleanupPreview {
    months: u32,
    cutoff: i64,
    messages: i64,
    sessions: i64,
    user_interactions: i64,
}

/// `GET /admin/api/maintenance/cleanup?months=N` — preview only.
pub async fn maintenance_cleanup_preview(
    State(state): State<AdminState>,
    Query(q): Query<std::collections::HashMap<String, String>>,
) -> Response {
    let months = q.get("months").and_then(|s| s.parse::<u32>().ok()).unwrap_or(0);
    let Some(cutoff) = cleanup_cutoff(months) else {
        return err(StatusCode::BAD_REQUEST, "invalid_input", "months must be 1, 3, 6, or 12");
    };
    match state.app.db.cleanup_counts(cutoff).await {
        Ok(c) => Json(CleanupPreview {
            months,
            cutoff,
            messages: c.messages,
            sessions: c.sessions,
            user_interactions: c.user_interactions,
        })
        .into_response(),
        Err(e) => internal(e),
    }
}

#[derive(Deserialize)]
pub struct CleanupRequest {
    months: u32,
    #[serde(default)]
    vacuum: bool,
}

#[derive(Serialize)]
struct CleanupResult {
    months: u32,
    cutoff: i64,
    deleted_messages: i64,
    deleted_sessions: i64,
    deleted_user_interactions: i64,
    vacuumed: bool,
}

/// `POST /admin/api/maintenance/cleanup` — delete + optional VACUUM.
pub async fn maintenance_cleanup(
    State(state): State<AdminState>,
    Json(req): Json<CleanupRequest>,
) -> Response {
    let Some(cutoff) = cleanup_cutoff(req.months) else {
        return err(StatusCode::BAD_REQUEST, "invalid_input", "months must be 1, 3, 6, or 12");
    };
    let deleted = match state.app.db.cleanup_delete(cutoff).await {
        Ok(c) => c,
        Err(e) => return internal(e),
    };
    let mut vacuumed = false;
    if req.vacuum {
        match state.app.db.vacuum().await {
            Ok(()) => vacuumed = true,
            Err(e) => tracing::warn!(error = %e, "vacuum after cleanup failed"),
        }
    }
    tracing::info!(
        months = req.months,
        messages = deleted.messages,
        sessions = deleted.sessions,
        user_interactions = deleted.user_interactions,
        vacuumed,
        "admin data cleanup executed"
    );
    Json(CleanupResult {
        months: req.months,
        cutoff,
        deleted_messages: deleted.messages,
        deleted_sessions: deleted.sessions,
        deleted_user_interactions: deleted.user_interactions,
        vacuumed,
    })
    .into_response()
}

// ---------------------------------------------------------------------
// Invite links
// ---------------------------------------------------------------------
//
// Admin-issued shareable URLs that mint a brand-new account on
// accept. The token lives in the public URL (`/invite/<token>`),
// the short `id` is the admin-side handle. `share_url` is derived
// from the Host header so the same hub serves correct URLs across
// dev / prod without a config knob.

const ADMIN_USERNAME_AS_CREATOR: &str = "admin";

#[derive(Serialize)]
struct InviteDto {
    id: String,
    label: Option<String>,
    token: String,
    share_url: String,
    max_uses: i64,
    used: i64,
    allowed_agents: Vec<String>,
    active: bool,
    created_at: i64,
    sandbox_mode: String,
}

/// Build a `https://<host>/invite/<token>` URL from the request's
/// Host header. Honours `X-Forwarded-Proto` when set by an upstream
/// proxy so behind-the-LB hubs still produce `https://` links;
/// falls back to `https://` otherwise (overwhelmingly the right
/// guess for a publicly-shared invite link).
fn build_share_url(headers: &HeaderMap, token: &str) -> String {
    let host = headers
        .get("host")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("localhost");
    let scheme = headers
        .get("x-forwarded-proto")
        .and_then(|v| v.to_str().ok())
        .map(|s| {
            // Some proxies forward a comma-separated list; first
            // hop is the closest to the client.
            s.split(',').next().unwrap_or(s).trim().to_string()
        })
        .filter(|s| s == "http" || s == "https")
        .unwrap_or_else(|| "https".to_string());
    format!("{scheme}://{host}/invite/{token}")
}

pub async fn invites_list(
    State(state): State<AdminState>,
    headers: HeaderMap,
) -> Response {
    let rows = match state.app.db.list_invites().await {
        Ok(r) => r,
        Err(e) => return internal(e),
    };
    let dto: Vec<InviteDto> = rows
        .into_iter()
        .map(|r| {
            let share_url = build_share_url(&headers, &r.token);
            InviteDto {
                id: r.id,
                label: r.label,
                token: r.token,
                share_url,
                max_uses: r.max_uses,
                used: r.used,
                allowed_agents: r.allowed_agents,
                active: r.active,
                created_at: r.created_at,
                sandbox_mode: r.sandbox_mode,
            }
        })
        .collect();
    Json(dto).into_response()
}

#[derive(Deserialize)]
pub struct CreateInviteRequest {
    #[serde(default)]
    pub label: Option<String>,
    /// 0 (or omitted) means unlimited uses.
    #[serde(default)]
    pub max_uses: Option<i64>,
    /// Agent names the new account is whitelisted for on accept.
    /// Empty means the user can log in but can't reach any agent
    /// until an admin grants access — fine for soft-onboarding.
    pub allowed_agents: Vec<String>,
    /// Sandbox mode applied to accounts created via this invite.
    /// Defaults to "strict" when omitted.
    #[serde(default)]
    pub sandbox_mode: Option<String>,
}

#[derive(Serialize)]
struct CreateInviteResponse {
    id: String,
    token: String,
    share_url: String,
}

pub async fn invites_create(
    State(state): State<AdminState>,
    headers: HeaderMap,
    Json(req): Json<CreateInviteRequest>,
) -> Response {
    let max_uses = req.max_uses.unwrap_or(0);
    if max_uses < 0 {
        return err(
            StatusCode::BAD_REQUEST,
            "invalid_input",
            "max_uses must be 0 (unlimited) or a positive integer",
        );
    }
    let label = norm(&req.label);
    // De-dup + trim agent list. Empty entries get filtered out so
    // a stray comma in the UI form doesn't produce a phantom
    // "" agent name down the line.
    let mut agents: Vec<String> = req
        .allowed_agents
        .into_iter()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    agents.sort();
    agents.dedup();

    let sandbox_mode = req
        .sandbox_mode
        .as_deref()
        .unwrap_or("strict")
        .to_string();
    if !matches!(sandbox_mode.as_str(), "strict" | "permissive" | "off") {
        return err(
            StatusCode::BAD_REQUEST,
            "invalid_input",
            "sandbox_mode must be one of: strict, permissive, off",
        );
    }

    let id = auth::generate_invite_id();
    let token = auth::generate_invite_token();
    if let Err(e) = state
        .app
        .db
        .insert_invite(
            &id,
            label.as_deref(),
            &token,
            max_uses,
            &agents,
            ADMIN_USERNAME_AS_CREATOR,
            &sandbox_mode,
        )
        .await
    {
        return internal(e);
    }
    let share_url = build_share_url(&headers, &token);
    (
        StatusCode::CREATED,
        Json(CreateInviteResponse {
            id,
            token,
            share_url,
        }),
    )
        .into_response()
}

#[derive(Deserialize)]
pub struct PatchInviteRequest {
    #[serde(default)]
    pub active: Option<bool>,
    #[serde(default)]
    pub max_uses: Option<i64>,
    /// `Some("")` clears the label (back to NULL); `Some("foo")` sets it.
    /// Omitting the field leaves it unchanged.
    #[serde(default)]
    pub label: Option<String>,
    #[serde(default)]
    pub allowed_agents: Option<Vec<String>>,
    #[serde(default)]
    pub sandbox_mode: Option<String>,
}

pub async fn invites_patch(
    State(state): State<AdminState>,
    Path(id): Path<String>,
    Json(req): Json<PatchInviteRequest>,
) -> Response {
    if let Some(active) = req.active {
        if let Err(e) = state.app.db.update_invite_active(&id, active).await {
            return err(StatusCode::NOT_FOUND, "not_found", e.to_string());
        }
    }
    if let Some(max_uses) = req.max_uses {
        if max_uses < 0 {
            return err(
                StatusCode::BAD_REQUEST,
                "invalid_input",
                "max_uses must be >= 0 (0 = unlimited)",
            );
        }
        if let Err(e) = state.app.db.update_invite_max_uses(&id, max_uses).await {
            return err(StatusCode::NOT_FOUND, "not_found", e.to_string());
        }
    }
    if let Some(label) = req.label {
        let trimmed = label.trim();
        let to_set = if trimmed.is_empty() { None } else { Some(trimmed) };
        if let Err(e) = state.app.db.update_invite_label(&id, to_set).await {
            return err(StatusCode::NOT_FOUND, "not_found", e.to_string());
        }
    }
    if let Some(agents) = req.allowed_agents {
        let mut cleaned: Vec<String> = agents
            .into_iter()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();
        cleaned.sort();
        cleaned.dedup();
        if let Err(e) = state.app.db.update_invite_allowed_agents(&id, &cleaned).await {
            return err(StatusCode::NOT_FOUND, "not_found", e.to_string());
        }
    }
    if let Some(mode) = req.sandbox_mode {
        if !matches!(mode.as_str(), "strict" | "permissive" | "off") {
            return err(
                StatusCode::BAD_REQUEST,
                "invalid_input",
                "sandbox_mode must be one of: strict, permissive, off",
            );
        }
        if let Err(e) = state.app.db.update_invite_sandbox_mode(&id, &mode).await {
            return err(StatusCode::NOT_FOUND, "not_found", e.to_string());
        }
    }
    StatusCode::NO_CONTENT.into_response()
}

pub async fn invites_delete(
    State(state): State<AdminState>,
    Path(id): Path<String>,
) -> Response {
    if let Err(e) = state.app.db.delete_invite(&id).await {
        return err(StatusCode::NOT_FOUND, "not_found", e.to_string());
    }
    StatusCode::NO_CONTENT.into_response()
}

#[derive(Serialize)]
struct InviteAcceptanceDto {
    account: String,
    accepted_at: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    real_name: Option<String>,
}

pub async fn invites_acceptances(
    State(state): State<AdminState>,
    Path(id): Path<String>,
) -> Response {
    // Confirm the invite still exists so a 404 means "no such invite"
    // and not "the invite has zero acceptances". Note acceptances
    // rows survive invite deletion on purpose — but the admin
    // browses them via the parent invite; if it's gone there's no
    // entry point anyway.
    match state.app.db.get_invite(&id).await {
        Ok(Some(_)) => {}
        Ok(None) => return err(StatusCode::NOT_FOUND, "not_found", "invite not found"),
        Err(e) => return internal(e),
    }
    let rows = match state.app.db.list_invite_acceptances(&id).await {
        Ok(r) => r,
        Err(e) => return internal(e),
    };
    let mut dto: Vec<InviteAcceptanceDto> = Vec::with_capacity(rows.len());
    for (account, accepted_at) in rows {
        let real_name = state
            .app
            .db
            .get_account(&account)
            .await
            .ok()
            .flatten()
            .and_then(|a| a.real_name);
        dto.push(InviteAcceptanceDto {
            account,
            accepted_at,
            real_name,
        });
    }
    Json(dto).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_tag_accepts_canonical_forms() {
        assert!(is_valid_version_tag("v0.0.0"));
        assert!(is_valid_version_tag("v1.10.1"));
        assert!(is_valid_version_tag("v1.11.0"));
        assert!(is_valid_version_tag("v999.999.999"));
    }

    #[test]
    fn version_tag_rejects_malformed() {
        assert!(!is_valid_version_tag(""));
        assert!(!is_valid_version_tag("1.10.1"), "missing v prefix");
        assert!(!is_valid_version_tag("v1.10"), "two components");
        assert!(!is_valid_version_tag("v1.10.1.0"), "four components");
        assert!(!is_valid_version_tag("v1.10.1-rc1"), "no prerelease support");
        assert!(!is_valid_version_tag("v1.10.x"));
    }

    #[test]
    fn target_triple_mapping_covers_release_matrix() {
        assert_eq!(map_target_to_release_os("aarch64-apple-darwin"), Some("macos-aarch64"));
        assert_eq!(map_target_to_release_os("x86_64-unknown-linux-musl"), Some("linux-x86_64"));
        assert_eq!(map_target_to_release_os("aarch64-unknown-linux-musl"), Some("linux-aarch64"));
        // Targets we don't ship for surface as None so callers can
        // emit a clear 400 instead of a 5xx.
        assert_eq!(map_target_to_release_os("x86_64-apple-darwin"), None);
        assert_eq!(map_target_to_release_os("unknown"), None);
        assert_eq!(map_target_to_release_os(""), None);
    }
}
