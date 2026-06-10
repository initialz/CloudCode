//! Manages a local MCP-over-stdio subprocess (P1: echo stub / future @playwright/mcp headless).
//! Pipes opaque JSON-RPC frames (raw text) in/out.
//! The pump loop lives in the endpoint (Task 4); this module is just the process pipe.

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};

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
        Self::spawn_with_cwd(program, args, None)
    }

    /// Spawn like [`McpProcess::spawn`] but optionally set the child's working
    /// directory. The endpoint (Task 4) uses this to point playwright-mcp's
    /// `.playwright-mcp/` scratch output at a dedicated dir instead of cwd.
    #[allow(dead_code)]
    pub fn spawn_with_cwd(
        program: &str,
        args: &[&str],
        cwd: Option<&std::path::Path>,
    ) -> std::io::Result<Self> {
        let mut cmd = Command::new(program);
        cmd.args(args)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::inherit())
            .kill_on_drop(true);
        if let Some(dir) = cwd {
            cmd.current_dir(dir);
        }
        let mut child = cmd.spawn()?;
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Best-effort check whether `node` is on PATH (capability probe).
    fn which_node() -> Option<std::path::PathBuf> {
        let path = std::env::var_os("PATH")?;
        for dir in std::env::split_paths(&path) {
            let cand = dir.join("node");
            if cand.is_file() {
                return Some(cand);
            }
        }
        None
    }

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
