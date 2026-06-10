//! Manages a local MCP-over-stdio subprocess (M2: @playwright/mcp headless).
//! Pipes opaque JSON-RPC frames (raw text) in/out.

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::mpsc;

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

    #[allow(dead_code)]
    pub async fn shutdown(mut self) {
        let _ = self.child.start_kill();
    }
}

/// A running browser MCP channel: owns the subprocess + a task that
/// forwards every subprocess output frame to `out_tx`. `feed` enqueues
/// an inbound frame for the subprocess.
#[allow(dead_code)]
pub struct BrowserChannel {
    in_tx: mpsc::Sender<String>,
}

impl BrowserChannel {
    #[allow(dead_code)]
    pub fn start(
        program: &str,
        args: &[&str],
        out_tx: mpsc::Sender<String>,
    ) -> std::io::Result<Self> {
        tracing::info!(program, ?args, "starting browser MCP subprocess");
        let mut proc = McpProcess::spawn(program, args)?;
        let (in_tx, mut in_rx) = mpsc::channel::<String>(32);
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
            proc.shutdown().await;
        });
        Ok(Self { in_tx })
    }

    /// Enqueue an inbound frame for the subprocess without blocking.
    /// Returns Err if the channel is full or the subprocess task is gone —
    /// callers should treat Err as "channel dead" and tear down.
    #[allow(dead_code)]
    pub fn feed(&self, frame: String) -> Result<(), ()> {
        self.in_tx.try_send(frame).map_err(|_| ())
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
        let chan = BrowserChannel::start("node", &[fixture], out_tx).expect("start channel");
        chan.feed(r#"{"jsonrpc":"2.0","id":7,"method":"tools/list"}"#.to_string())
            .unwrap();
        let got = out_rx.recv().await.expect("a response frame");
        assert!(got.contains("echo"));
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
