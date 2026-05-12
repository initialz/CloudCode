//! Per-route handlers for the admin UI. HTML is rendered as inline
//! string literals for M2 — we'll migrate to `askama` templates once
//! the template count justifies it.

use super::{AdminState, SESSION_COOKIE};
use axum::{
    extract::{Form, State},
    http::{header::SET_COOKIE, HeaderMap, StatusCode},
    response::{Html, IntoResponse, Redirect, Response},
};
use serde::Deserialize;

// (Form needs the `form` feature on axum, which is enabled by default.)

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
    let n = state
        .app
        .db
        .account_count()
        .await
        .unwrap_or(0);
    let html = DASHBOARD_HTML.replace("<!--ACCOUNTS-->", &n.to_string());
    Html(html).into_response()
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

const DASHBOARD_HTML: &str = r##"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<title>cloudcode admin</title>
<style>
:root { color-scheme: light dark; }
body { font-family: system-ui, sans-serif; max-width: 60rem; margin: 2rem auto; padding: 0 1rem; }
header { display: flex; justify-content: space-between; align-items: baseline; margin-bottom: 2rem; }
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
  <span style="opacity:0.4">accounts · audit · sessions (coming)</span>
</nav>
<section class="card" style="margin-top: 1.5rem; max-width: 14rem;">
  <div class="label">Accounts</div>
  <div class="stat"><!--ACCOUNTS--></div>
</section>
</body>
</html>"##;
