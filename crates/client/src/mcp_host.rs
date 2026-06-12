//! 通用 MCP 宿主(client 侧):拉起配置的 MCP-over-stdio 后端子进程,
//! 把不透明 JSON-RPC 帧(原文行)泵进/泵出。backend 无关:本模块不
//! 认识任何具体工具语义,只做 spawn / stdio 泵 / 握手缓存重放 / 退避
//! 重启。移植自 feature/local-browser:crates/client/src/cc_browser.rs,
//! 通用化并剥离授权门(决策 D2/D3)。

use std::sync::{Arc, Mutex};

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::mpsc;

/// claude 眼里固定的 MCP server 名(计划①唯一插槽)。与 agent 侧
/// `crates/agent/src/mcp_proxy.rs::CC_BROWSER_SERVER` 手工 lockstep。
#[allow(dead_code)] // Task 8 接线后使用
pub const CC_BROWSER_SERVER: &str = "cc-browser";

/// 把空白分隔的命令串拆成 (程序, argv)。空串/全空白 → None。
fn parse_backend(cmd: &str) -> Option<(String, Vec<String>)> {
    let mut parts = cmd.split_whitespace().map(|s| s.to_string());
    let prog = parts.next()?;
    Some((prog, parts.collect()))
}

/// 解析后端命令(计划①唯一来源,决策 D9):环境变量
/// `CC_REMOTE_MCP_BACKEND`,空白分隔,首段为程序、其余为 argv。
/// 未设置 → None(本机不提供远程-MCP 能力,Hello 能力位为 false)。
/// 计划②在此之上叠加 `[browser]` 配置段与内置默认后端。
#[allow(dead_code)] // Task 8 接线后使用
pub fn backend_command() -> Option<(String, Vec<String>)> {
    parse_backend(&std::env::var("CC_REMOTE_MCP_BACKEND").ok()?)
}

/// 已 spawn 的 MCP 子进程,说「按行分隔的 JSON-RPC over stdio」。
pub struct McpProcess {
    child: Child,
    stdin: ChildStdin,
    lines: Lines<BufReader<ChildStdout>>,
}

impl McpProcess {
    pub fn spawn(program: &str, args: &[String]) -> std::io::Result<Self> {
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

    /// 写一帧(换行分隔)进子进程 stdin。
    pub async fn feed(&mut self, payload: &str) -> std::io::Result<()> {
        self.stdin.write_all(payload.as_bytes()).await?;
        self.stdin.write_all(b"\n").await?;
        self.stdin.flush().await
    }

    /// 读下一帧;子进程 EOF → None。空行跳过。
    pub async fn next_frame(&mut self) -> Option<String> {
        loop {
            let line = self.lines.next_line().await.ok()??;
            if line.trim().is_empty() {
                continue;
            }
            return Some(line);
        }
    }

    /// 收摊:SIGKILL 直接子进程(npx 之类的包装层)并收尸。真正的后端
    /// 若是孙进程,靠本函数消费 self 掉落 stdin 收口 —— 规范 MCP server
    /// 监听 stdin 关闭后自行优雅退出(异步于本函数返回)。
    pub async fn shutdown(mut self) {
        let _ = self.child.start_kill();
        let _ = self.child.wait().await;
    }
}

/// 读 JSON-RPC 帧的 `id`(通知/非 JSON → None)。
fn json_id(frame: &str) -> Option<serde_json::Value> {
    serde_json::from_str::<serde_json::Value>(frame)
        .ok()?
        .get("id")
        .cloned()
}

/// 读 JSON-RPC 帧的 `method`(无 method/非 JSON → None)。
fn json_method(frame: &str) -> Option<String> {
    serde_json::from_str::<serde_json::Value>(frame)
        .ok()?
        .get("method")?
        .as_str()
        .map(|s| s.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 测试环境探测:PATH 上有无 node(echo 桩需要)。无则该测试 skip。
    pub(super) fn node_available() -> bool {
        let Some(path) = std::env::var_os("PATH") else { return false };
        std::env::split_paths(&path).any(|d| d.join("node").is_file())
    }

    #[test]
    fn parse_backend_splits_program_and_args() {
        assert_eq!(
            parse_backend("npx -y @playwright/mcp@0.0.76 --headless"),
            Some((
                "npx".to_string(),
                vec![
                    "-y".to_string(),
                    "@playwright/mcp@0.0.76".to_string(),
                    "--headless".to_string()
                ]
            ))
        );
        assert_eq!(parse_backend("node"), Some(("node".to_string(), vec![])));
        assert_eq!(parse_backend(""), None);
        assert_eq!(parse_backend("   "), None);
    }

    #[tokio::test]
    async fn echo_stub_roundtrips_tools_list() {
        if !node_available() {
            return; // 无 node → skip
        }
        let fixture = concat!(env!("CARGO_MANIFEST_DIR"), "/../../test-fixtures/echo-mcp.mjs");
        let mut proc =
            McpProcess::spawn("node", &[fixture.to_string()]).expect("spawn echo stub");
        proc.feed(r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#)
            .await
            .unwrap();
        let resp = proc.next_frame().await.expect("got a frame");
        assert!(resp.contains("echo"));
        proc.shutdown().await;
    }
}
