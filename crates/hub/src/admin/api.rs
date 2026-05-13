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

#[derive(Deserialize)]
pub struct LoginRequest {
    pub token: String,
}

pub async fn login(
    State(state): State<AdminState>,
    Json(req): Json<LoginRequest>,
) -> Response {
    let Some(sid) = state.auth.login(req.token.trim()) else {
        return err(StatusCode::UNAUTHORIZED, "invalid_token", "invalid admin token");
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
        state.auth.logout(&sid);
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
    (StatusCode::OK, Json(json!({"ok": true}))).into_response()
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
    token_prefix: Option<String>,
    created_at: i64,
    disabled: bool,
    /// Agents whitelisted for this account. Empty = locked out (strict
    /// whitelist semantics; admin must grant access from the editor).
    allowed_agents: Vec<String>,
}

pub async fn accounts_list(State(state): State<AdminState>) -> Response {
    let rows = match state.app.db.list_accounts().await {
        Ok(r) => r,
        Err(e) => return internal(e),
    };
    let mut dto: Vec<AccountDto> = Vec::with_capacity(rows.len());
    for a in rows {
        let allowed = state
            .app
            .db
            .list_allowed_agents(&a.name)
            .await
            .unwrap_or_default();
        dto.push(AccountDto {
            name: a.name,
            token_prefix: a.token_prefix,
            created_at: a.created_at,
            disabled: a.disabled,
            allowed_agents: allowed,
        });
    }
    Json(dto).into_response()
}

#[derive(Deserialize)]
pub struct CreateAccountRequest {
    pub name: String,
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
        .insert_account(&name, &hash, Some(&prefix))
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

pub async fn accounts_delete(
    State(state): State<AdminState>,
    Path(name): Path<String>,
) -> Response {
    if let Err(e) = state.app.db.delete_account(&name).await {
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

    // Surface historical workspaces whose agent is offline (or didn't
    // respond): they still belong on the inventory page, just shown as
    // fresh with agent_online=false so the admin can see them.
    for ((agent, account, workspace), ts) in last_started.iter() {
        let key = (agent.clone(), account.clone(), workspace.clone());
        if seen.contains(&key) {
            continue;
        }
        let online = online_names.contains(agent);
        if online {
            // Agent is online but its list didn't include this workspace
            // — it was likely deleted on the agent side. Skip.
            continue;
        }
        rows.push(WorkspaceRowDto {
            agent: agent.clone(),
            account: account.clone(),
            workspace: workspace.clone(),
            status: "fresh",
            has_client: false,
            tmux_alive: false,
            agent_online: false,
            last_started_at: Some(*ts),
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
