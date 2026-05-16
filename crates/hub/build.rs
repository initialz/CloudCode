//! Ensure SPA `dist/` folders exist before `rust-embed` reads them.
//! Both `admin-ui/dist/` and `webterm/dist/` are gitignored — first-time
//! `cargo build` would otherwise fail because the embed macro needs
//! *something* to scan. We drop tiny placeholder index.htmls so the
//! build succeeds; when the operator runs the real frontend build the
//! output overwrites this placeholder and the next `cargo build` picks
//! up the proper bundle.

use std::env;
use std::path::{Path, PathBuf};

fn main() {
    let manifest_dir = env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR");
    let manifest_dir = PathBuf::from(&manifest_dir);

    ensure_placeholder(
        &manifest_dir.join("../../admin-ui/dist"),
        "cloudcode admin",
        "admin UI",
        "cd admin-ui &amp;&amp; npm install &amp;&amp; npm run build",
        "/admin/api/*",
    );
    ensure_placeholder(
        &manifest_dir.join("../../webterm/dist"),
        "cloudcode webterm",
        "user-facing web terminal",
        "cd webterm &amp;&amp; pnpm install &amp;&amp; pnpm build",
        "/app/api/*",
    );

    println!("cargo:rerun-if-changed=../../admin-ui/dist/index.html");
    println!("cargo:rerun-if-changed=../../webterm/dist/index.html");
}

fn ensure_placeholder(dist: &Path, title: &str, label: &str, build_cmd: &str, api_path: &str) {
    let index = dist.join("index.html");
    if index.exists() {
        return;
    }
    std::fs::create_dir_all(dist.join("assets")).expect("create dist/assets");
    let html = format!(
        r#"<!doctype html>
<html lang="en"><head><meta charset="utf-8">
<title>{title}</title>
<style>body{{font-family:system-ui,sans-serif;max-width:40rem;margin:4rem auto;padding:0 1rem;color-scheme:light dark;}}</style>
</head><body>
<h1>{title}</h1>
<p>The {label} bundle hasn't been built yet. From the repo root run:</p>
<pre>{build_cmd}</pre>
<p>Then rebuild the hub. The JSON API at <code>{api_path}</code> works regardless.</p>
</body></html>"#
    );
    std::fs::write(&index, html).expect("write placeholder index.html");
}
