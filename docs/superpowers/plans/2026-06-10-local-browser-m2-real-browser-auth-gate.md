# 云端 claude 操作本地浏览器 — M2 真浏览器 + 授权门 实现计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 把 M1 的 echo 桩换成真 `@playwright/mcp`(headless),并加上会话级授权门:首个浏览器操作弹 relay 内联确认模态(y/n + 响铃),deny/空闲超时/会话关闭即收权;同时修复 M1 验证发现的 reattach token 缺口与 token 文件权限。

**Architecture:** 授权门是 client `cc_browser` 里的纯函数状态机(`AuthGate`),拦在 relay 的 `HubToClient::BrowserRpc` 臂之前;确认 UI 用 relay 内联纯 ANSI 浮框(仿 `main.rs:543 show_pill` 模式,模态期间 select! 其他臂自然暂停、channel 缓冲兜底),不建 ratatui Terminal。deny 经新增的 agent↔hub `BrowserClosed` 帧传到 agent 端点,把该 session 在飞请求全部以 JSON-RPC error 失败掉。playwright 子进程沿用 M1 的 `BrowserChannel` 按行管道(实测 0.0.76 输出单行 JSON、stderr 干净,直接兼容)。

**Tech Stack:** Rust(既有栈)+ `npx @playwright/mcp@0.0.76`(pin 版本)。

**分支:** `feature/local-browser-m2`(基于 feature/local-browser 大分支)。

**M2 不含**(留 M3 或后续):handoff(headless⇄headed 切换、`request_handoff` 注入、启发式检测)、install.sh 预装、webterm 浏览器通道(webterm 进的 session 调浏览器工具仍超时,文档已注明)、授权的"手动撤销"显式 UI(M2 的收权途径 = deny / 空闲超时 / 关会话;显式撤销入口推迟,需要的话用户关 session 即收权)。

---

## 摸查事实(写计划时已核实,implementer 直接引用)

- relay 期间终端:raw mode + **main screen**,PTY 输出在 `relay.rs` select! 的 `in_bin_rx` 臂直写 stdout;stdin 经 `input.rs` 的 `ByteRx`(`mpsc::Receiver<Vec<u8>>`,不可克隆)在 `bytes.recv()` 臂被独占消费,喂 `PasteDetector`。
- 会话中浮框先例:`crates/client/src/main.rs:543-583 show_pill()` —— 纯 ANSI 序列直写 stdout,无 ratatui。
- `@playwright/mcp@0.0.76` 实测:`npx -y @playwright/mcp@0.0.76` 默认 stdio、输出单行紧凑 JSON 以 `\n` 结尾、stderr 空;**默认 headed,必须 `--headless`**;profile 默认持久化(按 cwd hash),显式 `--user-data-dir` 更可控;同一 profile 单实例锁(重启子进程前旧浏览器必须已退);**勿用** 无 scope 的第三方包 `playwright-mcp`。
- M1 协议缺口:agent↔hub 段(两个 tunnel.rs)只有 `BrowserRpc`,**没有 `BrowserClosed`**;hub `handle_client_frame` 的 `ClientToHub::BrowserClosed` 臂目前是 noop(pty_session.rs ~1150)。
- reattach 现状(pty.rs + wrapper bash):wrapper 常驻 while 循环;首启 `"$TOOL_BIN" "$@"`($@ 含 `--mcp-config <path>`);resume 走 `eval "$RESUME_CMD" '"$@"'`(默认 `claude --continue`,**$@ 仍传入**,即 resume 命令行也带 `--mcp-config 同一路径`);每次 open_session 都重 mint token 并**覆写**该文件。因此:idle-reattach(旧 claude 被 pkill、wrapper 回环重启)→ 新 claude 重读文件 → **理论上拿到新 token,应该能通**;busy-reattach(旧 claude 进程仍活)→ 旧进程内存里是旧 token → 必死。M1 用户实测死的场景需先诊断属于哪类。
- token 文件:`<workspace>/.cloudcode/mcp-browser.json`,`std::fs::write` 默认权限(0644),含明文 token。

---

## File Structure

| 文件 | 改动 | 职责 |
|------|------|------|
| `crates/client/src/auth_gate.rs` | 新建 | 纯函数授权门状态机(TDD 先行) |
| `crates/client/src/cc_browser.rs` | 改 | M2 默认命令换 playwright;`mcp_command()` 重命名/扩展 |
| `crates/client/src/relay.rs` | 改 | BrowserRpc 臂前接授权门;内联 ANSI 确认模态;deny→BrowserClosed;空闲计时 |
| `crates/client/src/main.rs` | 改 | `mod auth_gate;` |
| `crates/agent/src/tunnel.rs` + `crates/hub/src/tunnel.rs` | 改 | 锁步加 `ServerMsg::BrowserClosed { session_id, reason }` |
| `crates/hub/src/pty_session.rs` | 改 | `ClientToHub::BrowserClosed` 臂从 noop 改为转发给 agent |
| `crates/agent/src/ws.rs` + `mcp_endpoint.rs` | 改 | 分发 BrowserClosed→`fail_pending(session_id, reason)` |
| `crates/agent/src/pty.rs` | 改 | token 文件写后 chmod 0600;(诊断后)reattach 修复 |
| `docs/superpowers/plans/2026-06-10-local-browser-m2-e2e-smoke.md` | 新建 | M2 手动冒烟(真浏览器版) |

---

## Task 1: AuthGate 纯函数状态机(TDD)

**Files:**
- Create: `crates/client/src/auth_gate.rs`
- Modify: `crates/client/src/main.rs`(`mod auth_gate;`)

spec 已定语义:会话级授权,放行谓词**仅空闲超时**;终止信号 = deny / 空闲超时(默认 10min,可配)/ 会话关闭(relay 退出自然终结,不进状态机)。状态机不做 IO、不知道 UI —— 它只回答"这帧该放行、该问用户、还是该拒"。

- [ ] **Step 1: 写失败测试**

```rust
// crates/client/src/auth_gate.rs
//! Session-scoped authorization gate for the browser channel.
//! Pure state machine: no IO, no clocks — callers pass `Instant::now()`.

use std::time::{Duration, Instant};

#[cfg(test)]
mod tests {
    use super::*;

    fn t0() -> Instant { Instant::now() }

    #[test]
    fn first_frame_asks_user() {
        let gate = AuthGate::new(Duration::from_secs(600));
        assert!(matches!(gate.check(t0()), Decision::AskUser));
    }

    #[test]
    fn granted_allows_within_idle_window() {
        let mut gate = AuthGate::new(Duration::from_secs(600));
        let now = t0();
        gate.grant(now);
        assert!(matches!(gate.check(now + Duration::from_secs(599)), Decision::Allow));
    }

    #[test]
    fn idle_timeout_expires_grant_and_asks_again() {
        let mut gate = AuthGate::new(Duration::from_secs(600));
        let now = t0();
        gate.grant(now);
        assert!(matches!(gate.check(now + Duration::from_secs(601)), Decision::AskUser));
    }

    #[test]
    fn allow_refreshes_idle_clock() {
        let mut gate = AuthGate::new(Duration::from_secs(600));
        let now = t0();
        gate.grant(now);
        // 9 分钟后一次活动……
        let later = now + Duration::from_secs(540);
        assert!(matches!(gate.check(later), Decision::Allow));
        gate.touch(later);
        // ……再过 9 分钟仍在窗口内(滑动)
        assert!(matches!(gate.check(later + Duration::from_secs(540)), Decision::Allow));
    }

    #[test]
    fn deny_resets_to_idle() {
        let mut gate = AuthGate::new(Duration::from_secs(600));
        let now = t0();
        gate.grant(now);
        gate.deny();
        assert!(matches!(gate.check(now), Decision::AskUser));
    }
}
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo test -p cloudcode-client auth_gate`
Expected: 编译失败(`AuthGate`/`Decision` 未定义)。

- [ ] **Step 3: 写实现**

```rust
/// What the relay should do with an inbound browser frame.
#[derive(Debug)]
pub enum Decision {
    /// Grant is live — forward the frame.
    Allow,
    /// No live grant — hold the frame and prompt the user.
    AskUser,
}

/// One grant per claude task, approximated by a sliding idle window
/// (per spec: the release predicate is idle-timeout only).
pub struct AuthGate {
    idle_timeout: Duration,
    granted_at: Option<Instant>,
    last_activity: Option<Instant>,
}

impl AuthGate {
    pub fn new(idle_timeout: Duration) -> Self {
        Self { idle_timeout, granted_at: None, last_activity: None }
    }

    pub fn check(&self, now: Instant) -> Decision {
        match self.last_activity.or(self.granted_at) {
            Some(last) if now.duration_since(last) <= self.idle_timeout => Decision::Allow,
            _ => Decision::AskUser,
        }
    }

    pub fn grant(&mut self, now: Instant) {
        self.granted_at = Some(now);
        self.last_activity = Some(now);
    }

    /// Record activity on an allowed frame (slides the idle window).
    pub fn touch(&mut self, now: Instant) {
        if self.granted_at.is_some() {
            self.last_activity = Some(now);
        }
    }

    pub fn deny(&mut self) {
        self.granted_at = None;
        self.last_activity = None;
    }
}
```

`main.rs` 加 `mod auth_gate;`。需要时按代码库 WIP 风格加 `#[allow(dead_code)]`(Task 3 消费)。

- [ ] **Step 4: 跑测试确认通过**

Run: `cargo test -p cloudcode-client auth_gate`
Expected: 5 个测试 PASS。

- [ ] **Step 5: 提交**

```bash
git add crates/client/src/auth_gate.rs crates/client/src/main.rs
git commit -m "feat(client): AuthGate session-grant state machine (idle-timeout predicate)"
```

---

## Task 2: relay 内联 ANSI 确认模态

**Files:**
- Modify: `crates/client/src/relay.rs`(新增 `prompt_browser_consent` 私有 async fn)

仿 `main.rs:543 show_pill()` 的纯 ANSI 直写。模态运行在 relay select! 的 text 臂内(`await` 期间其他臂自然暂停;PTY 输出堆在 channel 里,模态结束后照常冲刷 —— 这就是 spec"等待人工态阻塞自动化帧"的 M2 雏形)。

- [ ] **Step 1: 实现 prompt_browser_consent**

签名与行为(实现者按 show_pill 的真实绘制代码适配颜色/边框风格):

```rust
/// Draw an inline consent pill over the live PTY screen and block on a
/// y/n answer read from the raw stdin stream. The relay's other select!
/// arms are parked while this awaits — inbound PTY output buffers in its
/// channel and flushes after the modal closes.
/// Returns true = approved, false = denied (Esc / n / 10s of silence is
/// NOT auto-deny: wait indefinitely; the agent endpoint will time the
/// in-flight request out at 25s and claude sees a clean timeout error).
async fn prompt_browser_consent(bytes: &mut ByteRx) -> bool {
    let mut stdout = std::io::stdout();
    // BEL 唤起注意 + 保存光标
    let _ = stdout.write_all(b"\x07\x1b7");
    // 顶部第 2 行画一条醒目 pill(具体序列仿 show_pill: 定位/背景色/文案/复位)
    let _ = stdout.write_all(
        b"\x1b[2;1H\x1b[2K\x1b[30;43m  \xe4\xba\x91\xe7\xab\xaf\xe4\xbb\xbb\xe5\x8a\xa1\xe8\xaf\xb7\xe6\xb1\x82\xe6\x93\x8d\xe4\xbd\x9c\xe4\xbd\xa0\xe7\x9a\x84\xe6\xb5\x8f\xe8\xa7\x88\xe5\x99\xa8 \xe2\x80\x94 \xe5\x85\x81\xe8\xae\xb8? [y]\xe5\x85\x81\xe8\xae\xb8 / [n]\xe6\x8b\x92\xe7\xbb\x9d  \x1b[0m",
    );
    let _ = stdout.flush();
    let approved = loop {
        let Some(chunk) = bytes.recv().await else { break false };
        match chunk.iter().find(|b| matches!(b, b'y' | b'Y' | b'n' | b'N' | 0x1b)) {
            Some(b'y') | Some(b'Y') => break true,
            Some(b'n') | Some(b'N') | Some(0x1b) => break false,
            _ => continue, // 其他击键吞掉(不转发给 PTY,避免误输入)
        }
    };
    // 清掉 pill + 恢复光标
    let _ = stdout.write_all(b"\x1b[2;1H\x1b[2K\x1b8");
    let _ = stdout.flush();
    approved
}
```

实现要点(实现者必须遵守):
- **先读 `show_pill()` 真实代码**,绘制序列(定位行、清行、颜色、恢复)抄它的惯例,上面的序列是示意。
- 文案中文 UTF-8 字节已内嵌;若代码库习惯用 `format!`+`str`,改写为字符串字面量更可读 —— 按周围风格定。
- 模态期间击键**不得**转发 PTY(吞掉),y/n/Esc 之外忽略。
- 不设本地超时:挂多久都行,agent 端点 25s 会替 claude 解围(返回 timed out 错误),claude 侧表现干净。pill 留在屏上直到用户表态 —— 下一帧 BrowserRpc 到来会再次进 AskUser 弹窗。

- [ ] **Step 2: 编译**

Run: `cargo build -p cloudcode-client`
Expected: 绿(函数暂未被调用,允许 dead_code 或直接在 Task 3 一并接线后再提交也可 —— 实现者可把 Task 2/3 合并为一个提交序列,但测试节点要分开跑)。

- [ ] **Step 3: 提交**

```bash
git add crates/client/src/relay.rs
git commit -m "feat(client): inline ANSI consent pill for browser channel"
```

---

## Task 3: 授权门接线(relay BrowserRpc 臂)

**Files:**
- Modify: `crates/client/src/relay.rs`

- [ ] **Step 1: 状态接入**

在 relay_loop 现有 browser 状态旁(M1 加的 `let mut browser…` 处):

```rust
    let idle_timeout = std::time::Duration::from_secs(
        std::env::var("CC_BROWSER_IDLE_TIMEOUT_SECS").ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(600),
    );
    let mut gate = crate::auth_gate::AuthGate::new(idle_timeout);
```

- [ ] **Step 2: BrowserRpc 臂改造**

把 M1 的 `HubToClient::BrowserRpc { payload }` 臂改为先过门:

```rust
                        HubToClient::BrowserRpc { payload } => {
                            let now = std::time::Instant::now();
                            let allowed = match gate.check(now) {
                                crate::auth_gate::Decision::Allow => { gate.touch(now); true }
                                crate::auth_gate::Decision::AskUser => {
                                    if prompt_browser_consent(&mut bytes).await {
                                        gate.grant(std::time::Instant::now());
                                        true
                                    } else {
                                        gate.deny();
                                        false
                                    }
                                }
                            };
                            if !allowed {
                                browser = None; // 收掉子进程(若已起)
                                let _ = wire.out_tx
                                    .send(OutFrame::Text(ClientToHub::BrowserClosed {
                                        reason: Some("denied by user".to_string()),
                                    }))
                                    .await;
                            } else {
                                // —— 以下为 M1 原有的懒启动 + feed 逻辑,原样保留 ——
                            }
                        }
```

注意:`bytes` 在 select! 的另一臂被借用 —— `prompt_browser_consent(&mut bytes)` 发生在 text 臂内部,此时 `bytes.recv()` 臂并未持有借用(select! 各臂互斥执行),可编译;若 borrow checker 不接受,把 prompt 调用挪出 match 后以标志位驱动(实现者按编译结果调整,语义不变)。

- [ ] **Step 3: 编译 + 已有测试**

Run: `cargo test -p cloudcode-client && cargo build --workspace`
Expected: 全绿、零警告。

- [ ] **Step 4: 提交**

```bash
git add crates/client/src/relay.rs
git commit -m "feat(client): gate browser frames behind consent + idle-timeout grant"
```

---

## Task 4: agent↔hub 补 BrowserClosed 帧 + deny 全链路

**Files:**
- Modify: `crates/agent/src/tunnel.rs` + `crates/hub/src/tunnel.rs`(锁步)
- Modify: `crates/hub/src/pty_session.rs`(BrowserClosed 臂转发)
- Modify: `crates/agent/src/ws.rs`(分发)
- Modify: `crates/agent/src/mcp_endpoint.rs`(`fail_pending`)

- [ ] **Step 1: proto(两个 tunnel.rs 逐字锁步)**

`enum ServerMsg`(hub→agent)加:

```rust
    /// The client's browser channel is gone (user denied, subprocess
    /// died, or client teardown). The endpoint must fail this session's
    /// in-flight MCP requests so claude gets a clean JSON-RPC error
    /// instead of waiting out the 25s timeout.
    BrowserClosed {
        session_id: Uuid,
        #[serde(default)]
        reason: Option<String>,
    },
```

serde 往返测试加在 agent 侧 tunnel.rs 的 browser_tests(仿 M1 `browser_rpc_frames_roundtrip_byte_exact` 模式)。

- [ ] **Step 2: hub 转发**

`pty_session.rs` 的 `ClientToHub::BrowserClosed { reason }` 臂(M1 noop)改为:

```rust
        ClientToHub::BrowserClosed { reason } => {
            if let (Some(conn), Some(active)) =
                (ctx.selected_agent.as_ref(), ctx.active.as_ref())
            {
                let _ = conn
                    .send(ServerMsg::BrowserClosed {
                        session_id: active.session_id,
                        reason,
                    })
                    .await;
            }
            true
        }
```

- [ ] **Step 3: agent 端点 fail_pending**

`mcp_endpoint.rs` 加:

```rust
impl EndpointState {
    /// Fail every in-flight request for `session_id` with a JSON-RPC
    /// error (e.g. after the client denied or tore down the channel).
    pub fn fail_pending(&self, session_id: Uuid, reason: &str) {
        let keys: Vec<PendingKey> = self
            .pending
            .iter()
            .filter(|e| e.key().0 == session_id)
            .map(|e| e.key().clone())
            .collect();
        for key in keys {
            if let Some((k, tx)) = self.pending.remove(&key) {
                let _ = tx.send(jsonrpc_error(&k.1, -32002, reason));
            }
        }
    }
}
```

单测:注册两个 pending(同 session 不同 id)+ 一个异 session 的,`fail_pending` 后同 session 的两个 oneshot 收到 `-32002` 错误体、异 session 的不受影响。

`ws.rs` read_loop 在 BrowserRpc 臂旁加:

```rust
                Ok(ServerMsg::BrowserClosed { session_id, reason }) => {
                    state.mcp.fail_pending(
                        session_id,
                        reason.as_deref().unwrap_or("browser channel closed"),
                    );
                }
```

(agent 的 `PtyManager::handle` exhaustive match 需补 no-op 臂保持穷尽,同 M1 BrowserRpc 模式。)

- [ ] **Step 4: 测试 + 提交**

Run: `cargo test --workspace`
Expected: 全绿。

```bash
git add -A
git commit -m "feat: BrowserClosed propagates deny to agent, fails in-flight MCP requests"
```

---

## Task 5: 默认命令换真 playwright

**Files:**
- Modify: `crates/client/src/cc_browser.rs`

- [ ] **Step 1: 替换默认命令**

`m1_mcp_command()` 改名 `mcp_command()`(调用点同步),逻辑:

```rust
/// Resolve the browser MCP subprocess command.
/// Override entirely via `CC_BROWSER_MCP` (whitespace-separated).
/// Default (M2): pinned @playwright/mcp via npx, headless, with a
/// dedicated persistent profile under the client state dir.
/// Returns None when node/npx is unavailable.
pub fn mcp_command() -> Option<(String, Vec<String>)> {
    if let Ok(cmd) = std::env::var("CC_BROWSER_MCP") {
        let mut parts = cmd.split_whitespace().map(|s| s.to_string());
        let prog = parts.next()?;
        return Some((prog, parts.collect()));
    }
    which_node()?; // npx ships with node
    let profile = dirs::data_local_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join("cloudcode")
        .join("browser-profile");
    Some((
        "npx".to_string(),
        vec![
            "-y".to_string(),
            "@playwright/mcp@0.0.76".to_string(), // pin:防 npx 漂移;勿用无 scope 的 playwright-mcp(第三方)
            "--headless".to_string(),             // 默认 headed,必须显式
            "--user-data-dir".to_string(),
            profile.to_string_lossy().to_string(),
        ],
    ))
}
```

`dirs` 已是 workspace 依赖;client Cargo.toml 若未引则加 `dirs.workspace = true`。
echo 桩解析逻辑(`resolve_echo_stub`)保留但只在 `CC_BROWSER_MCP` 含 `echo-mcp.mjs` 时相关 —— 实际上桩现在只走 env 覆盖路径,`resolve_echo_stub` 若 dead 则随测试保留或内联进测试(实现者判断,保持 warning-clean)。

- [ ] **Step 2: 测试适配**

cc_browser 现有两个子进程测试继续用 echo 桩(显式路径,不依赖默认命令)—— 它们测的是管道而非 playwright。新增一个**忽略默认跑**的真 playwright 冒烟(标 `#[ignore]`,CI 不跑、本地手动 `cargo test -- --ignored` 验证):

```rust
    #[tokio::test]
    #[ignore = "spawns real @playwright/mcp via npx; run manually"]
    async fn playwright_mcp_initialize_roundtrips() {
        let Some((prog, args)) = mcp_command() else { return };
        let argrefs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
        let mut proc = McpProcess::spawn(&prog, &argrefs).expect("spawn playwright mcp");
        proc.feed(r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"test","version":"0"}}}"#).await.unwrap();
        let resp = tokio::time::timeout(std::time::Duration::from_secs(120), proc.next_frame())
            .await.expect("npx cold start within 2min").expect("a frame");
        assert!(resp.contains("Playwright"));
        proc.shutdown().await;
    }
```

- [ ] **Step 3: 测试 + 提交**

Run: `cargo test -p cloudcode-client`(ignored 不计)
Expected: 全绿。

```bash
git add crates/client/src/cc_browser.rs crates/client/Cargo.toml
git commit -m "feat(client): default browser MCP = pinned @playwright/mcp headless w/ persistent profile"
```

---

## Task 6: reattach 诊断 + 修复

**Files:**
- Modify: `crates/agent/src/pty.rs`(按诊断结果)
- Modify: e2e 冒烟文档

摸查显示 resume 命令行**确实带** `--mcp-config <同路径>` 且文件每次被覆写为新 token —— 所以 idle-reattach 理论上应当能通,M1 用户实测失败可能是当时 agent 重启导致(token 表清空)而非 reattach 本身。本任务先证实再动刀。

- [ ] **Step 1: 写一个可本地复现的诊断脚本/步骤**(不动产品代码)

在计划文档 / PR 描述里记录:开 session→关 client→重开同 workspace(不重启 agent)→ 看 claude `/mcp` 里 cc-browser 状态 + 新旧 token 对比(`cat <ws>/.cloudcode/mcp-browser.json` vs claude UI 里的 URL)。

- [ ] **Step 2: 按结果二选一**

- **若 idle-reattach 已通**:只剩 busy-reattach(旧 claude 进程仍活、内存旧 token)。最小修:open_session 的 swap 路径**不要急着 unregister 旧 token** —— 若 `(account,workspace)` 相同且 tmux 仍活(busy reattach),把旧 token **重新指到新 session_id**(`EndpointState` 加 `rebind(token, new_session_id)`),旧 claude 的旧 token 继续有效。实现 + 单测(rebind 后 `session_for` 返回新 sid)。
- **若 idle-reattach 也断**:实测找出断点(嫌疑:`eval "$RESUME_CMD" '"$@"'` 的引号展开丢参),修 wrapper 或 RESUME_CMD 拼接,加 wrapper 级注释说明。

- [ ] **Step 3: 测试 + 提交**

```bash
git add -A
git commit -m "fix(agent): keep browser token valid across reattach (per diagnosis)"
```

---

## Task 7: token 文件权限 0600

**Files:**
- Modify: `crates/agent/src/pty.rs`

- [ ] **Step 1:** 写 `mcp-browser.json` 后(unix)收权限:

```rust
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(
                &mcp_cfg_path,
                std::fs::Permissions::from_mode(0o600),
            );
        }
```

- [ ] **Step 2:** `cargo build -p cloudcode-agent` 绿;提交:

```bash
git add crates/agent/src/pty.rs
git commit -m "fix(agent): mcp-browser.json is 0600 (contains bearer token)"
```

---

## Task 8: M2 手动冒烟文档 + 收尾

**Files:**
- Create: `docs/superpowers/plans/2026-06-10-local-browser-m2-e2e-smoke.md`

- [ ] **Step 1:** 写 M2 冒烟步骤(沿 M1 文档结构,关键差异):
  - 前置:client 机器装 node;首次会 npx 下载 playwright/mcp + 需要浏览器二进制(`npx -y @playwright/mcp@0.0.76 install-browser` 或本机已有 Chrome);**首跑建议先手动预热一次 npx**(冷启动可超 25s 端点超时)。
  - 步骤:进 claude → 让 claude `browser_navigate` 到 example.com → **client 终端应响铃并弹黄色确认条** → 按 y → claude 收到页面快照;紧接着第二个操作(如 `browser_snapshot`)**不再弹窗**(grant 生效);按 n 的路径 → claude 收到 `-32002 denied` 错误;空闲 10 分钟后再操作 → 重新弹窗。
  - 已知限制:webterm 无浏览器通道;M3 才有 handoff/headed。
- [ ] **Step 2:** `cargo test --workspace` 全绿、`cargo build --workspace` 零警告。
- [ ] **Step 3:** 提交 + push:

```bash
git add docs/
git commit -m "docs: M2 e2e smoke procedure"
git push -u origin feature/local-browser-m2
```

---

## Self-Review 备忘

- spec 覆盖:授权门状态机(spec"授权门"节,放行谓词=仅空闲超时 ✓)、确认 UI(spec 定 TUI 模态+响铃,M2 用 relay 内联 ANSI 实现 —— 与 spec"复用 menu 模态"有出入,原因已摸实:relay 期间 ByteRx 独占、非 alt-screen,ratatui 模态不可行,内联 ANSI 是该约束下的正确形态)、deny 全链路(spec"授权失效映射"✓ 经新增 BrowserClosed 帧)、真浏览器(M2 主体 ✓)、M1 验证发现项(reattach、0600 ✓;webterm 显式出 M2 范围)。
- 类型一致:`AuthGate/Decision` 命名贯穿 Task 1/3;`mcp_command()` 重命名在 Task 5 内自洽;`fail_pending`+`-32002` 在 Task 4/8 一致。
- 风险给执行者:① Task 3 的 `&mut bytes` 双臂借用可能要按编译器调整结构(语义不变);② Task 5 npx 冷启动 vs 端点 25s 超时 —— 冒烟文档已写预热,如实测仍卡可在 Task 5 加"BrowserChannel 起后先自发 initialize 预热"优化(留给执行时判断);③ Task 6 是诊断驱动,两条修法都已给出。
