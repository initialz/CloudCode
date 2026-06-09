//! Manages a local MCP-over-stdio subprocess (M1: an echo stub;
//! M2: @playwright/mcp). Pipes opaque JSON-RPC frames (raw text) in/out.

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};

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
}
