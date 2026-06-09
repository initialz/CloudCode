# 云端 claude 操作本地浏览器 — M1 透明管道 实现计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 打通"claude(agent 侧)→ hub → client → 本地 MCP 子进程 → 原路返回"的透明 MCP JSON-RPC 反向通道,用一个**回显桩**子进程验证全链路,并实现能力协商(无 browser-capable client 时浏览器工具不出现)。

**Architecture:** 在 agent 守护进程内新建一个常驻 localhost HTTP/SSE MCP 端点供 claude 连接;它把 MCP 帧打成 `ClientMsg::BrowserRpc{session_id, payload}` 发给 hub;hub 按 `session_id` 路由(复用 PTY 的 `sessions` 通道机制)到对应 client 的 `HubToClient::BrowserRpc{payload}`;client 的新 `cc_browser` 模块把帧喂给一个 Node 子进程(M1 用回显桩,M2 换 `@playwright/mcp`),输出经 `ClientToHub::BrowserRpc` 原路返回。所有中间层只搬运不解析 `payload`(`Box<RawValue>`)。

**Tech Stack:** Rust(tokio、axum 0.7、serde、serde_json `RawValue`、uuid、tokio-tungstenite)、Node(回显桩脚本)。

**关键设计校正(相对 spec):** 路由键用 hub 现有的 `session_id: Uuid`(不是 `workspace`)。agent 侧帧(`ClientMsg`/`ServerMsg::BrowserRpc`)携带 `session_id`;client↔hub 侧(`ClientToHub`/`HubToClient::BrowserRpc`)session 隐式存于 `ConnCtx.active`,与现有 PTY 输入帧同构。

**方向回顾:** 请求 = claude(agent)→endpoint→`ClientMsg::BrowserRpc`→hub→`HubToClient::BrowserRpc`→client→子进程。响应 = 子进程→client→`ClientToHub::BrowserRpc`→hub→`ServerMsg::BrowserRpc`→endpoint→claude。

**M1 不含**(留 M2/M3):真 `@playwright/mcp`、授权门、handoff、headless⇄headed、install.sh 预装。M1 的桩无条件放行、无授权门。

---

## File Structure

| 文件 | 改动 | 职责 |
|------|------|------|
| `crates/hub/src/pty_proto.rs` | 改 | 加 `ClientToHub::BrowserRpc` / `HubToClient::BrowserRpc`;`Hello` 加 `browser_capable` |
| `crates/client/src/proto.rs` | 改 | 镜像上面三处(锁步) |
| `crates/agent/src/tunnel.rs` | 改 | 加 `ClientMsg::BrowserRpc{session_id,payload}` / `ServerMsg::BrowserRpc{session_id,payload}` |
| `crates/hub/src/registry.rs` | 改 | `classify()` 把 `ClientMsg::BrowserRpc` 路由为 `Routing::Session` |
| `crates/hub/src/pty_session.rs` | 改 | `handle_client_frame` 处理 `ClientToHub::BrowserRpc`→`ServerMsg::BrowserRpc`;`handle_agent_event` 处理 `ClientMsg::BrowserRpc`→`HubToClient::BrowserRpc`;`Hello` 透传 `browser_capable` 到 agent |
| `crates/agent/src/mcp_endpoint.rs` | 新建 | 常驻 localhost HTTP/SSE MCP 端点 + per-session token 路由 |
| `crates/agent/src/main.rs` | 改 | `AppState` 加端点共享状态;`serve()` spawn 端点监听 |
| `crates/agent/src/ws.rs` | 改 | `read_loop` 分发 `ServerMsg::BrowserRpc` 到端点;端点出帧作 `ClientMsg::BrowserRpc` 发回 |
| `crates/agent/src/pty.rs` | 改 | `open_session` 给 claude 注入 `--mcp-config`(指向端点 + session token) |
| `crates/client/src/cc_browser.rs` | 新建 | 管理 MCP 子进程,帧↔stdio 双向管道;能力探测 |
| `crates/client/src/relay.rs` | 改 | 分发 `HubToClient::BrowserRpc` 到 `cc_browser` |
| `crates/client/src/main.rs` | 改 | 启动时探测 browser 能力,`Hello` 带 `browser_capable` |
| `test-fixtures/echo-mcp.mjs` | 新建 | M1 回显桩:读 stdin 的 JSON-RPC,对 `initialize`/`tools/list`/`tools/call` 回最小合法响应 |

---

## Task 1: proto 帧 — hub 侧(pty_proto.rs)

**Files:**
- Modify: `crates/hub/src/pty_proto.rs`
- Test: `crates/hub/src/pty_proto.rs`(同文件 `#[cfg(test)]`)

- [ ] **Step 1: 写失败测试**

在 `crates/hub/src/pty_proto.rs` 末尾添加:

```rust
#[cfg(test)]
mod browser_tests {
    use super::*;

    #[test]
    fn browser_rpc_client_to_hub_roundtrips() {
        let raw = serde_json::value::RawValue::from_string(
            r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#.to_string(),
        )
        .unwrap();
        let frame = ClientToHub::BrowserRpc { payload: raw };
        let json = serde_json::to_string(&frame).unwrap();
        assert!(json.contains("\"type\":\"browser_rpc\""));
        assert!(json.contains("tools/list"));
        let back: ClientToHub = serde_json::from_str(&json).unwrap();
        matches!(back, ClientToHub::BrowserRpc { .. });
    }

    #[test]
    fn browser_rpc_hub_to_client_roundtrips() {
        let raw = serde_json::value::RawValue::from_string(r#"{"ok":true}"#.to_string()).unwrap();
        let frame = HubToClient::BrowserRpc { payload: raw };
        let json = serde_json::to_string(&frame).unwrap();
        let back: HubToClient = serde_json::from_str(&json).unwrap();
        matches!(back, HubToClient::BrowserRpc { .. });
    }
}
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo test -p cloudcode-hub browser_rpc -- --nocapture`
Expected: 编译失败 —— `no variant named BrowserRpc`。

- [ ] **Step 3: 加变体**

在 `pty_proto.rs` 顶部 import 区(`use uuid::Uuid;` 旁)加:

```rust
use serde_json::value::RawValue;
```

在 `enum ClientToHub` 中(`Close,` 之前)加:

```rust
    /// In-session: one opaque MCP JSON-RPC frame from the client's
    /// browser MCP subprocess back toward claude. Hub forwards it to
    /// the bound agent as `ServerMsg::BrowserRpc` tagged with the
    /// active session_id. Payload is never parsed in transit.
    BrowserRpc {
        payload: Box<RawValue>,
    },
    /// In-session: client tearing down its browser channel.
    BrowserClosed {
        #[serde(default)]
        reason: Option<String>,
    },
```

在 `enum HubToClient` 中(`Ping,` 之前)加:

```rust
    /// One opaque MCP JSON-RPC frame from claude (via the agent) toward
    /// the client's browser MCP subprocess. Payload is never parsed.
    BrowserRpc {
        payload: Box<RawValue>,
    },
    /// Hub/agent tore down the browser channel (denied / disconnect /
    /// task ended). Client should stop its MCP subprocess.
    BrowserClosed {
        #[serde(default)]
        reason: Option<String>,
    },
```

- [ ] **Step 4: 跑测试确认通过**

Run: `cargo test -p cloudcode-hub browser_rpc`
Expected: PASS(2 个测试)。

- [ ] **Step 5: 提交**

```bash
git add crates/hub/src/pty_proto.rs
git commit -m "feat(proto): add BrowserRpc/BrowserClosed to hub client protocol"
```

---

## Task 2: proto 帧 — client 侧镜像(proto.rs)

**Files:**
- Modify: `crates/client/src/proto.rs`
- Test: `crates/client/src/proto.rs`

- [ ] **Step 1: 写失败测试**

在 `crates/client/src/proto.rs` 末尾添加(与 Task 1 同形,验证镜像锁步):

```rust
#[cfg(test)]
mod browser_tests {
    use super::*;

    #[test]
    fn browser_rpc_roundtrips_both_directions() {
        let raw = serde_json::value::RawValue::from_string(r#"{"id":1}"#.to_string()).unwrap();
        let c = ClientToHub::BrowserRpc { payload: raw };
        let j = serde_json::to_string(&c).unwrap();
        assert!(j.contains("\"type\":\"browser_rpc\""));
        let _back: ClientToHub = serde_json::from_str(&j).unwrap();

        let raw2 = serde_json::value::RawValue::from_string(r#"{"id":2}"#.to_string()).unwrap();
        let h = HubToClient::BrowserRpc { payload: raw2 };
        let j2 = serde_json::to_string(&h).unwrap();
        let _back2: HubToClient = serde_json::from_str(&j2).unwrap();
    }
}
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo test -p cloudcode-client browser_rpc`
Expected: 编译失败 —— `no variant named BrowserRpc`。

- [ ] **Step 3: 加变体(与 hub 完全一致)**

在 `proto.rs` import 区加 `use serde_json::value::RawValue;`。在 `ClientToHub`(`Close,` 前)和 `HubToClient`(`Ping,` 前)加入与 Task 1 Step 3 **逐字相同**的 `BrowserRpc` + `BrowserClosed` 变体及其文档注释。

- [ ] **Step 4: 跑测试确认通过**

Run: `cargo test -p cloudcode-client browser_rpc`
Expected: PASS。

- [ ] **Step 5: 提交**

```bash
git add crates/client/src/proto.rs
git commit -m "feat(proto): mirror BrowserRpc/BrowserClosed in client protocol"
```

---

## Task 3: proto 帧 — agent↔hub(tunnel.rs)

**Files:**
- Modify: `crates/agent/src/tunnel.rs`
- Test: `crates/agent/src/tunnel.rs`

agent 侧帧携带 `session_id`(路由键)。`ClientMsg::BrowserRpc` = agent→hub(请求出境);`ServerMsg::BrowserRpc` = hub→agent(响应入境)。

- [ ] **Step 1: 写失败测试**

在 `crates/agent/src/tunnel.rs` 的 `#[cfg(test)]` 模块内(若无则新建一个)添加:

```rust
#[test]
fn browser_rpc_frames_roundtrip() {
    use uuid::Uuid;
    let sid = Uuid::new_v4();
    let raw = serde_json::value::RawValue::from_string(r#"{"id":1}"#.to_string()).unwrap();
    let c = ClientMsg::BrowserRpc { session_id: sid, payload: raw };
    let j = serde_json::to_string(&c).unwrap();
    assert!(j.contains("\"type\":\"browser_rpc\""));
    let _back: ClientMsg = serde_json::from_str(&j).unwrap();

    let raw2 = serde_json::value::RawValue::from_string(r#"{"id":2}"#.to_string()).unwrap();
    let s = ServerMsg::BrowserRpc { session_id: sid, payload: raw2 };
    let j2 = serde_json::to_string(&s).unwrap();
    let _back2: ServerMsg = serde_json::from_str(&j2).unwrap();
}
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo test -p cloudcode-agent browser_rpc_frames_roundtrip`
Expected: 编译失败。

- [ ] **Step 3: 加变体**

在 `tunnel.rs` import 区确保有 `use serde_json::value::RawValue;`(没有则加)。

在 `enum ClientMsg`(agent→hub)末尾变体处加:

```rust
    /// One opaque MCP JSON-RPC frame from the agent's resident MCP
    /// endpoint (claude is the MCP client) toward the bound client's
    /// browser subprocess. `session_id` is the routing key; payload is
    /// never parsed in transit.
    BrowserRpc {
        session_id: Uuid,
        payload: Box<RawValue>,
    },
```

在 `enum ServerMsg`(hub→agent)末尾变体处加:

```rust
    /// One opaque MCP JSON-RPC frame from the client's browser
    /// subprocess back toward claude, routed by `session_id` to the
    /// matching MCP endpoint SSE stream. Payload is never parsed.
    BrowserRpc {
        session_id: Uuid,
        payload: Box<RawValue>,
    },
```

- [ ] **Step 4: 跑测试确认通过**

Run: `cargo test -p cloudcode-agent browser_rpc_frames_roundtrip`
Expected: PASS。

- [ ] **Step 5: 提交**

```bash
git add crates/agent/src/tunnel.rs
git commit -m "feat(proto): add BrowserRpc to agent<->hub protocol"
```

---

## Task 4: hub 路由 — agent→client(classify + relay 翻译)

**Files:**
- Modify: `crates/hub/src/registry.rs`(`classify`)
- Modify: `crates/hub/src/pty_session.rs`(`handle_agent_event`)
- Test: `crates/hub/src/registry.rs`

`ClientMsg::BrowserRpc{session_id}` 进 hub → `classify` 归为 `Routing::Session(session_id)` → 落进该 session 的 `sessions` 通道,变成 `PtyEventOut::Frame(ClientMsg::BrowserRpc)` → relay 循环翻译成 `HubToClient::BrowserRpc{payload}` 发给 client。

- [ ] **Step 1: 写失败测试(classify 路由)**

在 `registry.rs` 的 `#[cfg(test)] mod tests` 内添加:

```rust
#[test]
fn browser_rpc_classifies_as_session() {
    use uuid::Uuid;
    let sid = Uuid::new_v4();
    let raw = serde_json::value::RawValue::from_string(r#"{"id":1}"#.to_string()).unwrap();
    let frame = ClientMsg::BrowserRpc { session_id: sid, payload: raw };
    match classify(&frame) {
        Routing::Session(got) => assert_eq!(got, sid),
        _ => panic!("BrowserRpc should route by session"),
    }
}
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo test -p cloudcode-hub browser_rpc_classifies_as_session`
Expected: 编译失败(`classify` 未覆盖该变体,或 non-exhaustive match 报错)。

- [ ] **Step 3: classify 加分支**

在 `registry.rs` 的 `fn classify` 中,`ClientMsg::PtyOpened { session_id, .. } | ...` 那组 `Routing::Session(*session_id)` 里追加 `BrowserRpc`:

```rust
        ClientMsg::PtyOpened { session_id, .. }
        | ClientMsg::PtyClosed { session_id, .. }
        | ClientMsg::PtyError { session_id, .. }
        | ClientMsg::SplitPaneResult { session_id, .. }
        | ClientMsg::BrowserRpc { session_id, .. } => Routing::Session(*session_id),
```

- [ ] **Step 4: 跑测试确认通过**

Run: `cargo test -p cloudcode-hub browser_rpc_classifies_as_session`
Expected: PASS。

- [ ] **Step 5: relay 翻译(agent→client)**

在 `pty_session.rs` 的 `handle_agent_event`(处理 `PtyEventOut::Frame` 的地方,即把 agent `ClientMsg` 翻译给 client 的分支)中,为 `ClientMsg::BrowserRpc` 加翻译。找到 `PtyEventOut::Frame(frame) => { match frame { ... } }` 区域,加:

```rust
            ClientMsg::BrowserRpc { payload, .. } => {
                send_client(sink, &HubToClient::BrowserRpc { payload }).await
            }
```

> 注:`send_client` 是 `pty_session.rs:1524` 的现有 helper;返回 bool 控制循环是否继续,沿用同区其他分支的返回风格。若该 match 在别处对未知帧有 `_ =>` 兜底,把本分支放在兜底之前。

- [ ] **Step 6: 编译并跑全部 hub 测试**

Run: `cargo test -p cloudcode-hub`
Expected: 全绿(含 Task 1、Task 4 新测试)。

- [ ] **Step 7: 提交**

```bash
git add crates/hub/src/registry.rs crates/hub/src/pty_session.rs
git commit -m "feat(hub): route agent BrowserRpc to client by session_id"
```

---

## Task 5: hub 路由 — client→agent(handle_client_frame 翻译)

**Files:**
- Modify: `crates/hub/src/pty_session.rs`(`handle_client_frame`)

`ClientToHub::BrowserRpc{payload}` 到 hub → 用 `ctx.active.session_id` 标记 → `ctx.selected_agent.send(ServerMsg::BrowserRpc{session_id, payload})`。`ClientToHub::BrowserClosed` 同理转 agent(M1 可仅记日志,M2 再接授权门)。

- [ ] **Step 1: 加 match 分支**

在 `pty_session.rs` 的 `handle_client_frame`(pty_session.rs:355 起的 exhaustive match)中,`ClientToHub::Close => false,` 之前加:

```rust
        ClientToHub::BrowserRpc { payload } => {
            if let (Some(conn), Some(active)) =
                (ctx.selected_agent.as_ref(), ctx.active.as_ref())
            {
                let _ = conn
                    .send(ServerMsg::BrowserRpc {
                        session_id: active.session_id,
                        payload,
                    })
                    .await;
            }
            true
        }
        ClientToHub::BrowserClosed { reason } => {
            tracing::debug!(?reason, "client closed browser channel");
            // M2 接授权门 / 通知 agent 端点。M1 仅记录。
            true
        }
```

> `ServerMsg` import 在本文件已存在(现有代码已用 `ServerMsg::PtyOpen` 等)。

- [ ] **Step 2: 编译**

Run: `cargo build -p cloudcode-hub`
Expected: 通过,无 non-exhaustive match 报错。

- [ ] **Step 3: 提交**

```bash
git add crates/hub/src/pty_session.rs
git commit -m "feat(hub): forward client BrowserRpc to agent as ServerMsg"
```

---

## Task 6: 回显桩 MCP server(测试夹具)

**Files:**
- Create: `test-fixtures/echo-mcp.mjs`

M1 用它替真 `@playwright/mcp`:从 stdin 读逐行 JSON-RPC,对 `initialize`/`tools/list`/`tools/call` 回最小合法响应,证明透传链路。

- [ ] **Step 1: 写桩脚本**

```javascript
// test-fixtures/echo-mcp.mjs
// Minimal MCP-over-stdio echo stub for M1 pipe testing.
// Reads line-delimited JSON-RPC on stdin, writes responses on stdout.
import readline from 'node:readline';

const rl = readline.createInterface({ input: process.stdin });
function send(obj) { process.stdout.write(JSON.stringify(obj) + '\n'); }

rl.on('line', (line) => {
  line = line.trim();
  if (!line) return;
  let msg;
  try { msg = JSON.parse(line); } catch { return; }
  if (msg.method === 'initialize') {
    send({ jsonrpc: '2.0', id: msg.id, result: {
      protocolVersion: '2024-11-05',
      capabilities: { tools: {} },
      serverInfo: { name: 'echo-mcp', version: '0.0.1' },
    }});
  } else if (msg.method === 'tools/list') {
    send({ jsonrpc: '2.0', id: msg.id, result: { tools: [
      { name: 'echo', description: 'echo back text',
        inputSchema: { type: 'object', properties: { text: { type: 'string' } } } },
    ]}});
  } else if (msg.method === 'tools/call') {
    const text = msg.params?.arguments?.text ?? '';
    send({ jsonrpc: '2.0', id: msg.id, result: {
      content: [{ type: 'text', text: `echo: ${text}` }],
    }});
  } else if (msg.id !== undefined) {
    send({ jsonrpc: '2.0', id: msg.id, result: {} });
  }
});
```

- [ ] **Step 2: 手动验证桩可用**

Run: `printf '%s\n' '{"jsonrpc":"2.0","id":1,"method":"tools/list"}' | node test-fixtures/echo-mcp.mjs`
Expected: 输出一行含 `"name":"echo"` 的 JSON。

- [ ] **Step 3: 提交**

```bash
git add test-fixtures/echo-mcp.mjs
git commit -m "test: add echo MCP stub for browser pipe M1"
```

---

## Task 7: client cc_browser 模块 — 子进程管道

**Files:**
- Create: `crates/client/src/cc_browser.rs`
- Modify: `crates/client/src/main.rs`(`mod cc_browser;`)
- Test: `crates/client/src/cc_browser.rs`

职责:给定子进程命令(M1 = `node test-fixtures/echo-mcp.mjs`),spawn 之,提供 `feed(payload)`(写一行到 stdin)和一个输出流(stdout 逐行 → `Box<RawValue>`)。能力探测:`browser_capable()` 返回 Node 是否可用。

- [ ] **Step 1: 写失败测试**

```rust
// 文件末尾
#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn echo_stub_roundtrips_tools_list() {
        // 跳过条件:无 node 时不跑(CI 装了 node 才有意义)
        if which_node().is_none() { return; }
        let mut proc = McpProcess::spawn(
            "node",
            &["test-fixtures/echo-mcp.mjs"],
        )
        .expect("spawn echo stub");
        let req = serde_json::value::RawValue::from_string(
            r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#.to_string(),
        )
        .unwrap();
        proc.feed(&req).await.unwrap();
        let resp = proc.next_frame().await.expect("got a frame");
        assert!(resp.get().contains("echo"));
    }
}
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo test -p cloudcode-client echo_stub_roundtrips`
Expected: 编译失败(`McpProcess` / `which_node` 未定义)。

- [ ] **Step 3: 写实现**

```rust
// crates/client/src/cc_browser.rs
//! Manages a local MCP-over-stdio subprocess (M1: an echo stub;
//! M2: @playwright/mcp). Pipes opaque JSON-RPC frames in/out.

use serde_json::value::RawValue;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines};
use tokio::process::{Child, ChildStdin, Command};

/// Best-effort check whether `node` is on PATH (capability probe).
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

pub struct McpProcess {
    child: Child,
    stdin: ChildStdin,
    lines: Lines<BufReader<tokio::process::ChildStdout>>,
}

impl McpProcess {
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
    pub async fn feed(&mut self, payload: &RawValue) -> std::io::Result<()> {
        self.stdin.write_all(payload.get().as_bytes()).await?;
        self.stdin.write_all(b"\n").await?;
        self.stdin.flush().await
    }

    /// Read the next JSON-RPC frame from the subprocess, or None on EOF.
    pub async fn next_frame(&mut self) -> Option<Box<RawValue>> {
        loop {
            let line = self.lines.next_line().await.ok()??;
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            if let Ok(raw) = RawValue::from_string(trimmed.to_string()) {
                return Some(raw);
            }
        }
    }

    pub async fn shutdown(mut self) {
        let _ = self.child.start_kill();
    }
}
```

在 `crates/client/src/main.rs` 的 mod 声明区加 `mod cc_browser;`。

- [ ] **Step 4: 跑测试确认通过**

Run: `cargo test -p cloudcode-client echo_stub_roundtrips`
Expected: PASS(机器有 node 时);无 node 时测试体提前 return 也算 PASS。

- [ ] **Step 5: 提交**

```bash
git add crates/client/src/cc_browser.rs crates/client/src/main.rs
git commit -m "feat(client): cc_browser MCP subprocess pipe with echo stub test"
```

---

## Task 8: client relay 分发 BrowserRpc → cc_browser

**Files:**
- Modify: `crates/client/src/cc_browser.rs`(加 `BrowserChannel` 任务封装)
- Modify: `crates/client/src/relay.rs`(分发)
- Test: `crates/client/src/cc_browser.rs`

把"收 `HubToClient::BrowserRpc` → feed 子进程 → 读输出 → 发 `ClientToHub::BrowserRpc`"封成一个后台任务,relay 循环只负责把入帧塞进它的 channel。

- [ ] **Step 1: 写失败测试(channel 级往返)**

在 `cc_browser.rs` 测试模块加:

```rust
    #[tokio::test]
    async fn channel_pumps_frames_both_ways() {
        if which_node().is_none() { return; }
        let (out_tx, mut out_rx) = tokio::sync::mpsc::channel(8);
        let chan = BrowserChannel::start("node", &["test-fixtures/echo-mcp.mjs"], out_tx)
            .expect("start channel");
        let req = serde_json::value::RawValue::from_string(
            r#"{"jsonrpc":"2.0","id":7,"method":"tools/list"}"#.to_string(),
        ).unwrap();
        chan.feed(req).await.unwrap();
        let got = out_rx.recv().await.expect("a response frame");
        assert!(got.get().contains("echo"));
    }
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo test -p cloudcode-client channel_pumps_frames`
Expected: 编译失败(`BrowserChannel` 未定义)。

- [ ] **Step 3: 实现 BrowserChannel**

在 `cc_browser.rs` 加:

```rust
use tokio::sync::mpsc;

/// A running browser MCP channel: owns the subprocess + a reader task
/// that forwards every output frame to `out_tx`. `feed` enqueues an
/// inbound frame for the subprocess.
pub struct BrowserChannel {
    in_tx: mpsc::Sender<Box<RawValue>>,
}

impl BrowserChannel {
    pub fn start(
        program: &str,
        args: &[&str],
        out_tx: mpsc::Sender<Box<RawValue>>,
    ) -> std::io::Result<Self> {
        let mut proc = McpProcess::spawn(program, args)?;
        let (in_tx, mut in_rx) = mpsc::channel::<Box<RawValue>>(32);
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    inbound = in_rx.recv() => {
                        let Some(frame) = inbound else { break; };
                        if proc.feed(&frame).await.is_err() { break; }
                    }
                    outbound = proc.next_frame() => {
                        match outbound {
                            Some(frame) => {
                                if out_tx.send(frame).await.is_err() { break; }
                            }
                            None => break, // subprocess EOF
                        }
                    }
                }
            }
            proc.shutdown().await;
        });
        Ok(Self { in_tx })
    }

    pub async fn feed(&self, frame: Box<RawValue>) -> Result<(), ()> {
        self.in_tx.send(frame).await.map_err(|_| ())
    }
}
```

- [ ] **Step 4: 跑测试确认通过**

Run: `cargo test -p cloudcode-client channel_pumps_frames`
Expected: PASS(有 node 时)。

- [ ] **Step 5: relay 接线**

在 `relay.rs` 的会话中继循环(relay.rs:215-234 的 `match frame`)里,把 `_ => {}` 兜底前加入:

```rust
                        HubToClient::BrowserRpc { payload } => {
                            // 懒启动 browser channel(首帧时拉起子进程)
                            if browser.is_none() {
                                if cc_browser::which_node().is_some() {
                                    if let Ok(ch) = cc_browser::BrowserChannel::start(
                                        "node",
                                        &["test-fixtures/echo-mcp.mjs"],
                                        browser_out_tx.clone(),
                                    ) {
                                        browser = Some(ch);
                                    }
                                }
                            }
                            if let Some(ch) = browser.as_ref() {
                                let _ = ch.feed(payload).await;
                            }
                        }
                        HubToClient::BrowserClosed { .. } => {
                            browser = None; // drop → kill_on_drop 清理子进程
                        }
```

在该 `select!` 所在函数开头,创建 channel 与状态:

```rust
    let mut browser: Option<cc_browser::BrowserChannel> = None;
    let (browser_out_tx, mut browser_out_rx) =
        tokio::sync::mpsc::channel::<Box<serde_json::value::RawValue>>(64);
```

并在同一 `select!` 里加一条把子进程输出回传 hub 的分支:

```rust
                    out = browser_out_rx.recv() => {
                        if let Some(payload) = out {
                            let _ = wire.out_tx
                                .send(OutFrame::Text(ClientToHub::BrowserRpc { payload }))
                                .await;
                        }
                    }
```

> `wire.out_tx`、`OutFrame::Text`、`ClientToHub` 均为本文件已用类型。`use crate::cc_browser;` 加到文件顶部。

- [ ] **Step 6: 编译 + 全部 client 测试**

Run: `cargo test -p cloudcode-client`
Expected: 全绿。

- [ ] **Step 7: 提交**

```bash
git add crates/client/src/cc_browser.rs crates/client/src/relay.rs
git commit -m "feat(client): pump HubToClient BrowserRpc through MCP subprocess"
```

---

## Task 9: agent MCP 端点 — HTTP 监听 + session token 路由骨架

**Files:**
- Create: `crates/agent/src/mcp_endpoint.rs`
- Modify: `crates/agent/src/main.rs`(`mod mcp_endpoint;`、`AppState` 加端点状态、`serve()` spawn 监听)
- Test: `crates/agent/src/mcp_endpoint.rs`

端点状态:`DashMap<token(String), SessionRoute>`,其中 `SessionRoute { session_id: Uuid, to_claude: mpsc::Sender<Box<RawValue>> }`。本任务只建监听 + 一个 `GET /healthz` 返回 200,验证端点起得来;MCP 收发在 Task 10。

- [ ] **Step 1: 写失败测试**

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn endpoint_state_registers_and_resolves_token() {
        let state = EndpointState::new();
        let sid = uuid::Uuid::new_v4();
        let (tx, _rx) = tokio::sync::mpsc::channel(4);
        state.register("tok-abc".into(), sid, tx);
        assert_eq!(state.session_for("tok-abc"), Some(sid));
        assert_eq!(state.session_for("nope"), None);
        state.unregister("tok-abc");
        assert_eq!(state.session_for("tok-abc"), None);
    }
}
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo test -p cloudcode-agent endpoint_state_registers`
Expected: 编译失败。

- [ ] **Step 3: 写 EndpointState + 监听骨架**

```rust
// crates/agent/src/mcp_endpoint.rs
//! Resident localhost MCP HTTP/SSE endpoint. claude (the MCP client)
//! connects here; frames are tunneled to the bound CloudCode client
//! over the existing agent<->hub ws as BrowserRpc.

use dashmap::DashMap;
use serde_json::value::RawValue;
use std::sync::Arc;
use tokio::sync::mpsc;
use uuid::Uuid;

/// Per-session routing: maps a claude-facing token to its session_id
/// and the channel that delivers subprocess responses back to claude.
pub struct SessionRoute {
    pub session_id: Uuid,
    pub to_claude: mpsc::Sender<Box<RawValue>>,
}

#[derive(Clone)]
pub struct EndpointState {
    routes: Arc<DashMap<String, SessionRoute>>,
    /// Set when the agent ws is up: lets the endpoint emit ClientMsg
    /// frames toward the hub. Stored as a sender of the agent's OutFrame.
    to_hub: Arc<tokio::sync::RwLock<Option<mpsc::Sender<crate::pty::OutFrame>>>>,
}

impl EndpointState {
    pub fn new() -> Self {
        Self {
            routes: Arc::new(DashMap::new()),
            to_hub: Arc::new(tokio::sync::RwLock::new(None)),
        }
    }

    pub fn register(&self, token: String, session_id: Uuid, to_claude: mpsc::Sender<Box<RawValue>>) {
        self.routes.insert(token, SessionRoute { session_id, to_claude });
    }

    pub fn unregister(&self, token: &str) {
        self.routes.remove(token);
    }

    pub fn session_for(&self, token: &str) -> Option<Uuid> {
        self.routes.get(token).map(|r| r.session_id)
    }

    /// Deliver a response frame (from hub/client) to claude's SSE stream
    /// for the given session_id. Returns false if no live route.
    pub async fn deliver_to_claude(&self, session_id: Uuid, payload: Box<RawValue>) -> bool {
        for entry in self.routes.iter() {
            if entry.session_id == session_id {
                return entry.to_claude.send(payload).await.is_ok();
            }
        }
        false
    }

    pub async fn set_hub_sender(&self, tx: mpsc::Sender<crate::pty::OutFrame>) {
        *self.to_hub.write().await = Some(tx);
    }

    pub async fn send_to_hub(&self, frame: crate::pty::OutFrame) {
        if let Some(tx) = self.to_hub.read().await.as_ref() {
            let _ = tx.send(frame).await;
        }
    }
}

/// Bind the localhost MCP listener. Routes added in Task 10.
pub async fn serve(state: EndpointState, port: u16) -> std::io::Result<()> {
    use axum::routing::get;
    let app = axum::Router::new()
        .route("/healthz", get(|| async { "ok" }))
        .with_state(state);
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", port)).await?;
    axum::serve(listener, app)
        .await
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))
}
```

在 `crates/agent/src/main.rs`:
- mod 区加 `mod mcp_endpoint;`
- `AppState`(main.rs:22-30)加字段 `pub mcp: mcp_endpoint::EndpointState,`,并在构造 `AppState` 处初始化 `mcp: mcp_endpoint::EndpointState::new(),`
- 在 `serve()`(main.rs ~line 231 manager 建好后)spawn:

```rust
    {
        let mcp_state = state.mcp.clone();
        let port = state.config.mcp_port.unwrap_or(7110);
        tokio::spawn(async move {
            if let Err(e) = mcp_endpoint::serve(mcp_state, port).await {
                tracing::error!(error = %e, "mcp endpoint exited");
            }
        });
    }
```

> 若 `Config` 无 `mcp_port`,在 `config.rs` 加 `pub mcp_port: Option<u16>,`(serde default)。

- [ ] **Step 4: 跑测试确认通过**

Run: `cargo test -p cloudcode-agent endpoint_state_registers`
Expected: PASS。

- [ ] **Step 5: 冒烟 healthz**

Run(另开窗口跑 agent 后):`curl -s 127.0.0.1:7110/healthz`
Expected: `ok`。(若不便起完整 agent,可加一个 `#[tokio::test]` 直接调用 `serve` 于随机端口再 `curl`;否则此步标记手动验证。)

- [ ] **Step 6: 提交**

```bash
git add crates/agent/src/mcp_endpoint.rs crates/agent/src/main.rs crates/agent/src/config.rs
git commit -m "feat(agent): resident localhost MCP endpoint skeleton + state"
```

---

## Task 10: agent 端点 ↔ ws 双向接线 + MCP 收发

**Files:**
- Modify: `crates/agent/src/mcp_endpoint.rs`(MCP POST + SSE 路由)
- Modify: `crates/agent/src/ws.rs`(分发 `ServerMsg::BrowserRpc` → `deliver_to_claude`;启动时 `set_hub_sender`)
- Test: `crates/agent/src/mcp_endpoint.rs`

claude→端点的入帧:POST `/mcp/:token` body 是 JSON-RPC → 查 `session_for(token)` → `send_to_hub(OutFrame::Text(ClientMsg::BrowserRpc{session_id, payload}))`。端点→claude 的响应:SSE `GET /mcp/:token` 持有 `to_claude` 接收端,把帧写成 SSE event。`ServerMsg::BrowserRpc` 入境时 `deliver_to_claude(session_id, payload)`。

- [ ] **Step 1: 写失败测试(POST 入帧打到 hub sender)**

```rust
    #[tokio::test]
    async fn post_frame_forwards_to_hub_as_client_msg() {
        let state = EndpointState::new();
        let sid = uuid::Uuid::new_v4();
        let (claude_tx, _claude_rx) = tokio::sync::mpsc::channel(4);
        state.register("tok-1".into(), sid, claude_tx);
        let (hub_tx, mut hub_rx) = tokio::sync::mpsc::channel(4);
        state.set_hub_sender(hub_tx).await;

        let body = r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#;
        handle_post("tok-1", body.to_string(), state.clone())
            .await
            .expect("accepted");

        let out = hub_rx.recv().await.expect("a frame to hub");
        match out {
            crate::pty::OutFrame::Text(crate::tunnel::ClientMsg::BrowserRpc { session_id, payload }) => {
                assert_eq!(session_id, sid);
                assert!(payload.get().contains("tools/list"));
            }
            _ => panic!("expected ClientMsg::BrowserRpc"),
        }
    }
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo test -p cloudcode-agent post_frame_forwards`
Expected: 编译失败(`handle_post` 未定义)。

- [ ] **Step 3: 实现 handle_post + SSE 路由 + 挂路由**

在 `mcp_endpoint.rs` 加:

```rust
use crate::tunnel::ClientMsg;
use crate::pty::OutFrame;

/// Core of the MCP POST handler, factored out for unit testing.
pub async fn handle_post(token: &str, body: String, state: EndpointState) -> Result<(), &'static str> {
    let session_id = state.session_for(token).ok_or("unknown token")?;
    let payload = RawValue::from_string(body).map_err(|_| "bad json")?;
    state
        .send_to_hub(OutFrame::Text(ClientMsg::BrowserRpc { session_id, payload }))
        .await;
    Ok(())
}
```

并在 `serve()` 的 router 上挂(替换骨架版):

```rust
    use axum::routing::{get, post};
    use axum::extract::{Path, State};
    let app = axum::Router::new()
        .route("/healthz", get(|| async { "ok" }))
        .route("/mcp/:token", post(
            |Path(token): Path<String>, State(st): State<EndpointState>, body: String| async move {
                match handle_post(&token, body, st).await {
                    Ok(()) => axum::http::StatusCode::ACCEPTED,
                    Err(_) => axum::http::StatusCode::BAD_REQUEST,
                }
            },
        ))
        .route("/mcp/:token", get(
            |Path(token): Path<String>, State(st): State<EndpointState>| async move {
                sse_handler(token, st).await
            },
        ))
        .with_state(state);
```

SSE handler(把 `to_claude` 接收端流式写出):

```rust
use axum::response::sse::{Event, Sse};
use futures::stream::Stream;

async fn sse_handler(
    token: String,
    state: EndpointState,
) -> Sse<impl Stream<Item = Result<Event, std::convert::Infallible>>> {
    let (tx, rx) = mpsc::channel::<Box<RawValue>>(64);
    // 该 SSE 连接到来时,token 已由 claude 启动注入;补登 to_claude sink。
    if let Some(sid) = state.session_for(&token) {
        state.register(token.clone(), sid, tx);
    }
    let stream = async_stream::stream! {
        let mut rx = rx;
        while let Some(frame) = rx.recv().await {
            yield Ok(Event::default().data(frame.get().to_string()));
        }
    };
    Sse::new(stream)
}
```

> 依赖:`crates/agent/Cargo.toml` 加 `async-stream = "0.3"`、`futures.workspace = true`(若未引);`axum` 需 `features` 含默认(SSE 在 axum 0.7 默认 feature)。

在 `ws.rs` 的 `read_loop`(ws.rs:173 起的 ServerMsg match),在 PTY 分发前加:

```rust
                Ok(ServerMsg::BrowserRpc { session_id, payload }) => {
                    state.mcp.deliver_to_claude(session_id, payload).await;
                }
```

并在 ws 连接建立、拿到 `tx`(ws.rs:100 的 `mpsc::Sender<OutFrame>`)后,登记给端点:

```rust
    state.mcp.set_hub_sender(tx.clone()).await;
```

- [ ] **Step 4: 跑测试确认通过**

Run: `cargo test -p cloudcode-agent post_frame_forwards`
Expected: PASS。

- [ ] **Step 5: 编译全 agent**

Run: `cargo build -p cloudcode-agent`
Expected: 通过。

- [ ] **Step 6: 提交**

```bash
git add crates/agent/src/mcp_endpoint.rs crates/agent/src/ws.rs crates/agent/Cargo.toml
git commit -m "feat(agent): wire MCP endpoint <-> hub ws (POST in, SSE out, ServerMsg deliver)"
```

---

## Task 11: agent 给 claude 注入 MCP 配置

**Files:**
- Modify: `crates/agent/src/pty.rs`(`open_session`)
- Modify: `crates/agent/src/mcp_endpoint.rs`(token 生成 helper)

每开一个 workspace 的 claude,生成一个随机 token,`state.mcp.register(token, session_id, <延迟到 SSE 补登>)`,并给 claude 注入一份 MCP 配置指向 `http://127.0.0.1:<port>/mcp/<token>`(SSE 类型)。

- [ ] **Step 1: 加 token 生成 + 注册占位**

在 `mcp_endpoint.rs` 加:

```rust
impl EndpointState {
    /// Reserve a session route before claude connects. The SSE handler
    /// will replace the placeholder sink on connect. Returns the token.
    pub fn reserve(&self, session_id: Uuid) -> String {
        let token = format!("{}", Uuid::new_v4().simple());
        let (placeholder, _rx) = mpsc::channel(1);
        self.routes.insert(token.clone(), SessionRoute { session_id, to_claude: placeholder });
        token
    }
}
```

- [ ] **Step 2: open_session 注入(写测试覆盖配置内容生成)**

先把"生成 MCP 配置 JSON 字符串"抽成纯函数并测它:

```rust
// mcp_endpoint.rs
/// Build the `--mcp-config` JSON claude should load for this session.
pub fn mcp_config_json(port: u16, token: &str) -> String {
    format!(
        r#"{{"mcpServers":{{"cc-browser":{{"type":"sse","url":"http://127.0.0.1:{port}/mcp/{token}"}}}}}}"#
    )
}

#[cfg(test)]
mod cfg_tests {
    use super::*;
    #[test]
    fn config_has_sse_url_with_token() {
        let s = mcp_config_json(7110, "abc123");
        assert!(s.contains("\"type\":\"sse\""));
        assert!(s.contains("/mcp/abc123"));
        let _: serde_json::Value = serde_json::from_str(&s).unwrap(); // 合法 JSON
    }
}
```

Run: `cargo test -p cloudcode-agent config_has_sse_url`
Expected: 先失败(函数未定义)→ 加函数后 PASS。

- [ ] **Step 3: 在 open_session 接线**

在 `pty.rs:open_session`(pty.rs:512,构造 argv/env 处,约 775-823)加:

```rust
        let mcp_port = self.config.mcp_port.unwrap_or(7110);
        let mcp_token = self.mcp.reserve(session_id);
        let mcp_cfg = crate::mcp_endpoint::mcp_config_json(mcp_port, &mcp_token);
        // 写到 workspace 下临时文件,作为 `claude --mcp-config <file>` 参数
        let mcp_cfg_path = workspace_dir.join(".cloudcode").join("mcp-browser.json");
        if let Some(parent) = mcp_cfg_path.parent() { let _ = std::fs::create_dir_all(parent); }
        let _ = std::fs::write(&mcp_cfg_path, &mcp_cfg);
        // 追加到 claude 启动参数
        extra_claude_args.push("--mcp-config".to_string());
        extra_claude_args.push(mcp_cfg_path.to_string_lossy().to_string());
```

> `self.mcp` 需让 `PtyManager` 持有 `EndpointState`:在 `PtyManager` 结构体加 `pub mcp: crate::mcp_endpoint::EndpointState,`,构造处从 `AppState.mcp.clone()` 传入。`workspace_dir`、`extra_claude_args`(或等价的 argv 累加变量)沿用现有 open_session 局部变量名;若现有变量名不同,按实际拼接 argv 的位置插入这两个参数。

- [ ] **Step 4: 编译**

Run: `cargo build -p cloudcode-agent`
Expected: 通过。

- [ ] **Step 5: 提交**

```bash
git add crates/agent/src/pty.rs crates/agent/src/mcp_endpoint.rs
git commit -m "feat(agent): inject per-session MCP config into claude launch"
```

---

## Task 12: 能力协商 — Hello 带 browser_capable + 无能力时不暴露工具

**Files:**
- Modify: `crates/hub/src/pty_proto.rs` / `crates/client/src/proto.rs`(`Hello` 加字段)
- Modify: `crates/client/src/main.rs`(探测 node 填字段)
- Modify: `crates/hub/src/pty_session.rs`(把 `browser_capable` 透传给 agent;无能力时不给 claude 注入 MCP 配置)
- Modify: `crates/agent/src/tunnel.rs`(`ServerMsg::PtyOpen` 加 `browser_capable: bool`)
- Modify: `crates/agent/src/pty.rs`(仅当 `browser_capable` 才注入 MCP 配置)

M1 的"能力协商"落到最小可用:client 上报有无 node;无 node 则 agent 不给 claude 注入 cc-browser MCP 配置 → claude 的工具列表里压根没有浏览器工具。

- [ ] **Step 1: Hello 加字段(hub + client 锁步)**

`ClientToHub::Hello` 在两个 proto 文件里加:

```rust
    Hello {
        token: String,
        version: String,
        #[serde(default)]
        browser_capable: bool,
    },
```

- [ ] **Step 2: client 填字段**

在 `crates/client/src/main.rs` 构造 `Hello` 处,设 `browser_capable: cc_browser::which_node().is_some(),`。

- [ ] **Step 3: ServerMsg::PtyOpen 加字段 + hub 透传**

`agent/src/tunnel.rs` 的 `ServerMsg::PtyOpen` 加 `#[serde(default)] browser_capable: bool,`。
hub `pty_session.rs` 处理 `OpenSession` 发 `PtyOpen` 时,带上本连接 Hello 记录的 `browser_capable`(在 `ConnCtx` 加 `browser_capable: bool` 字段,Hello 时写入)。

- [ ] **Step 4: pty.rs 条件注入**

把 Task 11 Step 3 的注入逻辑包一层 `if browser_capable {{ ... }}`(`browser_capable` 来自 `PtyOpen` 帧)。

- [ ] **Step 5: 写测试(序列化默认值兼容)**

在 `pty_proto.rs` browser_tests 加:旧 Hello(无 `browser_capable`)能反序列化为 `false`:

```rust
    #[test]
    fn hello_without_browser_capable_defaults_false() {
        let j = r#"{"type":"hello","token":"t","version":"1"}"#;
        let h: ClientToHub = serde_json::from_str(j).unwrap();
        match h { ClientToHub::Hello { browser_capable, .. } => assert!(!browser_capable), _ => panic!() }
    }
```

Run: `cargo test -p cloudcode-hub hello_without_browser_capable`
Expected: PASS。

- [ ] **Step 6: 编译三 crate**

Run: `cargo build`
Expected: 全通过。

- [ ] **Step 7: 提交**

```bash
git add -A
git commit -m "feat: browser capability negotiation via Hello.browser_capable"
```

---

## Task 13: 端到端集成验证(echo 桩穿全链路)

**Files:**
- Create: `crates/hub/tests/browser_pipe_e2e.rs`(或就近的现有集成测试目录)

用进程内起 hub + 假 agent + 假 client,或最务实地:跑真三件套连本地、用 echo 桩,手动验证 claude 侧 `tools/list` 能看到 `echo` 工具且 `tools/call` 返回 `echo: ...`。M1 至少要有一条**自动**集成测试覆盖 hub 路由双向翻译。

- [ ] **Step 1: 写 hub 路由集成测试(无需真 agent/client)**

直接测 hub 的两条翻译路径:构造 `ClientToHub::BrowserRpc` 经 `handle_client_frame` 应产生发往 agent 的 `ServerMsg::BrowserRpc`(用 mock `AgentConn` 的 `send` 捕获);构造 agent 侧 `ClientMsg::BrowserRpc` 经 `classify`+relay 应产生 `HubToClient::BrowserRpc`。

> 若 `AgentConn`/`ConnCtx` 不易在测试中构造,退而求其次:为 Task 4/5 的翻译逻辑各抽一个纯函数 `translate_agent_browser_rpc(ClientMsg) -> Option<HubToClient>` 与 `translate_client_browser_rpc(payload, session_id) -> ServerMsg`,对纯函数写单测。重构 Task 4/5 调用这些纯函数。

```rust
#[test]
fn agent_browser_rpc_translates_to_hubtoclient() {
    use cloudcode_hub::pty_session::translate_agent_browser_rpc;
    use cloudcode_hub::tunnel::ClientMsg;
    let sid = uuid::Uuid::new_v4();
    let raw = serde_json::value::RawValue::from_string(r#"{"id":9}"#.to_string()).unwrap();
    let out = translate_agent_browser_rpc(ClientMsg::BrowserRpc { session_id: sid, payload: raw });
    assert!(matches!(out, Some(cloudcode_hub::pty_proto::HubToClient::BrowserRpc { .. })));
}
```

- [ ] **Step 2: 跑测试**

Run: `cargo test -p cloudcode-hub browser_rpc_translates`
Expected: PASS(需把翻译抽成 `pub fn` 并在 Task 4/5 调用)。

- [ ] **Step 3: 手动端到端冒烟(记录在 PR 描述)**

1. `cargo build --release`
2. 起 hub、起 agent(确认日志 `mcp endpoint` 在 7110 监听)、起 client(机器装了 node)
3. 在 client 开一个 workspace 进 claude
4. 在 claude 里让它列出 MCP 工具 → 应能看到 `cc-browser` 的 `echo` 工具
5. 让 claude 调 `echo`,text=hello → 应返回 `echo: hello`
6. 在无 node 的机器重复 3-4 → claude 看不到 `cc-browser` 工具(能力协商生效)

Expected: 上述行为成立 → M1 透明管道打通。

- [ ] **Step 4: 提交**

```bash
git add crates/hub/tests/browser_pipe_e2e.rs crates/hub/src/pty_session.rs
git commit -m "test(hub): browser rpc translation unit + e2e smoke notes"
```

---

## 后续里程碑(本计划不实现,各自独立成 plan)

- **M2 真浏览器 + 授权门**:client `cc_browser` 把 `node test-fixtures/echo-mcp.mjs` 换成 `npx @playwright/mcp@latest`;实现授权门状态机(纯函数 `should_allow` TDD 先行,IDLE/PENDING/GRANTED + 空闲超时 + session 关闭硬顶 + 重连不恢复);首帧拦截走 `menu::prompt_confirm` 弹原生 TUI 模态 + 系统通知/响铃;deny → `ClientToHub::BrowserClosed{denied}` → 翻译成 MCP 错误给 claude。
- **M3 handoff + 收尾**:headless⇄headed(始终启有头、移屏外、bringToFront);注入 `request_handoff` 工具(拦 `tools/list` 响应追加);client 启发式监听登录页/密码框;"等待人工"态阻塞自动化帧;接管超时;`install.sh` client 分支预装 Node + `@playwright/mcp` + `playwright install chromium`;配置项(空闲超时、接管超时、mcp_port)文档化。

---

## Self-Review 备忘(写计划者已核对)

- **Spec 覆盖**:M1 对应 spec 的"数据流与消息集""组件(cc-mcp 端点/cc-browser/hub relay)""能力协商";授权门/handoff/错误处理的断线恢复明确归 M2/M3。
- **类型一致**:`payload: Box<RawValue>` 全程一致;agent 侧帧带 `session_id: Uuid`,client/hub-client 侧隐式;`EndpointState`/`BrowserChannel`/`McpProcess`/`handle_post`/`mcp_config_json` 命名在任务间一致。
- **已知实现风险(交给执行者注意)**:① axum 0.7 path 占位符语法 `/:token`;② SSE 补登 `to_claude` 与 `reserve` 占位 sink 的竞态(Task 10 Step 3 用 `register` 覆盖占位,需确保 claude 先建 SSE 再 POST,或在 POST 前等待 SSE 就绪——执行时若发现 claude 先 POST 后 SSE,改为端点侧短暂缓冲);③ Task 4/13 可能需把翻译抽纯函数才好测,已在 Task 13 给出退路。
