//! Per-route handlers for the admin UI. HTML is rendered as inline
//! string literals for M2 — we'll migrate to `askama` templates once
//! the template count justifies it.

use super::{AdminState, SESSION_COOKIE};
use crate::auth;
use crate::db::AuditFilter;
use axum::{
    extract::{Form, Path, Query, State},
    http::{header::SET_COOKIE, HeaderMap, StatusCode},
    response::{Html, IntoResponse, Redirect, Response},
};
use serde::Deserialize;

// (Form needs the `form` feature on axum, which is enabled by default.)

// ---------------------------------------------------------------------
// utility
// ---------------------------------------------------------------------

fn html_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(c),
        }
    }
    out
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
        // last 6 chars are enough to disambiguate without revealing much
        token.chars().skip(n - 6).collect()
    }
}

const LOGIN_OK_COOKIE_MAX_AGE: i64 = 60 * 60 * 12; // 12 hours

// ---------------------------------------------------------------------
// /admin/login
// ---------------------------------------------------------------------

pub async fn login_page() -> Html<&'static str> {
    Html(LOGIN_HTML)
}

#[derive(Deserialize)]
pub struct LoginForm {
    token: String,
}

pub async fn login_submit(
    State(state): State<AdminState>,
    Form(form): Form<LoginForm>,
) -> Response {
    match state.auth.login(form.token.trim()) {
        Some(sid) => {
            let cookie = format!(
                "{name}={sid}; HttpOnly; SameSite=Strict; Path=/admin; Max-Age={age}",
                name = SESSION_COOKIE,
                sid = sid,
                age = LOGIN_OK_COOKIE_MAX_AGE,
            );
            let mut headers = HeaderMap::new();
            headers.insert(SET_COOKIE, cookie.parse().unwrap());
            (headers, Redirect::to("/admin/")).into_response()
        }
        None => {
            // Same form, with an inline error banner.
            let html = LOGIN_HTML.replace("<!--ERR-->", LOGIN_ERROR_BANNER);
            (StatusCode::UNAUTHORIZED, Html(html)).into_response()
        }
    }
}

// ---------------------------------------------------------------------
// /admin/logout
// ---------------------------------------------------------------------

pub async fn logout(State(state): State<AdminState>, headers: HeaderMap) -> Response {
    if let Some(sid) = super::session_cookie(&headers) {
        state.auth.logout(&sid);
    }
    // Clear cookie by setting Max-Age=0
    let cookie = format!(
        "{name}=; HttpOnly; SameSite=Strict; Path=/admin; Max-Age=0",
        name = SESSION_COOKIE
    );
    let mut out = HeaderMap::new();
    out.insert(SET_COOKIE, cookie.parse().unwrap());
    (out, Redirect::to("/admin/login")).into_response()
}

// ---------------------------------------------------------------------
// /admin/  (dashboard, protected)
// ---------------------------------------------------------------------

pub async fn dashboard(State(state): State<AdminState>) -> Response {
    let n = state.app.db.account_count().await.unwrap_or(0);
    let html = DASHBOARD_HTML.replace("<!--ACCOUNTS-->", &n.to_string());
    Html(html).into_response()
}

// ---------------------------------------------------------------------
// /admin/accounts  (CRUD, protected)
// ---------------------------------------------------------------------

pub async fn accounts_list(State(state): State<AdminState>) -> Response {
    let accounts = match state.app.db.list_accounts().await {
        Ok(a) => a,
        Err(e) => {
            return Html(error_page(&format!("listing accounts: {}", e))).into_response();
        }
    };
    let mut rows = String::new();
    if accounts.is_empty() {
        rows.push_str(r#"<tr><td colspan="5" style="opacity:0.6">no accounts yet — create one below.</td></tr>"#);
    } else {
        for a in &accounts {
            let prefix = a.token_prefix.as_deref().unwrap_or("(legacy)");
            let status = if a.disabled { "disabled" } else { "active" };
            let toggle_label = if a.disabled { "Enable" } else { "Disable" };
            let name_esc = html_escape(&a.name);
            rows.push_str(&format!(
                r##"<tr>
  <td><code>{name}</code></td>
  <td><code style="opacity:0.6">…{prefix}</code></td>
  <td>{status}</td>
  <td>{created}</td>
  <td class="actions">
    <form method="POST" action="/admin/accounts/{name}/rotate" style="display:inline;">
      <button type="submit">Rotate token</button>
    </form>
    <form method="POST" action="/admin/accounts/{name}/toggle" style="display:inline;">
      <button type="submit">{toggle}</button>
    </form>
    <form method="POST" action="/admin/accounts/{name}/delete" style="display:inline;"
          onsubmit="return confirm('Delete account {name}? This cannot be undone.');">
      <button type="submit" class="danger">Delete</button>
    </form>
  </td>
</tr>
"##,
                name = name_esc,
                prefix = html_escape(prefix),
                status = status,
                created = unix_to_short(a.created_at),
                toggle = toggle_label,
            ));
        }
    }
    let html = ACCOUNTS_HTML
        .replace("<!--HEAD-->", SHELL_HEAD)
        .replace("<!--FOOT-->", SHELL_FOOT)
        .replace("<!--ROWS-->", &rows);
    Html(html).into_response()
}

#[derive(Deserialize)]
pub struct CreateAccountForm {
    pub name: String,
}

pub async fn accounts_create(
    State(state): State<AdminState>,
    Form(form): Form<CreateAccountForm>,
) -> Response {
    let name = form.name.trim().to_string();
    if !valid_account_name(&name) {
        return Html(error_page(
            "Account name must be [a-z0-9_-]{1..64} (case sensitive).",
        ))
        .into_response();
    }
    match state.app.db.account_exists(&name).await {
        Ok(true) => {
            return Html(error_page(&format!("Account '{}' already exists.", name)))
                .into_response();
        }
        Ok(false) => {}
        Err(e) => return Html(error_page(&format!("db: {}", e))).into_response(),
    }
    let token = auth::generate_token();
    let hash = match auth::hash_token(&token) {
        Ok(h) => h,
        Err(e) => return Html(error_page(&format!("hash: {}", e))).into_response(),
    };
    let prefix = token_prefix(&token);
    if let Err(e) = state
        .app
        .db
        .insert_account(&name, &hash, Some(&prefix))
        .await
    {
        return Html(error_page(&format!("db insert: {}", e))).into_response();
    }
    Html(token_shown_once_page(
        "Account created",
        &format!("Hand <code>{}</code>'s account token to them now:", html_escape(&name)),
        &token,
    ))
    .into_response()
}

pub async fn accounts_rotate(
    State(state): State<AdminState>,
    Path(name): Path<String>,
) -> Response {
    if !valid_account_name(&name) {
        return Html(error_page("invalid account name")).into_response();
    }
    let token = auth::generate_token();
    let hash = match auth::hash_token(&token) {
        Ok(h) => h,
        Err(e) => return Html(error_page(&format!("hash: {}", e))).into_response(),
    };
    let prefix = token_prefix(&token);
    if let Err(e) = state.app.db.update_account_token(&name, &hash, &prefix).await {
        return Html(error_page(&format!("rotate: {}", e))).into_response();
    }
    Html(token_shown_once_page(
        "Token rotated",
        &format!(
            "Old token for <code>{}</code> no longer works. New token:",
            html_escape(&name)
        ),
        &token,
    ))
    .into_response()
}

pub async fn accounts_toggle(
    State(state): State<AdminState>,
    Path(name): Path<String>,
) -> Response {
    let accounts = state.app.db.list_accounts().await.unwrap_or_default();
    let current = accounts.iter().find(|a| a.name == name);
    let Some(a) = current else {
        return Html(error_page("account not found")).into_response();
    };
    let new_disabled = !a.disabled;
    if let Err(e) = state.app.db.set_account_disabled(&name, new_disabled).await {
        return Html(error_page(&format!("toggle: {}", e))).into_response();
    }
    Redirect::to("/admin/accounts").into_response()
}

pub async fn accounts_delete(
    State(state): State<AdminState>,
    Path(name): Path<String>,
) -> Response {
    if let Err(e) = state.app.db.delete_account(&name).await {
        return Html(error_page(&format!("delete: {}", e))).into_response();
    }
    Redirect::to("/admin/accounts").into_response()
}

fn unix_to_short(ts: i64) -> String {
    chrono::DateTime::<chrono::Utc>::from_timestamp(ts, 0)
        .map(|d| d.format("%Y-%m-%d").to_string())
        .unwrap_or_else(|| "—".into())
}

fn unix_to_iso(ts: i64) -> String {
    chrono::DateTime::<chrono::Utc>::from_timestamp(ts, 0)
        .map(|d| d.format("%Y-%m-%d %H:%M:%S").to_string())
        .unwrap_or_else(|| "—".into())
}

/// Parse a `<input type="datetime-local">` value ("YYYY-MM-DDTHH:MM[:SS]")
/// or a plain ISO date ("YYYY-MM-DD") into a UTC unix timestamp.
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

// ---------------------------------------------------------------------
// /admin/audit  (timeline, protected)
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
}

const AUDIT_PAGE_SIZE: i64 = 100;

pub async fn audit_list(
    State(state): State<AdminState>,
    Query(q): Query<AuditQuery>,
) -> Response {
    fn norm(v: &Option<String>) -> Option<String> {
        v.as_ref().map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
    }
    let acct = norm(&q.account);
    let agent = norm(&q.agent);
    let kind = norm(&q.kind);
    let since_raw = norm(&q.since);
    let until_raw = norm(&q.until);
    let filter = AuditFilter {
        account: acct.clone(),
        agent: agent.clone(),
        kind: kind.clone(),
        since: since_raw.as_deref().and_then(parse_datetime_local),
        until: until_raw.as_deref().and_then(parse_datetime_local),
    };

    let page = q.page.unwrap_or(1).max(1);
    let offset = (page - 1) * AUDIT_PAGE_SIZE;
    let rows = match state
        .app
        .db
        .list_audit_events(&filter, AUDIT_PAGE_SIZE, offset)
        .await
    {
        Ok(r) => r,
        Err(e) => {
            return Html(error_page(&format!("audit query: {}", e))).into_response();
        }
    };
    let total = state
        .app
        .db
        .count_audit_events(&filter)
        .await
        .unwrap_or(rows.len() as i64);
    let kinds = state.app.db.distinct_audit_kinds().await.unwrap_or_default();

    // Build kind <select>
    let mut kind_options = String::new();
    kind_options.push_str(r#"<option value="">(any)</option>"#);
    for k in &kinds {
        let sel = if kind.as_deref() == Some(k.as_str()) {
            " selected"
        } else {
            ""
        };
        kind_options.push_str(&format!(
            r#"<option value="{k}"{sel}>{k}</option>"#,
            k = html_escape(k),
            sel = sel
        ));
    }

    // Build table rows
    let mut body = String::new();
    if rows.is_empty() {
        body.push_str(
            r#"<tr><td colspan="7" style="opacity:0.6">no events match these filters.</td></tr>"#,
        );
    } else {
        for r in &rows {
            let sid_short = r
                .session_id
                .as_deref()
                .map(|s| s.chars().take(8).collect::<String>())
                .unwrap_or_default();
            body.push_str(&format!(
                r##"<tr>
  <td><time>{ts}</time></td>
  <td><code>{kind}</code></td>
  <td>{acct}</td>
  <td>{agt}</td>
  <td>{ws}</td>
  <td><code style="opacity:0.6">{sid}</code></td>
  <td><code style="opacity:0.6">{detail}</code></td>
</tr>
"##,
                ts = unix_to_iso(r.ts),
                kind = html_escape(&r.kind),
                acct = html_escape(r.account.as_deref().unwrap_or("—")),
                agt = html_escape(r.agent.as_deref().unwrap_or("—")),
                ws = html_escape(r.workspace.as_deref().unwrap_or("—")),
                sid = html_escape(&sid_short),
                detail = html_escape(r.detail.as_deref().unwrap_or("")),
            ));
        }
    }

    // Pagination links
    let last_page = ((total - 1).max(0) / AUDIT_PAGE_SIZE) + 1;
    let prev_link = if page > 1 {
        format!(
            r#"<a href="{}">← Prev</a>"#,
            audit_url(&acct, &agent, &kind, &since_raw, &until_raw, page - 1)
        )
    } else {
        r#"<span style="opacity:0.4">← Prev</span>"#.into()
    };
    let next_link = if page < last_page {
        format!(
            r#"<a href="{}">Next →</a>"#,
            audit_url(&acct, &agent, &kind, &since_raw, &until_raw, page + 1)
        )
    } else {
        r#"<span style="opacity:0.4">Next →</span>"#.into()
    };

    let html = AUDIT_HTML
        .replace("<!--HEAD-->", SHELL_HEAD)
        .replace("<!--FOOT-->", SHELL_FOOT)
        .replace("<!--ROWS-->", &body)
        .replace("<!--KIND_OPTIONS-->", &kind_options)
        .replace("<!--ACCT_VAL-->", &html_escape(acct.as_deref().unwrap_or("")))
        .replace("<!--AGENT_VAL-->", &html_escape(agent.as_deref().unwrap_or("")))
        .replace(
            "<!--SINCE_VAL-->",
            &html_escape(since_raw.as_deref().unwrap_or("")),
        )
        .replace(
            "<!--UNTIL_VAL-->",
            &html_escape(until_raw.as_deref().unwrap_or("")),
        )
        .replace("<!--PREV-->", &prev_link)
        .replace("<!--NEXT-->", &next_link)
        .replace("<!--PAGE_INFO-->", &format!("page {} of {}", page, last_page))
        .replace("<!--TOTAL-->", &total.to_string());
    Html(html).into_response()
}

fn audit_url(
    account: &Option<String>,
    agent: &Option<String>,
    kind: &Option<String>,
    since: &Option<String>,
    until: &Option<String>,
    page: i64,
) -> String {
    let mut parts: Vec<String> = vec![format!("page={}", page)];
    let mut push = |k: &str, v: &Option<String>| {
        if let Some(s) = v {
            if !s.is_empty() {
                parts.push(format!("{}={}", k, urlencoding(s)));
            }
        }
    };
    push("account", account);
    push("agent", agent);
    push("kind", kind);
    push("since", since);
    push("until", until);
    format!("/admin/audit?{}", parts.join("&"))
}

fn urlencoding(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char);
            }
            _ => out.push_str(&format!("%{:02X}", b)),
        }
    }
    out
}

// ---------------------------------------------------------------------
// Templates (inline; askama later)
// ---------------------------------------------------------------------

const LOGIN_HTML: &str = r##"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<title>cloudcode admin · sign in</title>
<style>
:root { color-scheme: light dark; }
body { font-family: system-ui, sans-serif; max-width: 24rem; margin: 4rem auto; padding: 0 1rem; }
h1 { font-size: 1.25rem; margin-bottom: 1.5rem; }
form { display: grid; gap: 0.75rem; }
input[type=password] { padding: 0.5rem; font-size: 1rem; border: 1px solid #888; border-radius: 4px; background: transparent; color: inherit; }
button { padding: 0.5rem 1rem; font-size: 1rem; cursor: pointer; }
.err { background: #fee; border-left: 3px solid #c33; padding: 0.5rem 0.75rem; color: #900; margin-bottom: 1rem; border-radius: 0 4px 4px 0; }
footer { margin-top: 2rem; font-size: 0.8rem; opacity: 0.6; }
</style>
</head>
<body>
<h1>cloudcode admin</h1>
<!--ERR-->
<form method="POST" action="/admin/login">
  <label>
    Admin token
    <input type="password" name="token" autofocus required>
  </label>
  <button type="submit">Sign in</button>
</form>
<footer>The plaintext token was printed once by <code>cloudcode-hub --init</code>.</footer>
</body>
</html>"##;

const LOGIN_ERROR_BANNER: &str = r#"<p class="err">Invalid token.</p>"#;

const SHELL_HEAD: &str = r##"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<title>cloudcode admin</title>
<style>
:root { color-scheme: light dark; }
body { font-family: system-ui, sans-serif; max-width: 60rem; margin: 2rem auto; padding: 0 1rem; }
header { display: flex; justify-content: space-between; align-items: baseline; margin-bottom: 1.5rem; }
h1 { font-size: 1.5rem; margin: 0; }
nav { margin-bottom: 1.5rem; }
nav a { margin-right: 1rem; }
.card { padding: 1rem; border: 1px solid #888; border-radius: 4px; }
.stat { font-size: 2rem; font-weight: 600; }
.label { opacity: 0.6; font-size: 0.9rem; }
table { width: 100%; border-collapse: collapse; }
th, td { padding: 0.5rem 0.75rem; text-align: left; border-bottom: 1px solid #888; }
th { font-weight: 600; font-size: 0.85rem; text-transform: uppercase; letter-spacing: 0.05em; opacity: 0.7; }
code { font-size: 0.95em; }
button { padding: 0.35rem 0.7rem; font-size: 0.9rem; cursor: pointer; margin-right: 0.25rem; }
button.danger { color: #c33; }
form.create { display: flex; gap: 0.5rem; align-items: center; margin-top: 1rem; }
form.create input { padding: 0.4rem 0.6rem; border: 1px solid #888; border-radius: 4px; background: transparent; color: inherit; }
.flash { border-left: 3px solid #f80; padding: 0.75rem 1rem; background: #fff8e6; color: #663; margin-bottom: 1rem; border-radius: 0 4px 4px 0; }
.token { padding: 0.75rem 1rem; background: #1117; color: #f6e8b8; font-family: ui-monospace, SFMono-Regular, Menlo, monospace; font-size: 1rem; word-break: break-all; border-radius: 4px; user-select: all; }
.err { background: #fee; border-left: 3px solid #c33; padding: 0.75rem 1rem; color: #900; margin-bottom: 1rem; border-radius: 0 4px 4px 0; }
@media (prefers-color-scheme: dark) {
  .flash { background: #443300; color: #f0d77b; }
  .err { background: #4a1c1c; color: #f3a3a3; }
}
</style>
</head>
<body>
<header>
  <h1>cloudcode admin</h1>
  <form method="POST" action="/admin/logout" style="margin:0;">
    <button type="submit">Sign out</button>
  </form>
</header>
<nav>
  <a href="/admin/">Dashboard</a>
  <a href="/admin/accounts">Accounts</a>
  <a href="/admin/audit">Audit</a>
  <span style="opacity:0.4">sessions (coming)</span>
</nav>
"##;

const SHELL_FOOT: &str = r##"
</body>
</html>"##;

const DASHBOARD_HTML: &str = r##"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<title>cloudcode admin</title>
<style>
:root { color-scheme: light dark; }
body { font-family: system-ui, sans-serif; max-width: 60rem; margin: 2rem auto; padding: 0 1rem; }
header { display: flex; justify-content: space-between; align-items: baseline; margin-bottom: 1.5rem; }
h1 { font-size: 1.5rem; margin: 0; }
nav a { margin-right: 1rem; }
.card { padding: 1rem; border: 1px solid #888; border-radius: 4px; }
.stat { font-size: 2rem; font-weight: 600; }
.label { opacity: 0.6; font-size: 0.9rem; }
</style>
</head>
<body>
<header>
  <h1>cloudcode admin</h1>
  <form method="POST" action="/admin/logout" style="margin:0;">
    <button type="submit">Sign out</button>
  </form>
</header>
<nav>
  <a href="/admin/">Dashboard</a>
  <a href="/admin/accounts">Accounts</a>
  <a href="/admin/audit">Audit</a>
  <span style="opacity:0.4">sessions (coming)</span>
</nav>
<section class="card" style="margin-top: 1.5rem; max-width: 14rem;">
  <div class="label">Accounts</div>
  <div class="stat"><!--ACCOUNTS--></div>
</section>
</body>
</html>"##;

const AUDIT_HTML: &str = r##"<!--HEAD-->
<form method="GET" action="/admin/audit" style="display:flex; gap:0.6rem; align-items:flex-end; flex-wrap:wrap; margin-bottom:1rem;">
  <label>account <input name="account" value="<!--ACCT_VAL-->" placeholder="alice"></label>
  <label>agent <input name="agent" value="<!--AGENT_VAL-->" placeholder="petez-mbp"></label>
  <label>kind <select name="kind"><!--KIND_OPTIONS--></select></label>
  <label>since <input type="datetime-local" name="since" value="<!--SINCE_VAL-->"></label>
  <label>until <input type="datetime-local" name="until" value="<!--UNTIL_VAL-->"></label>
  <button type="submit">Filter</button>
  <a href="/admin/audit" style="margin-left:0.5rem;">reset</a>
</form>
<div style="opacity:0.6; margin-bottom:0.5rem;"><!--TOTAL--> events match</div>
<table>
  <thead>
    <tr><th>Time (UTC)</th><th>Kind</th><th>Account</th><th>Agent</th><th>Workspace</th><th>Session</th><th>Detail</th></tr>
  </thead>
  <tbody>
    <!--ROWS-->
  </tbody>
</table>
<div style="display:flex; justify-content:space-between; margin-top:1rem; align-items:center;">
  <!--PREV-->
  <span style="opacity:0.6;"><!--PAGE_INFO--></span>
  <!--NEXT-->
</div>
<!--FOOT-->"##;

const ACCOUNTS_HTML: &str = r##"<!--HEAD-->
<table>
  <thead>
    <tr><th>Name</th><th>Token suffix</th><th>Status</th><th>Created</th><th>Actions</th></tr>
  </thead>
  <tbody>
    <!--ROWS-->
  </tbody>
</table>
<form class="create" method="POST" action="/admin/accounts">
  <label for="name">New account name</label>
  <input id="name" name="name" required pattern="[A-Za-z0-9_-]{1,64}" placeholder="alice">
  <button type="submit">Create</button>
</form>
<!--FOOT-->"##;

fn error_page(msg: &str) -> String {
    format!(
        "{head}<div class=\"err\">{msg}</div><p><a href=\"/admin/accounts\">Back to accounts</a></p>{foot}",
        head = SHELL_HEAD,
        foot = SHELL_FOOT,
        msg = html_escape(msg),
    )
}

fn token_shown_once_page(title: &str, lead: &str, token: &str) -> String {
    format!(
        r##"{head}<div class="flash"><strong>{title}.</strong> This token will only be shown once. Copy it now.</div>
<p>{lead}</p>
<div class="token">{token}</div>
<p><a href="/admin/accounts">← Back to accounts</a></p>
{foot}"##,
        head = SHELL_HEAD,
        foot = SHELL_FOOT,
        title = html_escape(title),
        lead = lead, // raw — caller pre-escapes interpolation but allows <code> markup
        token = html_escape(token),
    )
}
