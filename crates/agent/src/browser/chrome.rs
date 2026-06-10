//! Resident headless Chrome manager (P1 Task 3).
//!
//! We run ONE long-lived Chrome with `--headless=new --remote-debugging-port`
//! and a dedicated `--user-data-dir`, so the CDP endpoint is always there for
//! playwright-mcp to attach to lazily (it only fails on the first tool call if
//! the port is dead — see the p1-spike notes — so OUR side must keep Chrome
//! healthy and expose readiness).
//!
//! Chrome and the mcp process have independent lifecycles: either can restart
//! without the other. This module owns Chrome only; the mcp subprocess lives in
//! `subprocess.rs` and is pumped by the endpoint (Task 4).
//!
//! Ownership mirrors `PtyManager`: a shared `Arc<ChromeManager>` lives on the
//! app state. Internal mutable bits (the running child, the readiness flag) sit
//! behind `Mutex`/atomic so the manager can be cloned-by-Arc and the
//! supervision task can drive restarts without re-borrowing the manager.

// The public surface here is consumed by the browser endpoint (Task 4), which
// isn't wired yet. Mirror `subprocess.rs`'s `#[allow(dead_code)]` so the
// not-yet-called API doesn't trip the workspace's zero-warning bar.
#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Result};
use tokio::process::{Child, Command};
use tokio::sync::Mutex;

use crate::config::BrowserConfig;

/// Max time we'll wait for the CDP endpoint to answer `/json/version` after a
/// fresh spawn before giving up.
const READINESS_TIMEOUT: Duration = Duration::from_secs(15);
/// How often we re-probe the CDP endpoint while waiting for readiness.
const READINESS_INTERVAL: Duration = Duration::from_millis(200);
/// Supervision backoff floor and ceiling.
const BACKOFF_MIN: Duration = Duration::from_millis(500);
const BACKOFF_MAX: Duration = Duration::from_secs(8);

/// Long-lived headless Chrome supervisor.
pub struct ChromeManager {
    cfg: BrowserConfig,
    /// `--user-data-dir`; `<agent state dir>/browser-profile` by default.
    profile_dir: PathBuf,
    /// The currently-running Chrome child. `None` between exit and respawn.
    /// Held so `start`/supervision can replace it and so drop kills Chrome
    /// (the child is spawned `kill_on_drop(true)`).
    child: Mutex<Option<Child>>,
    /// True once the CDP endpoint has answered `/json/version` at least once
    /// since the last (re)spawn. The endpoint (Task 4) reads this to decide
    /// whether to wait before handing the port to playwright-mcp.
    ready: AtomicBool,
}

impl ChromeManager {
    /// Build a manager. Does not spawn Chrome — call [`ChromeManager::start`].
    /// `state_dir` is the agent state dir (`<state>/agent`); the Chrome profile
    /// lives under it at `browser-profile`.
    pub fn new(cfg: BrowserConfig, state_dir: &Path) -> Self {
        let profile_dir = state_dir.join("browser-profile");
        Self {
            cfg,
            profile_dir,
            child: Mutex::new(None),
            ready: AtomicBool::new(false),
        }
    }

    /// Whether the CDP endpoint has been confirmed reachable since the last
    /// (re)spawn. Consumed by Task 4 to gate handing the port to mcp.
    pub fn is_ready(&self) -> bool {
        self.ready.load(Ordering::Acquire)
    }

    /// Base CDP HTTP URL, e.g. `http://127.0.0.1:19222`.
    pub fn cdp_http_url(&self) -> String {
        format!("http://127.0.0.1:{}", self.cfg.cdp_port)
    }

    /// Resolve the Chrome/Chromium binary to launch.
    ///
    /// Pure and testable: explicit `chrome_path` wins if it's set AND exists on
    /// disk; otherwise we probe well-known candidates in order and return the
    /// first that exists. Returns `None` if nothing is found — the caller turns
    /// that into a clear error.
    pub fn resolve_chrome_path(cfg: &BrowserConfig) -> Option<PathBuf> {
        if let Some(explicit) = cfg.chrome_path.as_ref() {
            let p = PathBuf::from(explicit);
            if p.exists() {
                return Some(p);
            }
            // Explicit-but-missing: fall through to auto-detect rather than
            // hard-fail, so a stale config path doesn't wedge the browser.
        }

        // Absolute macOS app bundles — check existence directly.
        let macos_candidates = [
            "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
            "/Applications/Chromium.app/Contents/MacOS/Chromium",
        ];
        for cand in macos_candidates {
            let p = PathBuf::from(cand);
            if p.exists() {
                return Some(p);
            }
        }

        // Linux / PATH-based names. Walk $PATH like the M1 which_node helper.
        for name in ["google-chrome", "chromium", "chromium-browser"] {
            if let Some(p) = which_on_path(name) {
                return Some(p);
            }
        }

        None
    }

    /// Build the Chrome argv. Pure and testable.
    pub fn build_args(cdp_port: u16, profile_dir: &Path) -> Vec<String> {
        vec![
            "--headless=new".to_string(),
            format!("--remote-debugging-port={cdp_port}"),
            format!("--user-data-dir={}", profile_dir.display()),
            "--no-first-run".to_string(),
            "--no-default-browser-check".to_string(),
            "about:blank".to_string(),
        ]
    }

    /// Spawn Chrome, wait for the CDP endpoint to become ready, then start a
    /// background supervision task that restarts Chrome on exit (with
    /// exponential backoff) and re-probes readiness.
    ///
    /// Returns once Chrome is confirmed ready, or `Err` on spawn failure /
    /// readiness timeout. The supervision task keeps running for the lifetime
    /// of the `Arc<ChromeManager>`.
    pub async fn start(self: &Arc<Self>) -> Result<()> {
        let chrome = Self::resolve_chrome_path(&self.cfg).ok_or_else(|| {
            anyhow!(
                "no Chrome/Chromium binary found; set [browser].chrome_path in agent.toml \
                 (looked for /Applications/Google Chrome.app, Chromium, and \
                  google-chrome/chromium/chromium-browser on PATH)"
            )
        })?;

        std::fs::create_dir_all(&self.profile_dir).map_err(|e| {
            anyhow!(
                "creating Chrome profile dir {}: {e}",
                self.profile_dir.display()
            )
        })?;

        // Spawn + probe once up front so `start` reflects the initial result.
        self.spawn_chrome(&chrome).await?;
        let url = format!("{}/json/version", self.cdp_http_url());
        probe_ready(&url, READINESS_TIMEOUT, READINESS_INTERVAL).await?;
        self.ready.store(true, Ordering::Release);
        tracing::info!(cdp = %self.cdp_http_url(), chrome = %chrome.display(), "Chrome ready");

        // Hand off to the supervision loop.
        let me = Arc::clone(self);
        tokio::spawn(async move {
            me.supervise(chrome).await;
        });

        Ok(())
    }

    /// Spawn Chrome and store the child handle, replacing any previous one.
    async fn spawn_chrome(&self, chrome: &Path) -> Result<()> {
        let args = Self::build_args(self.cfg.cdp_port, &self.profile_dir);
        let child = Command::new(chrome)
            .args(&args)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| anyhow!("spawning Chrome at {}: {e}", chrome.display()))?;
        tracing::info!(pid = child.id(), "spawned headless Chrome");
        *self.child.lock().await = Some(child);
        Ok(())
    }

    /// Supervision loop: wait for the current child to exit, then respawn with
    /// exponential backoff and re-probe readiness. Runs until the manager (and
    /// thus this task's `Arc`) is dropped.
    async fn supervise(self: Arc<Self>, chrome: PathBuf) {
        let mut backoff = BACKOFF_MIN;
        loop {
            // Wait for the current child to exit. We take() the child out of
            // the slot so other code that holds the lock briefly isn't blocked
            // for the whole lifetime of Chrome.
            match self.child.lock().await.take() {
                Some(mut child) => {
                    let status = child.wait().await;
                    self.ready.store(false, Ordering::Release);
                    match status {
                        Ok(s) => tracing::warn!(status = ?s, "Chrome exited; restarting"),
                        Err(e) => {
                            tracing::warn!(error = %e, "waiting on Chrome failed; restarting")
                        }
                    }
                }
                None => {
                    // No child (e.g. a spawn failed mid-restart). Nothing to
                    // wait on; fall straight through to a backoff+respawn.
                    self.ready.store(false, Ordering::Release);
                    tracing::warn!("Chrome supervision: no child to wait on; will respawn");
                }
            }

            self.backoff_then_respawn(&chrome, &mut backoff).await;
        }
    }

    /// Sleep `*backoff`, respawn Chrome, re-probe readiness, and update the
    /// backoff for the next round (reset to MIN on success, double up to MAX on
    /// failure).
    async fn backoff_then_respawn(&self, chrome: &Path, backoff: &mut Duration) {
        tracing::info!(backoff_ms = backoff.as_millis() as u64, "Chrome restart backoff");
        tokio::time::sleep(*backoff).await;

        match self.spawn_chrome(chrome).await {
            Ok(()) => {
                let url = format!("{}/json/version", self.cdp_http_url());
                match probe_ready(&url, READINESS_TIMEOUT, READINESS_INTERVAL).await {
                    Ok(()) => {
                        self.ready.store(true, Ordering::Release);
                        *backoff = BACKOFF_MIN;
                        tracing::info!(cdp = %self.cdp_http_url(), "Chrome restarted and ready");
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "Chrome restarted but readiness probe failed");
                        *backoff = (*backoff * 2).min(BACKOFF_MAX);
                    }
                }
            }
            Err(e) => {
                tracing::warn!(error = %e, "Chrome respawn failed");
                *backoff = (*backoff * 2).min(BACKOFF_MAX);
            }
        }
    }
}

/// Walk `$PATH` looking for an executable named `name`, returning the first
/// match. Mirrors the M1 `which_node` helper (kept local so it's trivially
/// testable and has no extra deps).
fn which_on_path(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let cand = dir.join(name);
        if cand.is_file() {
            return Some(cand);
        }
    }
    None
}

/// Poll `GET <url>` until it answers 2xx or `timeout` elapses, sleeping
/// `interval` between attempts. Factored out (with injectable timeout/interval)
/// so tests can drive it with a short budget against a stub listener.
pub async fn probe_ready(url: &str, timeout: Duration, interval: Duration) -> Result<()> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(2))
        .build()
        .map_err(|e| anyhow!("building probe client: {e}"))?;

    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if let Ok(resp) = client.get(url).send().await {
            if resp.status().is_success() {
                return Ok(());
            }
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(anyhow!(
                "CDP endpoint {url} did not become ready within {timeout:?}"
            ));
        }
        tokio::time::sleep(interval).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write as _;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    fn cfg_with(chrome_path: Option<String>, port: u16) -> BrowserConfig {
        BrowserConfig {
            enabled: true,
            chrome_path,
            cdp_port: port,
            mcp_command: None,
        }
    }

    #[test]
    fn resolve_explicit_path_that_exists_wins() {
        // A real temp file stands in for the Chrome binary.
        let mut f = tempfile::NamedTempFile::new().unwrap();
        f.write_all(b"#!/bin/sh\n").unwrap();
        let path = f.path().to_string_lossy().to_string();
        let cfg = cfg_with(Some(path.clone()), 19222);
        let resolved = ChromeManager::resolve_chrome_path(&cfg).unwrap();
        assert_eq!(resolved, PathBuf::from(path));
    }

    #[test]
    fn resolve_explicit_missing_falls_through() {
        // A path that definitely doesn't exist must NOT be returned as-is;
        // resolution falls through to auto-detect. We can't assert what
        // auto-detect finds in CI, but we CAN assert the bogus path is never
        // returned.
        let bogus = "/nonexistent/definitely/not/here/chrome".to_string();
        let cfg = cfg_with(Some(bogus.clone()), 19222);
        let resolved = ChromeManager::resolve_chrome_path(&cfg);
        assert_ne!(resolved, Some(PathBuf::from(bogus)));
    }

    #[test]
    fn build_args_exact() {
        let dir = PathBuf::from("/tmp/profile");
        let args = ChromeManager::build_args(12345, &dir);
        assert_eq!(
            args,
            vec![
                "--headless=new".to_string(),
                "--remote-debugging-port=12345".to_string(),
                "--user-data-dir=/tmp/profile".to_string(),
                "--no-first-run".to_string(),
                "--no-default-browser-check".to_string(),
                "about:blank".to_string(),
            ]
        );
    }

    #[test]
    fn cdp_http_url_format() {
        let cfg = cfg_with(None, 4567);
        let mgr = ChromeManager::new(cfg, Path::new("/tmp/state/agent"));
        assert_eq!(mgr.cdp_http_url(), "http://127.0.0.1:4567");
        assert_eq!(
            mgr.profile_dir,
            PathBuf::from("/tmp/state/agent/browser-profile")
        );
    }

    #[tokio::test]
    async fn probe_ready_succeeds_against_live_listener() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        // Serve as many connections as the probe makes (it may retry).
        tokio::spawn(async move {
            loop {
                serve_one_ok(&listener).await;
            }
        });
        let url = format!("http://{addr}/json/version");
        let r = probe_ready(&url, Duration::from_millis(2000), Duration::from_millis(100)).await;
        assert!(r.is_ok(), "probe should succeed: {r:?}");
    }

    /// Minimal HTTP/1.1 stub: accept one connection, read the request, reply
    /// 200 with a tiny JSON body shaped like `/json/version`.
    async fn serve_one_ok(listener: &TcpListener) {
        if let Ok((mut sock, _)) = listener.accept().await {
            let mut buf = [0u8; 1024];
            let _ = sock.read(&mut buf).await;
            let body = r#"{"webSocketDebuggerUrl":"ws://127.0.0.1/devtools/x"}"#;
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            let _ = sock.write_all(resp.as_bytes()).await;
            let _ = sock.flush().await;
        }
    }

    #[tokio::test]
    async fn probe_ready_times_out_on_dead_port() {
        // Bind then immediately drop the listener so the port has nothing
        // listening (a connection refused). Short timeout keeps the test fast.
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);
        let url = format!("http://{addr}/json/version");
        let start = std::time::Instant::now();
        let r = probe_ready(&url, Duration::from_millis(500), Duration::from_millis(100)).await;
        assert!(r.is_err(), "probe against dead port should time out");
        assert!(
            start.elapsed() < Duration::from_secs(3),
            "should fail fast, took {:?}",
            start.elapsed()
        );
    }

    /// Real-Chrome integration test. Run manually:
    /// `cargo test -p cloudcode-agent -- --ignored real_chrome`.
    /// Resolves a real Chrome, starts it, asserts /json/version is reachable,
    /// then drops the manager and asserts Chrome dies (kill_on_drop).
    #[tokio::test]
    #[ignore = "requires a real Chrome install; run manually"]
    async fn real_chrome_lifecycle() {
        let cfg = cfg_with(None, 19233);
        let tmp = tempfile::tempdir().unwrap();
        let mgr = Arc::new(ChromeManager::new(cfg, tmp.path()));
        mgr.start().await.expect("Chrome should start and become ready");
        assert!(mgr.is_ready());

        let url = format!("{}/json/version", mgr.cdp_http_url());
        let client = reqwest::Client::new();
        let resp = client.get(&url).send().await.expect("CDP reachable");
        assert!(resp.status().is_success());

        // Drop the manager -> child dropped with kill_on_drop -> Chrome dies.
        // Pull the child out to drop it deterministically.
        {
            let mut guard = mgr.child.lock().await;
            let _ = guard.take(); // dropping the Child SIGKILLs Chrome
        }
        // Give the OS a moment, then the port should stop answering.
        tokio::time::sleep(Duration::from_millis(800)).await;
        let dead = client.get(&url).send().await;
        assert!(dead.is_err() || !dead.unwrap().status().is_success());
    }
}
