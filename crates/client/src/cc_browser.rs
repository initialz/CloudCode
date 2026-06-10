//! Manages a local MCP-over-stdio subprocess (M1: an echo stub;
//! M2: @playwright/mcp). Pipes opaque JSON-RPC frames (raw text) in/out.

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

/// Relative path of the M1 echo MCP stub, from a directory root.
const ECHO_STUB_REL: &str = "test-fixtures/echo-mcp.mjs";

/// Resolve the M1 echo-stub path robustly. Tries, in order:
///   (a) `test-fixtures/echo-mcp.mjs` relative to CWD,
///   (b) the same relative path under the current executable's directory
///       and each ancestor up to 4 levels above it.
/// Returns the first candidate that exists (as an absolute path where
/// possible). Returns None if no candidate exists.
fn resolve_echo_stub() -> Option<std::path::PathBuf> {
    // (a) CWD-relative.
    let cwd_cand = std::path::PathBuf::from(ECHO_STUB_REL);
    if cwd_cand.is_file() {
        return Some(std::fs::canonicalize(&cwd_cand).unwrap_or(cwd_cand));
    }
    // (b) exe dir and ancestors (target/debug -> ... -> workspace root).
    if let Ok(exe) = std::env::current_exe() {
        let mut dir = exe.parent();
        for _ in 0..=4 {
            let Some(d) = dir else { break };
            let cand = d.join(ECHO_STUB_REL);
            if cand.is_file() {
                return Some(std::fs::canonicalize(&cand).unwrap_or(cand));
            }
            dir = d.parent();
        }
    }
    None
}

/// Resolve the MCP subprocess command for M1. Defaults to the echo stub.
/// The stub path is resolved robustly (CWD first, then walking up from the
/// executable's directory). Override the whole command via `CC_BROWSER_MCP`
/// (whitespace-separated). Returns None if `node` isn't available.
///
/// If the default stub can't be found at any candidate location, the
/// CWD-relative default is returned anyway (so callers still get a command)
/// and a warning is logged pointing at `CC_BROWSER_MCP`.
#[allow(dead_code)]
pub fn m1_mcp_command() -> Option<(String, Vec<String>)> {
    if let Ok(cmd) = std::env::var("CC_BROWSER_MCP") {
        let mut parts = cmd.split_whitespace().map(|s| s.to_string());
        let prog = parts.next()?;
        return Some((prog, parts.collect()));
    }
    which_node()?;
    let stub = match resolve_echo_stub() {
        Some(p) => p.to_string_lossy().into_owned(),
        None => {
            tracing::warn!(
                path = %ECHO_STUB_REL,
                "echo MCP stub not found at resolved path; set CC_BROWSER_MCP to an absolute command"
            );
            ECHO_STUB_REL.to_string()
        }
    };
    Some(("node".to_string(), vec![stub]))
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
}
