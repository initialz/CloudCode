//! Ensure `admin-ui/dist/` exists before `rust-embed` reads it. The
//! dist dir is gitignored — first-time `cargo build` would otherwise
//! fail because the embed macro needs *something* to scan. We drop a
//! tiny placeholder index.html so the build succeeds; when the
//! operator runs `cd admin-ui && npm install && npm run build` the real
//! Vite output overwrites this placeholder and the next `cargo build`
//! picks up the proper bundle.

use std::env;
use std::path::PathBuf;

fn main() {
    let manifest_dir = env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR");
    let dist: PathBuf = PathBuf::from(&manifest_dir).join("../../admin-ui/dist");
    let index = dist.join("index.html");

    if !index.exists() {
        std::fs::create_dir_all(dist.join("assets")).expect("create dist/assets");
        std::fs::write(
            &index,
            r#"<!doctype html>
<html lang="en"><head><meta charset="utf-8">
<title>cloudcode admin</title>
<style>body{font-family:system-ui,sans-serif;max-width:40rem;margin:4rem auto;padding:0 1rem;color-scheme:light dark;}</style>
</head><body>
<h1>cloudcode admin</h1>
<p>The admin UI bundle hasn't been built yet. From the repo root run:</p>
<pre>cd admin-ui &amp;&amp; npm install &amp;&amp; npm run build</pre>
<p>Then rebuild the hub. The JSON API at <code>/admin/api/*</code> works regardless.</p>
</body></html>"#,
        )
        .expect("write placeholder index.html");
    }

    println!("cargo:rerun-if-changed=../../admin-ui/dist/index.html");
}
