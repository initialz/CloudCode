//! Serve the Vite-built admin SPA from inside the hub binary.
//!
//! The frontend is in `admin-ui/`. `cd admin-ui && npm run build`
//! drops `admin-ui/dist/{index.html, assets/*}` and `rust-embed` slurps
//! that directory into the binary at compile time (`debug-embed`
//! feature on, so debug builds also embed — no runtime filesystem
//! dependency).
//!
//! Routing rules:
//! - exact hits under `/admin/assets/<hash>.{js,css,…}` serve their
//!   long-cache hashed asset
//! - any other `/admin/*` path falls through to `index.html` so the
//!   React Router takes over (deep links, refresh, etc.)

use axum::{
    body::Body,
    extract::Path,
    http::{header, HeaderValue, StatusCode, Uri},
    response::{IntoResponse, Response},
};
use rust_embed::{EmbeddedFile, RustEmbed};

#[derive(RustEmbed)]
#[folder = "../../admin-ui/dist/"]
struct Asset;

/// `/admin` and `/admin/` and any `/admin/<rest>` that isn't an asset
/// → `index.html`. Vite emits long-cache `assets/<hash>` paths so
/// those are handled by `serve_asset` below; this is just the shell.
pub async fn serve_index(_uri: Uri) -> Response {
    match Asset::get("index.html") {
        Some(file) => file_response("index.html", file),
        None => (StatusCode::NOT_FOUND, "admin-ui not built").into_response(),
    }
}

/// `/admin/assets/*path` — serve the hashed bundle file. 404s fall back
/// to index.html so refreshing `/admin/something/deep` still loads.
pub async fn serve_asset(Path(path): Path<String>) -> Response {
    let key = format!("assets/{}", path);
    match Asset::get(&key) {
        Some(file) => file_response(&key, file),
        None => serve_index(Uri::from_static("/admin/")).await,
    }
}

/// `/admin/*spa` — first try to serve an exact file from dist root
/// (favicon.svg, logo.svg, robots.txt, etc, anything Vite copies from
/// `public/`); fall back to index.html so React Router can resolve
/// deep links and reloads.
pub async fn serve_spa(Path(path): Path<String>) -> Response {
    // Strip leading slashes and reject path traversal.
    let key = path.trim_start_matches('/');
    if key.is_empty() || key.contains("..") {
        return serve_index(Uri::from_static("/admin/")).await;
    }
    if let Some(file) = Asset::get(key) {
        return file_response(key, file);
    }
    serve_index(Uri::from_static("/admin/")).await
}

fn file_response(path: &str, file: EmbeddedFile) -> Response {
    let mime = mime_for(path);
    let cache = if path.ends_with("index.html") {
        "no-cache"
    } else {
        // Vite output filenames carry content hashes -> immutable.
        "public, max-age=31536000, immutable"
    };
    let body = Body::from(file.data.into_owned());
    let mut res = Response::new(body);
    res.headers_mut()
        .insert(header::CONTENT_TYPE, HeaderValue::from_static(mime));
    res.headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static(cache));
    res
}

fn mime_for(path: &str) -> &'static str {
    if path.ends_with(".js") || path.ends_with(".mjs") {
        "application/javascript; charset=utf-8"
    } else if path.ends_with(".css") {
        "text/css; charset=utf-8"
    } else if path.ends_with(".html") {
        "text/html; charset=utf-8"
    } else if path.ends_with(".svg") {
        "image/svg+xml"
    } else if path.ends_with(".png") {
        "image/png"
    } else if path.ends_with(".jpg") || path.ends_with(".jpeg") {
        "image/jpeg"
    } else if path.ends_with(".webp") {
        "image/webp"
    } else if path.ends_with(".ico") {
        "image/x-icon"
    } else if path.ends_with(".json") {
        "application/json; charset=utf-8"
    } else if path.ends_with(".woff2") {
        "font/woff2"
    } else if path.ends_with(".woff") {
        "font/woff"
    } else if path.ends_with(".ttf") {
        "font/ttf"
    } else if path.ends_with(".map") {
        "application/json"
    } else {
        "application/octet-stream"
    }
}
