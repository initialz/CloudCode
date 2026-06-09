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

/// Resolve the MCP subprocess command for M1. Defaults to the echo stub
/// (resolved relative to CWD, which is the workspace root during M1
/// smoke testing). Override the whole command via `CC_BROWSER_MCP`
/// (whitespace-separated). Returns None if `node` isn't available.
#[allow(dead_code)]
pub fn m1_mcp_command() -> Option<(String, Vec<String>)> {
    if let Ok(cmd) = std::env::var("CC_BROWSER_MCP") {
        let mut parts = cmd.split_whitespace().map(|s| s.to_string());
        let prog = parts.next()?;
        return Some((prog, parts.collect()));
    }
    which_node()?;
    Some(("node".to_string(), vec!["test-fixtures/echo-mcp.mjs".to_string()]))
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
