//! Manages a local MCP-over-stdio subprocess (M2: @playwright/mcp headless).
//! Pipes opaque JSON-RPC frames (raw text) in/out.

use std::sync::{Arc, Mutex};

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::{mpsc, oneshot};

/// Best-effort check whether `node` is on PATH (capability probe).
#[allow(dead_code)]
pub fn which_node() -> Option<std::path::PathBuf> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let cand = dir.join("node");
        if cand.is_file() {
            return Some(cand);
        }
    }
    None
}

/// A spawned MCP subprocess speaking newline-delimited JSON-RPC over stdio.
#[allow(dead_code)]
pub struct McpProcess {
    child: Child,
    stdin: ChildStdin,
    lines: Lines<BufReader<ChildStdout>>,
}

impl McpProcess {
    #[allow(dead_code)]
    pub fn spawn(program: &str, args: &[&str]) -> std::io::Result<Self> {
        let mut child = Command::new(program)
            .args(args)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::inherit())
            .kill_on_drop(true)
            .spawn()?;
        let stdin = child.stdin.take().expect("piped stdin");
        let stdout = child.stdout.take().expect("piped stdout");
        let lines = BufReader::new(stdout).lines();
        Ok(Self { child, stdin, lines })
    }

    /// Write one JSON-RPC frame (newline-delimited) to the subprocess.
    #[allow(dead_code)]
    pub async fn feed(&mut self, payload: &str) -> std::io::Result<()> {
        self.stdin.write_all(payload.as_bytes()).await?;
        self.stdin.write_all(b"\n").await?;
        self.stdin.flush().await
    }

    /// Read the next JSON-RPC frame from the subprocess, or None on EOF.
    #[allow(dead_code)]
    pub async fn next_frame(&mut self) -> Option<String> {
        loop {
            let line = self.lines.next_line().await.ok()??;
            if line.trim().is_empty() {
                continue;
            }
            return Some(line);
        }
    }

    /// Reap the npx wrapper and close our pipe ends. Be precise about
    /// what this does and does NOT do: `start_kill` SIGKILLs only the
    /// npx WRAPPER process — the real playwright-mcp server (and its
    /// Chrome) is a grandchild that survives the kill. What actually
    /// tears the server + browser down is this function consuming
    /// `self`, which drops `self.stdin`: playwright-mcp watches its
    /// stdin and on close runs a graceful shutdown (~0.3s typically,
    /// with a 15s force-kill fallback under load). That teardown is
    /// ASYNC relative to this function returning — `wait()` only reaps
    /// the wrapper, so the profile (`--user-data-dir`) single-instance
    /// lock may still be held briefly afterwards. Callers that respawn
    /// on the same profile must tolerate a transient "Browser is
    /// already in use" (see the warmup retry in relay.rs).
    #[allow(dead_code)]
    pub async fn shutdown(mut self) {
        let _ = self.child.start_kill();
        let _ = self.child.wait().await;
    }
}

/// Parse the `id` of a JSON-RPC frame (None for notifications / non-JSON).
fn json_id(frame: &str) -> Option<serde_json::Value> {
    serde_json::from_str::<serde_json::Value>(frame)
        .ok()?
        .get("id")
        .cloned()
}

/// Parse the `method` of a JSON-RPC frame.
fn json_method(frame: &str) -> Option<String> {
    serde_json::from_str::<serde_json::Value>(frame)
        .ok()?
        .get("method")?
        .as_str()
        .map(|s| s.to_string())
}

/// A running browser MCP channel: a pump task owns the subprocess and
/// forwards every subprocess output frame to `out_tx`. `feed` enqueues
/// an inbound frame for the subprocess.
///
/// The channel also caches the MCP handshake frames (`initialize`
/// request + `notifications/initialized`) that pass through `feed`:
/// claude never re-sends the handshake on a live connection, so after a
/// `restart` (headless<->headed switch) the new subprocess must have the
/// cached handshake replayed before it serves tools.
///
/// The cache is OWNED by the relay loop (M3 review MEDIUM-2) and merely
/// shared into each channel: a teardown (`browser = None` on deny /
/// BrowserClosed) drops the channel but not the cache, so a later
/// lazy-respawn or cold-start handoff can still replay the handshake.
#[allow(dead_code)]
pub struct BrowserChannel {
    in_tx: mpsc::Sender<String>,
    /// Cached handshake frames in arrival order (at most 2: initialize,
    /// notifications/initialized). Shared with the relay loop and
    /// across restarts.
    handshake: Arc<Mutex<Vec<String>>>,
    /// Resolves when the pump task has exited — i.e. the npx wrapper
    /// has been reaped and our pipe ends dropped. NOTE: this does NOT
    /// guarantee the real playwright-mcp server + browser have exited
    /// or released the profile lock (see `McpProcess::shutdown`).
    done_rx: oneshot::Receiver<()>,
}

impl BrowserChannel {
    #[allow(dead_code)]
    pub fn start(
        program: &str,
        args: &[&str],
        out_tx: mpsc::Sender<String>,
        handshake: Arc<Mutex<Vec<String>>>,
    ) -> std::io::Result<Self> {
        tracing::info!(program, ?args, "starting browser MCP subprocess");
        let proc = McpProcess::spawn(program, args)?;
        Ok(Self::from_process(proc, out_tx, handshake))
    }

    /// Wrap an already-spawned (and possibly already-handshaken) process
    /// in a pump task + channel.
    fn from_process(
        mut proc: McpProcess,
        out_tx: mpsc::Sender<String>,
        handshake: Arc<Mutex<Vec<String>>>,
    ) -> Self {
        let (in_tx, mut in_rx) = mpsc::channel::<String>(32);
        let (done_tx, done_rx) = oneshot::channel::<()>();
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    inbound = in_rx.recv() => {
                        let Some(frame) = inbound else { break; };
                        if proc.feed(&frame).await.is_err() { break; }
                    }
                    outbound = proc.next_frame() => {
                        match outbound {
                            Some(frame) => { if out_tx.send(frame).await.is_err() { break; } }
                            None => break, // subprocess EOF
                        }
                    }
                }
            }
            // Reaps the npx wrapper + drops our stdin pipe; the real
            // server + browser exit asynchronously via the stdin-close
            // watchdog (see McpProcess::shutdown).
            proc.shutdown().await;
            let _ = done_tx.send(());
        });
        Self { in_tx, handshake, done_rx }
    }

    /// Enqueue an inbound frame for the subprocess without blocking.
    /// Returns Err if the channel is full or the subprocess task is gone —
    /// callers should treat Err as "channel dead" and tear down.
    #[allow(dead_code)]
    pub fn feed(&self, frame: String) -> Result<(), ()> {
        self.maybe_cache_handshake(&frame);
        self.in_tx.try_send(frame).map_err(|_| ())
    }

    /// Cache the handshake frames for later replay. Cheap: once both
    /// frames are cached (len >= 2) no further parsing happens. Dedup
    /// by equality: the relay-side replay (`replay_handshake`) re-feeds
    /// cached frames through `feed`, which must not grow the cache.
    fn maybe_cache_handshake(&self, frame: &str) {
        let mut cache = self.handshake.lock().expect("handshake mutex");
        if cache.len() >= 2 || cache.iter().any(|f| f == frame) {
            return;
        }
        match json_method(frame).as_deref() {
            Some("initialize") | Some("notifications/initialized") => {
                cache.push(frame.to_string());
            }
            _ => {}
        }
    }

    /// Retire the current subprocess and start a new one with
    /// `program`/`args`, replaying the cached MCP handshake so the live
    /// claude session continues seamlessly. The replayed initialize
    /// RESPONSE is swallowed (claude already has one). Outbound frames
    /// keep flowing to `out_tx`.
    ///
    /// HONESTY NOTE (M3 review MEDIUM-1): retiring the old subprocess
    /// only reaps the npx WRAPPER; the real playwright-mcp server and
    /// its browser exit asynchronously via the server's stdin-close
    /// watchdog (~0.3s typically, up to 15s force-kill under load).
    /// `done_rx` resolving therefore does NOT guarantee the old browser
    /// has released the `--user-data-dir` profile lock when the
    /// successor spawns. That race is harmless on the lazy (headless)
    /// path — playwright-mcp launches its browser only inside the first
    /// tools/call, well after the watchdog fires — and the headed
    /// warmup call retries on "Browser is already in use" (relay.rs).
    pub async fn restart(
        self,
        program: &str,
        args: &[&str],
        out_tx: mpsc::Sender<String>,
    ) -> std::io::Result<BrowserChannel> {
        let BrowserChannel { in_tx, handshake, done_rx } = self;
        // Dropping in_tx makes the old pump's in_rx yield None -> the
        // pump reaps the npx wrapper, drops the stdin pipe (the real
        // teardown trigger — see McpProcess::shutdown), then signals
        // done_tx. Bound the wait (M3 review LOW-1): if the old pump is
        // wedged (e.g. blocked on a hung child read), proceed after 20s
        // with a warning rather than hanging the handoff forever —
        // accepting the risk that the old wrapper is still being
        // reaped while the successor spawns.
        drop(in_tx);
        if tokio::time::timeout(std::time::Duration::from_secs(20), done_rx)
            .await
            .is_err()
        {
            tracing::warn!("old browser pump did not exit within 20s; proceeding with restart");
        }

        tracing::info!(program, ?args, "restarting browser MCP subprocess");
        let mut proc = McpProcess::spawn(program, args)?;

        // Replay the cached handshake into the fresh process, swallowing
        // the replayed initialize response (matched by id) so claude
        // never sees a duplicate. Unrelated frames (e.g. server-initiated
        // notifications) are forwarded to out_tx as usual.
        let frames: Vec<String> = handshake.lock().expect("handshake mutex").clone();
        for frame in &frames {
            let init_id = if json_method(frame).as_deref() == Some("initialize") {
                json_id(frame)
            } else {
                None // notifications/initialized: no response expected
            };
            proc.feed(frame).await?;
            if let Some(want) = init_id {
                let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(60);
                for _ in 0..10 {
                    let Ok(maybe) = tokio::time::timeout_at(deadline, proc.next_frame()).await
                    else {
                        tracing::warn!("timed out waiting for replayed initialize response");
                        break;
                    };
                    let Some(resp) = maybe else { break }; // EOF
                    if json_id(&resp).as_ref() == Some(&want) {
                        break; // swallowed: claude already has its initialize response
                    }
                    let _ = out_tx.send(resp).await;
                }
            }
        }

        Ok(Self::from_process(proc, out_tx, handshake))
    }
}

/// Resolve the browser MCP subprocess command.
/// Override entirely via `CC_BROWSER_MCP` (whitespace-separated).
///
/// Default (M2): pinned `@playwright/mcp@0.0.76` via npx, headless, with a
/// dedicated persistent profile AND a predictable output dir (screenshots,
/// saved files) under the client state dir — without `--output-dir`,
/// playwright-mcp writes outputs relative to its own cwd, i.e. wherever the
/// client process happened to be started. playwright creates the directory
/// itself on first write; we create nothing here.
///
/// The version is pinned to prevent silent npx drift. Note: do NOT use the
/// unscoped third-party package `playwright-mcp` — that is a different,
/// unrelated package. The correct scope is `@playwright/mcp`.
///
/// Returns None when node/npx is unavailable.
pub fn mcp_command() -> Option<(String, Vec<String>)> {
    if let Ok(cmd) = std::env::var("CC_BROWSER_MCP") {
        let mut parts = cmd.split_whitespace().map(|s| s.to_string());
        let prog = parts.next()?;
        return Some((prog, parts.collect()));
    }
    which_node()?; // npx ships with node
    let base = dirs::data_local_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join("cloudcode");
    let profile = base.join("browser-profile");
    let output_dir = base.join("browser-output");
    Some((
        "npx".to_string(),
        vec![
            "-y".to_string(),
            "@playwright/mcp@0.0.76".to_string(), // pin: prevents npx drift; do NOT use unscoped playwright-mcp (third-party)
            "--headless".to_string(),              // default is headed; must be explicit
            "--user-data-dir".to_string(),
            profile.to_string_lossy().to_string(),
            "--output-dir".to_string(),
            output_dir.to_string_lossy().to_string(),
        ],
    ))
}

/// Remove any `--headless` token from an argv (pure, for testability).
fn strip_headless(args: Vec<String>) -> Vec<String> {
    args.into_iter().filter(|a| a != "--headless").collect()
}

/// Resolve the HEADED browser MCP subprocess command (human handoff).
/// playwright-mcp is headed by default — there is no `--headed` flag —
/// so headed = the same command minus `--headless`.
///
/// Override entirely via `CC_BROWSER_MCP_HEADED` (whitespace-separated,
/// same parsing as `CC_BROWSER_MCP`); otherwise derives from
/// `mcp_command()` by stripping `--headless` (returned as-is when the
/// token is absent). Returns None when `mcp_command()` does.
pub fn mcp_command_headed() -> Option<(String, Vec<String>)> {
    if let Ok(cmd) = std::env::var("CC_BROWSER_MCP_HEADED") {
        let mut parts = cmd.split_whitespace().map(|s| s.to_string());
        let prog = parts.next()?;
        return Some((prog, parts.collect()));
    }
    let (prog, args) = mcp_command()?;
    Some((prog, strip_headless(args)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn echo_stub_roundtrips_tools_list() {
        if which_node().is_none() {
            return; // no node -> skip
        }
        let fixture = concat!(env!("CARGO_MANIFEST_DIR"), "/../../test-fixtures/echo-mcp.mjs");
        let mut proc = McpProcess::spawn("node", &[fixture]).expect("spawn echo stub");
        proc.feed(r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#)
            .await
            .unwrap();
        let resp = proc.next_frame().await.expect("got a frame");
        assert!(resp.contains("echo"));
    }

    #[tokio::test]
    async fn channel_pumps_frames_both_ways() {
        if which_node().is_none() {
            return;
        }
        let fixture = concat!(env!("CARGO_MANIFEST_DIR"), "/../../test-fixtures/echo-mcp.mjs");
        let (out_tx, mut out_rx) = tokio::sync::mpsc::channel(8);
        let chan = BrowserChannel::start("node", &[fixture], out_tx, Arc::new(Mutex::new(Vec::new())))
            .expect("start channel");
        chan.feed(r#"{"jsonrpc":"2.0","id":7,"method":"tools/list"}"#.to_string())
            .unwrap();
        let got = out_rx.recv().await.expect("a response frame");
        assert!(got.contains("echo"));
    }

    /// Restart must (1) fully retire the old subprocess, (2) replay the
    /// cached handshake into the new one, (3) swallow the replayed
    /// initialize response so claude never sees a duplicate. The echo
    /// stub answers any request by id, exercising the replay pipeline;
    /// it ignores id-less frames, so `notifications/initialized` draws
    /// no response (matching real MCP semantics).
    #[tokio::test]
    async fn restart_replays_handshake_and_swallows_response() {
        if which_node().is_none() {
            return;
        }
        let fixture = concat!(env!("CARGO_MANIFEST_DIR"), "/../../test-fixtures/echo-mcp.mjs");
        let (out_tx, mut out_rx) = tokio::sync::mpsc::channel(8);
        let cache = Arc::new(Mutex::new(Vec::new()));
        let chan = BrowserChannel::start("node", &[fixture], out_tx.clone(), cache.clone())
            .expect("start");

        // Normal handshake flow: initialize response reaches out_rx, and
        // both handshake frames get cached by feed().
        chan.feed(
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"t","version":"0"}}}"#
                .to_string(),
        )
        .unwrap();
        let init_resp = out_rx.recv().await.expect("initialize response");
        assert!(init_resp.contains("serverInfo"));
        chan.feed(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#.to_string())
            .unwrap();
        assert_eq!(chan.handshake.lock().unwrap().len(), 2);

        // Restart onto the same stub: handshake is replayed, and the
        // replayed initialize response (id 1) must be swallowed.
        let chan = chan
            .restart("node", &[fixture], out_tx.clone())
            .await
            .expect("restart");

        chan.feed(r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#.to_string())
            .unwrap();
        let next = tokio::time::timeout(std::time::Duration::from_secs(10), out_rx.recv())
            .await
            .expect("a frame within 10s")
            .expect("channel alive");
        // The very next frame after restart must be the tools/list
        // response — NOT a duplicate initialize response.
        assert!(next.contains(r#""id":2"#), "expected tools/list response, got: {next}");
        assert!(!next.contains("serverInfo"), "duplicate initialize response leaked: {next}");
        assert!(next.contains("echo"));
        // Nothing else queued in between.
        assert!(out_rx.try_recv().is_err());
    }

    /// The handshake cache is owned OUTSIDE the channel (M3 MEDIUM-2):
    /// it must survive the channel being dropped (deny / BrowserClosed
    /// teardown), and re-feeding a cached frame (the relay-side replay
    /// path) must not duplicate entries.
    #[tokio::test]
    async fn shared_cache_survives_channel_drop_and_dedups() {
        if which_node().is_none() {
            return;
        }
        let fixture = concat!(env!("CARGO_MANIFEST_DIR"), "/../../test-fixtures/echo-mcp.mjs");
        let (out_tx, mut out_rx) = tokio::sync::mpsc::channel(8);
        let cache = Arc::new(Mutex::new(Vec::new()));
        let chan = BrowserChannel::start("node", &[fixture], out_tx, cache.clone())
            .expect("start channel");
        let init = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#;
        chan.feed(init.to_string()).unwrap();
        let _ = out_rx.recv().await.expect("initialize response");
        assert_eq!(cache.lock().unwrap().len(), 1);
        // Replay path re-feeds the identical frame: cache must not grow.
        chan.feed(init.to_string()).unwrap();
        assert_eq!(cache.lock().unwrap().len(), 1);
        // Cache outlives the channel.
        drop(chan);
        assert_eq!(cache.lock().unwrap().len(), 1);
    }

    #[test]
    fn strip_headless_removes_token() {
        let args = vec![
            "-y".to_string(),
            "@playwright/mcp@0.0.76".to_string(),
            "--headless".to_string(),
            "--user-data-dir".to_string(),
            "/tmp/p".to_string(),
        ];
        let stripped = strip_headless(args);
        assert!(!stripped.iter().any(|a| a == "--headless"));
        assert_eq!(stripped.len(), 4);
    }

    #[test]
    fn strip_headless_noop_when_absent() {
        let args = vec!["node".to_string(), "stub.mjs".to_string()];
        assert_eq!(strip_headless(args.clone()), args);
    }

    /// Real playwright smoke test — spawns `@playwright/mcp@0.0.76` via npx
    /// and sends an MCP initialize request. Marked `#[ignore]` so it does not
    /// run in the normal suite; the npx cold-start can take well over 25s on
    /// a fresh machine. Run manually:
    ///   cargo test -p cloudcode-client -- --ignored playwright_mcp_initialize_roundtrips
    #[tokio::test]
    #[ignore = "spawns real @playwright/mcp via npx; run manually"]
    async fn playwright_mcp_initialize_roundtrips() {
        let Some((prog, args)) = mcp_command() else { return };
        let argrefs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
        let mut proc = McpProcess::spawn(&prog, &argrefs).expect("spawn playwright mcp");
        proc.feed(r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"test","version":"0"}}}"#)
            .await
            .unwrap();
        let resp = tokio::time::timeout(
            std::time::Duration::from_secs(120),
            proc.next_frame(),
        )
        .await
        .expect("npx cold start within 2min")
        .expect("a frame");
        assert!(resp.contains("Playwright"));
        proc.shutdown().await;
    }
}
