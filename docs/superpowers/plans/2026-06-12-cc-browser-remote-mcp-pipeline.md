# cc-browser 远程-MCP 透明管道(计划①/共二) 实现计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 打通一条 backend 无关的「远程 MCP」透明传输管道:agent 上的 claude 经进程内 localhost HTTP MCP 端点 → agent⇄hub 隧道 → hub 哑中继 → client⇄hub 连接 → client 端 MCP 宿主 → 任意 MCP-over-stdio 后端子进程,全程不透明 JSON-RPC 帧原样透传、按 `id` 配对、永不卡死。

**Architecture:** 三端各加一组镜像 `RemoteMcp` 帧(负载 = 原文 JSON-RPC 字符串,中途零解析零改写);agent 侧「远程-MCP proxy」把 claude 的 HTTP POST 阻塞翻译成隧道帧并按 JSON-RPC `id` 配对回包,hub 按会话绑定哑转发,client 侧 `McpHost` 拉起配置的后端子进程并桥接 stdio⇄隧道。注入面 = agent 拉 claude 时拼进程级 `--mcp-config` + `--strict-mcp-config` + 通用引导 system prompt,server 名固定 `cc-browser`。降级面 = 始终广告工具、无 client 时合成 JSON-RPC 错误、attach/detach 发 `notifications/tools/list_changed`、三档分层超时。

**Tech Stack:** Rust / tokio / axum(hub 与 agent 端点)/ tokio-tungstenite / serde_json(不透明负载用 `String` 原文)/ tmux+claude

---

## 对执行者的总约定(每个任务都适用)

- REPO 根 = 本文件所在仓库根。所有命令默认在仓库根执行。
- 当前分支 `feature/cc-browser`(= dev `8013bc6` + 一个 spec commit)。本计划全部提交落在该分支。
- 逐字移植源:`feature/local-browser` 分支(M1-M3)。本计划所有「移植」均指从该分支拷贝代码后按本文给出的**改名/精炼后版本**落盘——本文已把改造后的完整代码贴出,**不需要**再去读源分支;源分支仅供溯源对照。
- 三对镜像文件必须手工保持 lockstep,本仓没有共享 crate:
  - `crates/agent/src/tunnel.rs` ⇄ `crates/hub/src/tunnel.rs`(agent↔hub 协议)
  - `crates/client/src/proto.rs` ⇄ `crates/hub/src/pty_proto.rs`(client↔hub 协议)
- 每个 Task 结束时整个 workspace 必须可编译(`cargo build --workspace`)且全部测试通过(`cargo test --workspace`),然后 commit。
- 部分测试需要本机有 `node`(spawn `test-fixtures/echo-mcp.mjs` 桩)。这些测试在无 node 时自动 skip(测试内探测,M1-M3 惯例)。执行机器应装 node ≥ 18。
- commit message 一律以下面一行结尾(单独一行):
  `Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>`

## 决策记录

| # | 决策 | 理由 |
|---|------|------|
| D1 | **claude⇄proxy 传输 = 复用 M1-M3 的进程内 localhost HTTP MCP 端点**(`"type":"http"`,POST 阻塞式),不改 stdio | spec 开放问题 1 在此拍定。HTTP 一侧已有经实战验证的实现,且已填平最大的坑:claude 把 MCP POST 的**任何非 2xx** 当成"需要 OAuth 认证",触发探测瀑布并报误导性 `SDK auth failed: HTTP 404`——M1-M3 的 `mcp_endpoint.rs` 已固化「传输层错误一律 HTTP 200 + JSON-RPC error 对象」的约定,本计划逐字保留。stdio 方案(claude spawn 一个 proxy 子进程)帧边界更干净,但要新写进程模型、丢掉全部现成测试,收益不抵成本;仅在此记录,不实现。 |
| D2 | **丢授权门**:不移植 `crates/client/src/auth_gate.rs`,不移植 relay 里的 consent pill 接线,不设任何同意弹窗 | spec「隔离与安全」明确决策:监督由可见窗口本身承担(计划②的 headed Chrome)。M1-M3 的 AuthGate 状态机、`prompt_browser_consent`、idle 窗口整条线不随迁。 |
| D3 | **backend 无关命名**:帧/类型一律 `RemoteMcp*`,不出现 `Browser*`;帧携带 `server: String` 字段(claude 眼里的 MCP server 名),负载是不透明 `payload: String`(原文 JSON-RPC,免二次序列化);proxy/host 只读 `id`(配对)与 `method`/`params.name`(选超时档),不解析任何浏览器语义 | spec 核心目标 3。M1-M3 隧道帧实际用的就是 `String` 负载(spec 提到的 `Box<RawValue>` 是其端点内部手法,帧上从未用过),`String` 已满足「零反序列化透传」,沿用。`server` 字段是对 M1-M3 帧的前向扩展,为计划②(同一管道多 server / 换预设)留位;计划①固定填 `"cc-browser"`。 |
| D4 | **PROTOCOL_VERSION(agent↔hub)`"12"` → `"13"`** | 新帧类型横跨 agent/hub。注意:hub 的校验是 `agent_proto > hub_proto` 才拒(`crates/hub/src/ws_handler.rs:61-65`),即**旧 agent 可连新 hub**,不是严格相等 lockstep ⇒ 发布顺序必须 hub 先升、agent 后升;旧 agent(12)连新 hub(13)时,旧 agent 永不发 `RemoteMcp` 帧、对 `PtyOpen` 新字段靠 serde 忽略未知字段降级,链路安全。 |
| D5 | **PTY_PROTOCOL_VERSION(client↔hub)`"1"` → `"2"`,仅文档性** | 实情:该常量两侧都标 `#[allow(dead_code)]`,client 把它放进 `Hello.version`,但 **hub 从不校验**(`pty_session.rs` 里无任何比较)。跨版本安全不靠它,靠:① `Hello.remote_mcp_capable` 带 `#[serde(default)]`(旧 client/webterm 缺省 false);② hub/agent 只对宣称 capable 的 client 发 `RemoteMcp` 帧;③ 三端读循环对解析失败均为告警/跳过(agent `ws.rs:250` warn、client `wire.rs:95` continue)。bump 到 "2" 只为镜像文件的版本史可读,两侧必须同时改。 |
| D6 | **跨版本宽容策略**(spec「发布」节的待定项):新枚举变体全部**追加在枚举末尾**;新 struct 字段一律 `#[serde(default)]`;旧端收到未知 `type` 的帧 = 解析失败 = 按既有读循环行为跳过该帧(不断连);新端绝不向未宣称能力的旧对端发新帧 | 见 D4/D5。这使「新 hub + 旧 client」「新 hub + 旧 agent」组合都安全降级为"无远程 MCP 功能"。 |
| D7 | **丢 `tools/list` 能力过滤**,改为「始终广告 + 调用时错误 + `list_changed`」 | spec 降级模型 ①②③。M1-M3 没有过滤逻辑本体(它根本不在无 client 时注入),本计划的差异点是:注入**不再**以 client capability 为前提(Phase E 翻转),无 client 时由 proxy 权威应答 `initialize`/`tools/list`(静态表,缺省空)并对其余请求合成 `-32004` 错误。 |
| D8 | **`notifications/tools/list_changed` 经 streamable-HTTP 的 GET SSE 流下发**(M1-M3 对 GET 回 405,无此能力,Phase E 新建);**claude 是否消费该通知属未验证假设**,Phase E 末有手动验证清单;若不消费,降级为错误文案引导(`-32004` 文案已含"连接后重试"指引),不阻塞合并 | spec 开放问题 4。 |
| D9 | **后端命令来源(计划①)= 环境变量 `CC_REMOTE_MCP_BACKEND`**(空白分隔,首段为程序,余为 argv),不新增 client TOML 配置段 | spec 组件 6 的 `[browser]` 配置段(enabled/headed/backend 默认 dev-browser)整体属计划②。`McpHost` 构造函数接 `(String, Vec<String>)` 命令参数,与来源解耦——②把 `[browser].backend`/内置 dev-browser 灌进同一参数即可。M1-M3 的 `CC_BROWSER_MCP` 同型先例。 |
| D10 | **agent 侧功能开关 = agent.toml `[remote_mcp]` 段**:`enabled`(默认 true)、`port`(默认 7110,localhost 监听)、`tools_manifest`(可选,JSON 数组文件路径,Phase E 静态工具表) | spec 组件 2/7 的实现级落点。M1-M3 用顶层 `mcp_port`,本计划收进独立段,②直接复用。 |
| D11 | **注入铁律**:进程级 `--mcp-config <每会话临时文件>` + `--strict-mcp-config`,临时文件 = 工作区 `.cloudcode/mcp-remote.json`(0600);**绝不写全局 `~/.claude.json`、绝不调用 `claude mcp add`**;引导 prompt 经 `--append-system-prompt` 注入,通用措辞(说"网页浏览等本地能力经 `cc-browser` MCP server",不写死任何工具名);注入只对 `tool_name == "claude"` 生效(M1-M3 未做此门控,会把 claude 专属 flag 喂给 codex 等其他工具,本计划修正) | spec「隔离与安全」+ 本计划精炼要求。 |
| D12 | **工作区稳定 token 机制整体移植**(M1-M3 `workspace_tokens`):token = `Uuid::simple()` 32 hex,按 (account, workspace) 记忆、每次 open 对新 session_id 重注册(覆盖式)、agent 重启时从 `mcp-remote.json` 自愈回采(格式校验防走私)、仅 workspace delete/reset 时注销 | 这是传输正确性机制不是浏览器语义:hub 每次 OpenSession(含 reattach)都铸新 session_id,而 tmux 里的 claude 比 hub 会话长寿,token 必须以工作区为生命周期。 |
| D13 | **错误码分配**(沿 M1-M3 续编):`-32000` 请求超时;`-32001` token 未注册;`-32002` 通道拆除(fail_pending);`-32003` 保留给计划②(M3 的"用户拒绝接管");`-32004` 调用时无可用 client/后端(始终广告的兜底错误) | spec「错误处理与降级」错误码待定项在此拍定。 |
| D14 | **分层超时三档原样保留**:~600s(`tools/call` 且工具名 ∈ `LONG_CALL_TOOLS`,目前仅 `request_handoff`,①不提供该工具、常量留给②)/ ~120s(其余 `tools/call`)/ ~25s(握手、元数据、垃圾;低于 claude 自身 ~30s MCP 连接超时,保证我们的错误先到) | spec 降级 ④。机制通用(method 感知),`LONG_CALL_TOOLS` 是数据不是浏览器代码。 |
| D15 | **集成测试位置**:agent/client 均为纯 bin crate(无 lib target),`tests/` 目录**无法 import crate 内部模块**;故端到端/集成测试按 M1-M3 先例放各模块内 `#[cfg(test)] mod`(M1-M3 的真 HTTP 端到端测试就在 `mcp_endpoint.rs` 模块内)。桩后端 = `test-fixtures/echo-mcp.mjs`(node,无则 skip) | 对"集成测试放 `tests/`"惯例的仓库实情修正。 |
| D16 | **冷启动握手归属**(spec 开放问题 3):client **不在线**时 claude 的 `initialize` 由 agent proxy 权威应答(回显请求的 protocolVersion,声明 `tools.listChanged: true`);client **在线**时握手端到端透传、由 client 宿主缓存重放(M1-M3 机制)。两者的衔接缝——claude 冷启动后 client 才上线、宿主握手缓存为空——由宿主**合成**一份自有握手(id `"cc-host-init"`,塞进缓存走既有重放-吞响应路径)补平 | 不补这个缝,后端会收到没有 `initialize` 前导的 `tools/call` 而报"未初始化"。 |
| D17 | manifest 内容、dev-browser 分发(npx vs vendored)、托管 Chrome、「请用户接管」工具、`[browser]` 配置段 → **全部计划②**;计划①只留通用接口(`server` 字段、`LONG_CALL_TOOLS`、`tools_manifest` 静态表机制、`-32003` 码位) | 计划①/②边界。 |

## File Structure

```
(均相对仓库根;「镜像」= 必须与对侧文件手工 lockstep)

crates/agent/src/tunnel.rs          修改  agent↔hub 协议(agent 侧):PROTOCOL_VERSION 13;
                                          ClientMsg::RemoteMcp;ServerMsg::RemoteMcp/RemoteMcpClosed;
                                          ServerMsg::PtyOpen.remote_mcp_capable;serde 测试
crates/hub/src/tunnel.rs            修改  同上的 hub 侧镜像 + 同套测试
crates/hub/src/registry.rs          修改  classify():ClientMsg::RemoteMcp → Routing::Session;测试
crates/client/src/proto.rs          修改  client↔hub 协议(client 侧):PTY_PROTOCOL_VERSION "2";
                                          Hello.remote_mcp_capable;ClientToHub/HubToClient 的
                                          RemoteMcp/RemoteMcpClosed;serde 测试
crates/hub/src/pty_proto.rs         修改  同上的 hub 侧镜像 + 同套测试
crates/hub/src/pty_session.rs       修改  capability 协商(authenticate→ConnCtx→PtyOpen)+
                                          RemoteMcp 双向哑转发(纯映射函数 + 字节不变量测试)
crates/agent/src/pty.rs             修改  PtyOpen 解构传参;workspace 稳定 token + mcp-remote.json
                                          注入(--mcp-config/--strict-mcp-config/--append-system-prompt);
                                          PtyClose→detach 钩子;delete/reset 注销 token
crates/agent/src/mcp_proxy.rs       创建  远程-MCP proxy:McpProxy 状态机(token 路由/在飞配对/
                                          attach 跟踪/SSE 通知)、handle_post(200-错误体)、分层超时、
                                          mcp_config_json/claude_mcp_args、fallback 应答、serve(axum);
                                          全部单测 + 进程内 loopback 端到端测试(D15)
crates/agent/src/config.rs          修改  [remote_mcp] 配置段(enabled/port/tools_manifest)
crates/agent/src/main.rs            修改  AppState.mcp;构建 McpProxy(载入 manifest);spawn serve
crates/agent/src/ws.rs              修改  set_hub_sender;读循环拦截 ServerMsg::RemoteMcp(resolve)
                                          与 RemoteMcpClosed(fail_pending)
crates/agent/Cargo.toml             修改  [dependencies] 加 axum;[dev-dependencies] 加 reqwest
crates/client/src/mcp_host.rs       创建  通用 MCP 宿主:backend_command(env)、McpProcess(spawn/
                                          stdio 泵)、McpChannel(握手缓存+重放)、McpHost(惰性拉起/
                                          退避上限/deliver/shutdown/握手合成);全部单测(echo 桩)
crates/client/src/main.rs           修改  mod mcp_host;
crates/client/src/wire.rs           修改  Hello.remote_mcp_capable 真值
crates/client/src/relay.rs          修改  relay_loop 接线:HubToClient::RemoteMcp→host.deliver、
                                          host 出帧→ClientToHub::RemoteMcp、RemoteMcpClosed→shutdown
test-fixtures/echo-mcp.mjs          创建  MCP-over-stdio echo 桩(从 feature/local-browser 逐字移植)
```

不触碰(明确不在本计划内):`crates/client/src/auth_gate.rs`(不移植)、webterm 前端、`crates/daemon`、`install.sh`、任何 `~/.claude.json` 全局路径。

---

# Phase A — RemoteMcp 协议帧(三端 lockstep)

目标:四个枚举(`ClientMsg`/`ServerMsg`、`ClientToHub`/`HubToClient`)各获得镜像的 `RemoteMcp` 帧;agent↔hub 协议版本 12→13;client↔hub 镜像常量 1→2(文档性,见 D5)。本阶段**只加帧与最小穷尽性臂**,不接任何路由逻辑。阶段结束:`cargo test --workspace` 全绿。

### Task 1: agent↔hub 隧道帧 + PROTOCOL_VERSION 13

**Files:**
- Modify: `crates/agent/src/tunnel.rs`(L4 版本常量;L43-224 `ClientMsg`;L291-483 `ServerMsg`;文件末尾加测试)
- Modify: `crates/hub/src/tunnel.rs`(L4;L43 起 `ClientMsg`;L288 起 `ServerMsg`;文件末尾加测试)——与 agent 侧逐字镜像
- Modify: `crates/agent/src/pty.rs`(约 L510,`PtyManager` 内处理 `ServerMsg` 的 match:穷尽性空臂)
- Modify: `crates/hub/src/registry.rs`(L290-317 `classify()`:新变体路由臂;L320 起测试 mod 加一例)
- Test: 上述四文件内的 `#[cfg(test)]`

- [ ] **A1-1 写失败测试(agent 侧)**:在 `crates/agent/src/tunnel.rs` 文件**末尾**(现有 `RejectReason` 枚举之后)追加:

```rust
#[cfg(test)]
mod remote_mcp_tests {
    use super::*;

    #[test]
    fn remote_mcp_frames_roundtrip_byte_exact() {
        let sid = Uuid::new_v4();
        // 负载故意带乱序键:中继若做过任何反序列化-再序列化都可能重排,
        // 字节等值断言钉死「原文透传」不变量。
        let original = r#"{"jsonrpc":"2.0","id":1,"method":"tools/list","params":{"zebra":1,"alpha":2}}"#;

        let c = ClientMsg::RemoteMcp {
            session_id: sid,
            server: "cc-browser".to_string(),
            payload: original.to_string(),
        };
        let j = serde_json::to_string(&c).unwrap();
        assert!(j.contains("\"type\":\"remote_mcp\""), "tag mismatch: {j}");
        match serde_json::from_str::<ClientMsg>(&j).unwrap() {
            ClientMsg::RemoteMcp { session_id, server, payload } => {
                assert_eq!(session_id, sid);
                assert_eq!(server, "cc-browser");
                assert_eq!(payload, original);
            }
            _ => panic!("wrong variant"),
        }

        let s = ServerMsg::RemoteMcp {
            session_id: sid,
            server: "cc-browser".to_string(),
            payload: original.to_string(),
        };
        let j2 = serde_json::to_string(&s).unwrap();
        assert!(j2.contains("\"type\":\"remote_mcp\""), "tag mismatch: {j2}");
        match serde_json::from_str::<ServerMsg>(&j2).unwrap() {
            ServerMsg::RemoteMcp { session_id, server, payload } => {
                assert_eq!(session_id, sid);
                assert_eq!(server, "cc-browser");
                assert_eq!(payload, original);
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn remote_mcp_closed_roundtrips_and_reason_defaults() {
        let sid = Uuid::new_v4();
        let s = ServerMsg::RemoteMcpClosed {
            session_id: sid,
            server: "cc-browser".to_string(),
            reason: Some("backend died".to_string()),
        };
        let j = serde_json::to_string(&s).unwrap();
        assert!(j.contains("\"type\":\"remote_mcp_closed\""), "tag mismatch: {j}");
        match serde_json::from_str::<ServerMsg>(&j).unwrap() {
            ServerMsg::RemoteMcpClosed { session_id, server, reason } => {
                assert_eq!(session_id, sid);
                assert_eq!(server, "cc-browser");
                assert_eq!(reason.as_deref(), Some("backend died"));
            }
            _ => panic!("wrong variant"),
        }

        // 线上省略 reason → #[serde(default)] 解出 None(跨版本字节兼容)。
        let wire = format!(
            r#"{{"type":"remote_mcp_closed","session_id":"{sid}","server":"cc-browser"}}"#
        );
        match serde_json::from_str::<ServerMsg>(&wire).unwrap() {
            ServerMsg::RemoteMcpClosed { reason, .. } => assert_eq!(reason, None),
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn protocol_version_is_13() {
        assert_eq!(PROTOCOL_VERSION, "13");
    }
}
```

- [ ] **A1-2 跑测试确认失败**:`cargo test -p cloudcode-agent remote_mcp` —— 预期**编译失败**,错误形如 ``error[E0599]: no variant or associated item named `RemoteMcp` found for enum `ClientMsg` `` (以及 `ServerMsg` 同款)。这就是 TDD 的红灯;若它意外通过说明你改错了文件。
- [ ] **A1-3 最小实现(agent 侧)**:三处修改 `crates/agent/src/tunnel.rs`:

  ① L4 版本常量,改为:

```rust
pub const PROTOCOL_VERSION: &str = "13";
```

  ② `ClientMsg` 枚举(L43 起)的**右花括号(当前 L224)之前**追加:

```rust
    /// One opaque MCP JSON-RPC frame from the agent-side remote-MCP
    /// proxy (claude is the MCP client) toward the bound client's
    /// backend MCP subprocess. Backend-agnostic: `server` is the MCP
    /// server name claude sees (plan-1 always "cc-browser"), `payload`
    /// is raw JSON-RPC text, never parsed in transit. `session_id` is
    /// the hub routing key.
    RemoteMcp {
        session_id: Uuid,
        server: String,
        payload: String,
    },
```

  ③ `ServerMsg` 枚举(L291 起)的**右花括号(当前 L483)之前**追加:

```rust
    /// One opaque MCP JSON-RPC frame from the client's backend MCP
    /// subprocess back toward claude, routed by `session_id` (and
    /// `server`) to the matching in-flight proxy request. Payload is
    /// raw text, never parsed in transit.
    RemoteMcp {
        session_id: Uuid,
        server: String,
        payload: String,
    },

    /// The client's remote-MCP channel is gone (backend unavailable,
    /// subprocess died, or client teardown). The agent-side proxy must
    /// fail this session's in-flight MCP requests so claude gets a
    /// clean JSON-RPC error instead of waiting out a timeout.
    RemoteMcpClosed {
        session_id: Uuid,
        server: String,
        #[serde(default)]
        reason: Option<String>,
    },
```

- [ ] **A1-4 修复 agent 穷尽性**:`crates/agent/src/pty.rs` 中处理入站 `ServerMsg` 的 match(搜索锚点:`ServerMsg::Welcome { .. } | ServerMsg::Rejected { .. } | ServerMsg::Ping => {}`,约 L510),在该行**之前**插入:

```rust
            // Phase D 起这两类帧在 ws.rs 读循环里被拦截(resolve_response /
            // fail_pending),永远到不了 PtyManager;空臂仅为 match 穷尽性。
            ServerMsg::RemoteMcp { .. } => {}
            ServerMsg::RemoteMcpClosed { .. } => {}
```

- [ ] **A1-5 跑测试确认通过(agent)**:`cargo test -p cloudcode-agent remote_mcp` —— 预期 3 个测试 PASS(`remote_mcp_frames_roundtrip_byte_exact` / `remote_mcp_closed_roundtrips_and_reason_defaults` / `protocol_version_is_13`)。
- [ ] **A1-6 写失败测试(hub 侧镜像)**:在 `crates/hub/src/tunnel.rs` 文件末尾追加与 A1-1 **逐字相同**的 `#[cfg(test)] mod remote_mcp_tests`(hub/agent 两份 `tunnel.rs` 本就是手工镜像,测试也镜像,作 lockstep 守卫)。随后在 `crates/hub/src/registry.rs` 的现有 `mod tests`(L320 起)中追加:

```rust
    #[test]
    fn remote_mcp_classifies_as_session() {
        let sid = Uuid::new_v4();
        let frame = ClientMsg::RemoteMcp {
            session_id: sid,
            server: "cc-browser".to_string(),
            payload: "{}".to_string(),
        };
        match classify(&frame) {
            Routing::Session(got) => assert_eq!(got, sid),
            _ => panic!("RemoteMcp must route by session"),
        }
    }
```

- [ ] **A1-7 跑测试确认失败**:`cargo test -p cloudcode-hub remote_mcp` —— 预期编译失败,同 A1-2 的 E0599。
- [ ] **A1-8 最小实现(hub 侧)**:
  ① `crates/hub/src/tunnel.rs`:L4 改 `pub const PROTOCOL_VERSION: &str = "13";`;`ClientMsg`(L43 起)与 `ServerMsg`(L288 起)各自右花括号前,**逐字**加入 A1-3 ②③ 的变体(含注释)。
  ② `crates/hub/src/registry.rs` `classify()`(L290 起):把第一组 Session 路由臂

```rust
        ClientMsg::PtyOpened { session_id, .. }
        | ClientMsg::PtyClosed { session_id, .. }
        | ClientMsg::PtyError { session_id, .. }
        | ClientMsg::SplitPaneResult { session_id, .. } => Routing::Session(*session_id),
```

  改为:

```rust
        ClientMsg::PtyOpened { session_id, .. }
        | ClientMsg::PtyClosed { session_id, .. }
        | ClientMsg::PtyError { session_id, .. }
        | ClientMsg::SplitPaneResult { session_id, .. }
        | ClientMsg::RemoteMcp { session_id, .. } => Routing::Session(*session_id),
```

  (`classify` 没有通配臂,这一步同时满足穷尽性。hub 其余消费点已有通配:`pty_session.rs` 的 `PtyEventOut::Frame(_) => true` 兜底,本任务不触碰。)
- [ ] **A1-9 跑测试确认通过 + 全仓回归**:`cargo test -p cloudcode-hub remote_mcp` 预期 4 个 PASS(3 个 serde + 1 个 classify);再跑 `cargo test --workspace` 预期全绿(确认没有踩到其他 match 的穷尽性)。
- [ ] **A1-10 commit**:

```bash
git add crates/agent/src/tunnel.rs crates/hub/src/tunnel.rs crates/agent/src/pty.rs crates/hub/src/registry.rs
git commit -m "protocol: RemoteMcp frames on the agent<->hub tunnel; bump to 13

Mirrored ClientMsg::RemoteMcp + ServerMsg::RemoteMcp/RemoteMcpClosed in
both tunnel.rs copies. Opaque String payload, never parsed in transit;
server field reserves multi-backend routing for plan-2. classify()
routes RemoteMcp by session. PtyManager gets exhaustiveness-only no-op
arms (ws.rs intercepts land in Phase D).

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

### Task 2: client↔hub 帧 + Hello 能力位 + PTY_PROTOCOL_VERSION "2"

**Files:**
- Modify: `crates/client/src/proto.rs`(L7 常量;L11 起 `ClientToHub`,在 L93 `Close,` 前插入;L99 起 `HubToClient`,在 L155 `Ping,` 前插入;`Hello` 变体 L12-15;文件末尾测试)
- Modify: `crates/hub/src/pty_proto.rs`(L8 常量;L12 起 `ClientToHub`,在 L119 `Close,` 前插入;L125 起 `HubToClient`,在 L181 `Ping,` 前插入;`Hello` 变体 L13-16;文件末尾测试)——与 client 侧逐字镜像
- Modify: `crates/client/src/wire.rs`(L44-47 `Hello` 构造点:补字段,暂填 `false`)
- Modify: `crates/hub/src/pty_session.rs`(`handle_client_frame` 的 match 末段,搜索锚点 `ClientToHub::Close => false,`:临时哑臂保穷尽性)
- Test: `crates/client/src/proto.rs`、`crates/hub/src/pty_proto.rs` 内 `#[cfg(test)]`

- [ ] **A2-1 写失败测试(client 侧)**:在 `crates/client/src/proto.rs` 文件末尾追加:

```rust
#[cfg(test)]
mod remote_mcp_tests {
    use super::*;

    #[test]
    fn remote_mcp_both_directions_roundtrip_byte_exact() {
        let original = r#"{"jsonrpc":"2.0","id":1,"method":"tools/list","params":{"zebra":1,"alpha":2}}"#;
        let c = ClientToHub::RemoteMcp {
            server: "cc-browser".to_string(),
            payload: original.to_string(),
        };
        let j = serde_json::to_string(&c).unwrap();
        assert!(j.contains("\"type\":\"remote_mcp\""), "tag mismatch: {j}");
        match serde_json::from_str::<ClientToHub>(&j).unwrap() {
            ClientToHub::RemoteMcp { server, payload } => {
                assert_eq!(server, "cc-browser");
                assert_eq!(payload, original);
            }
            _ => panic!("wrong variant"),
        }

        let h = HubToClient::RemoteMcp {
            server: "cc-browser".to_string(),
            payload: original.to_string(),
        };
        let j2 = serde_json::to_string(&h).unwrap();
        assert!(j2.contains("\"type\":\"remote_mcp\""), "tag mismatch: {j2}");
        match serde_json::from_str::<HubToClient>(&j2).unwrap() {
            HubToClient::RemoteMcp { server, payload } => {
                assert_eq!(server, "cc-browser");
                assert_eq!(payload, original);
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn remote_mcp_closed_roundtrips_and_defaults() {
        let c = ClientToHub::RemoteMcpClosed {
            server: "cc-browser".to_string(),
            reason: Some("backend unavailable".to_string()),
        };
        let j = serde_json::to_string(&c).unwrap();
        assert!(j.contains("\"type\":\"remote_mcp_closed\""), "tag mismatch: {j}");
        match serde_json::from_str::<ClientToHub>(&j).unwrap() {
            ClientToHub::RemoteMcpClosed { server, reason } => {
                assert_eq!(server, "cc-browser");
                assert_eq!(reason.as_deref(), Some("backend unavailable"));
            }
            _ => panic!("wrong variant"),
        }
        // 线上省略 reason → None
        let from_wire: HubToClient =
            serde_json::from_str(r#"{"type":"remote_mcp_closed","server":"cc-browser"}"#).unwrap();
        match from_wire {
            HubToClient::RemoteMcpClosed { server, reason } => {
                assert_eq!(server, "cc-browser");
                assert_eq!(reason, None);
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn hello_without_capability_defaults_false() {
        // 旧 client / webterm SPA 的 Hello 没有该字段 → 必须解析成功且为 false。
        let j = r#"{"type":"hello","token":"t","version":"1"}"#;
        match serde_json::from_str::<ClientToHub>(j).unwrap() {
            ClientToHub::Hello { remote_mcp_capable, .. } => assert!(!remote_mcp_capable),
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn pty_protocol_version_is_2() {
        assert_eq!(PTY_PROTOCOL_VERSION, "2");
    }
}
```

- [ ] **A2-2 跑测试确认失败**:`cargo test -p cloudcode-client remote_mcp` —— 预期编译失败:``no variant or associated item named `RemoteMcp` found for enum `ClientToHub` ``。
- [ ] **A2-3 最小实现(client 侧)**:修改 `crates/client/src/proto.rs`:

  ① L7 常量改为(注释一并更新,固化 D5 实情):

```rust
/// 文档性镜像版本(本常量两侧均 #[allow(dead_code)],hub 不校验;
/// 跨版本安全靠 Hello.remote_mcp_capable 缺省 false + 读循环对未知帧
/// 容忍跳过)。与 crates/hub/src/pty_proto.rs 同步改动。
#[allow(dead_code)]
pub const PTY_PROTOCOL_VERSION: &str = "2";
```

  (原文件 L6 已有 `#[allow(dead_code)]`,保留一份即可,勿重复。)

  ② `Hello` 变体(L12 起)在 `version: String,` 之后追加:

```rust
        /// 本 client 能否承载远程-MCP 后端子进程(配置了后端命令)。
        /// 缺省 false:旧 client / webterm SPA 不发该字段,hub→agent
        /// 链路便绝不向其转发 RemoteMcp 帧(决策 D5/D6)。
        #[serde(default)]
        remote_mcp_capable: bool,
```

  ③ `ClientToHub` 枚举中,`Close,`(改后约 L100)之前插入:

```rust
    /// In-session:client 侧后端 MCP 子进程回向 claude 的一帧不透明
    /// JSON-RPC。hub 打上当前活动会话的 session_id 转发给绑定 agent
    ///(ServerMsg::RemoteMcp)。负载中途零解析。
    RemoteMcp {
        server: String,
        payload: String,
    },
    /// In-session:client 拆除其远程-MCP 通道(后端不可用 / 子进程
    /// 死亡 / 收摊)。agent 据此立刻 fail 该会话在飞请求。
    RemoteMcpClosed {
        server: String,
        #[serde(default)]
        reason: Option<String>,
    },
```

  ④ `HubToClient` 枚举中,`Ping,`(改后约 L170)之前插入:

```rust
    /// claude(经 agent proxy)指向 client 侧后端 MCP 子进程的一帧
    /// 不透明 JSON-RPC。负载中途零解析。
    RemoteMcp {
        server: String,
        payload: String,
    },
    /// hub/agent 侧拆除远程-MCP 通道;client 应停掉后端子进程
    ///(保留握手缓存,见 Phase C)。
    RemoteMcpClosed {
        server: String,
        #[serde(default)]
        reason: Option<String>,
    },
```

  ⑤ `crates/client/src/wire.rs` L44-47 的 `Hello` 构造改为:

```rust
    let hello = ClientToHub::Hello {
        token: token.to_string(),
        version: PTY_PROTOCOL_VERSION.into(),
        // Task 8(Phase C)接真值 mcp_host::backend_command().is_some();
        // 在宿主模块存在前恒 false,行为与旧 client 一致。
        remote_mcp_capable: false,
    };
```

- [ ] **A2-4 跑测试确认通过(client)**:`cargo test -p cloudcode-client remote_mcp` —— 预期 4 个 PASS。(client 其余 `HubToClient` 消费点——`relay.rs:233`、`main.rs:514`、`menu.rs` 各 match、`wire.rs:103`——均有通配臂,已核实不破穷尽性。)
- [ ] **A2-5 写失败测试(hub 侧镜像)**:在 `crates/hub/src/pty_proto.rs` 文件末尾追加与 A2-1 **逐字相同**的 `#[cfg(test)] mod remote_mcp_tests`。
- [ ] **A2-6 跑测试确认失败**:`cargo test -p cloudcode-hub pty_proto` —— 预期编译失败(E0599 同上)。
- [ ] **A2-7 最小实现(hub 侧)**:
  ① `crates/hub/src/pty_proto.rs`:L8 常量改 `"2"`(带 A2-3 ① 同款注释);`Hello`(L13 起)、`ClientToHub`(在 `Close,` 前)、`HubToClient`(在 `Ping,` 前)与 client 侧**逐字镜像**(A2-3 ②③④)。
  ② `crates/hub/src/pty_session.rs` `handle_client_frame` 的 match(搜索锚点 `ClientToHub::Close => false,`),在该行**之前**插入临时哑臂:

```rust
        // 临时哑臂:Task 5(Phase B)替换为按会话绑定的真实转发。
        // client 侧要到 Phase C 才会发出这两类帧,此前丢弃无影响。
        ClientToHub::RemoteMcp { .. } | ClientToHub::RemoteMcpClosed { .. } => true,
```

  (`authenticate` 解构 `Hello` 用的是 `Ok(ClientToHub::Hello { token, .. })`,带 `..`,新字段不破编译;能力位接线在 Task 4。)
- [ ] **A2-8 跑测试确认通过 + 全仓回归**:`cargo test -p cloudcode-hub pty_proto` 预期 4 个 PASS;`cargo test --workspace` 全绿。
- [ ] **A2-9 commit**:

```bash
git add crates/client/src/proto.rs crates/hub/src/pty_proto.rs crates/client/src/wire.rs crates/hub/src/pty_session.rs
git commit -m "protocol: RemoteMcp frames on the client<->hub leg + Hello capability bit

Mirrored ClientToHub/HubToClient RemoteMcp + RemoteMcpClosed in proto.rs
and pty_proto.rs. Hello.remote_mcp_capable defaults false so old
clients and the webterm SPA degrade to no-remote-MCP. Informational
PTY_PROTOCOL_VERSION 1 -> 2 (hub never enforces it; documented).
Temporary no-op hub arms until Phase B wires routing.

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

# Phase B — hub 中继路由(capability 协商 + 双向哑转发)

目标:hub 把 client 宣称的能力位带进 `PtyOpen` 交给 agent;并把 `RemoteMcp`/`RemoteMcpClosed` 帧按会话绑定双向原样转发——hub 是哑管道,`server` 与 `payload` 必须**字节不动**地穿过。阶段结束:Task 2 的临时哑臂被真实转发替换,全仓测试绿。

### Task 3: capability 协商(Hello → ConnCtx → PtyOpen → agent)

**Files:**
- Modify: `crates/agent/src/tunnel.rs`(`ServerMsg::PtyOpen` 变体 L304-334:`env` 字段后加字段;测试 mod 追加一例)
- Modify: `crates/hub/src/tunnel.rs`(`PtyOpen` 镜像位置,`env` 字段约 L328;测试 mod 追加一例)
- Modify: `crates/hub/src/pty_session.rs`(`handle_socket` 约 L71-95;`ConnCtx` 结构体约 L190;`authenticate` 约 L251-340;`PtyOpen` 发送点约 L845-855)
- Modify: `crates/agent/src/pty.rs`(`handle()` 的 `PtyOpen` 解构约 L186-205;`open_session` 签名约 L515-528 与函数体)
- Test: 两份 `tunnel.rs` 的 `mod remote_mcp_tests`

- [ ] **B3-1 写失败测试**:在 `crates/agent/src/tunnel.rs` 的 `mod remote_mcp_tests`(Task 1 创建)内追加:

```rust
    #[test]
    fn pty_open_without_capability_defaults_false() {
        // 构造一个带 capability 的 PtyOpen,序列化后把新字段从 JSON 里
        // 删掉,再反序列化 —— 模拟旧 hub 发来的帧,必须降级为 false。
        let msg = ServerMsg::PtyOpen {
            session_id: Uuid::new_v4(),
            account: "acct".to_string(),
            workspace: "ws".to_string(),
            cols: 80,
            rows: 24,
            claude_args: vec![],
            sandbox: false,
            sandbox_mode: None,
            tool: None,
            env: std::collections::HashMap::new(),
            remote_mcp_capable: true,
        };
        let mut v: serde_json::Value =
            serde_json::from_str(&serde_json::to_string(&msg).unwrap()).unwrap();
        v.as_object_mut().unwrap().remove("remote_mcp_capable");
        match serde_json::from_str::<ServerMsg>(&v.to_string()).unwrap() {
            ServerMsg::PtyOpen { remote_mcp_capable, .. } => assert!(!remote_mcp_capable),
            _ => panic!("wrong variant"),
        }
    }
```

  同时在 `crates/hub/src/tunnel.rs` 的 `mod remote_mcp_tests` 内追加**逐字相同**的测试。
- [ ] **B3-2 跑测试确认失败**:`cargo test -p cloudcode-agent pty_open_without_capability` —— 预期编译失败:``error[E0560]: variant `ServerMsg::PtyOpen` has no field named `remote_mcp_capable` ``。
- [ ] **B3-3 最小实现(协议两镜像)**:在 `crates/agent/src/tunnel.rs` 与 `crates/hub/src/tunnel.rs` 的 `ServerMsg::PtyOpen` 变体中,`env: std::collections::HashMap<String, String>,` 字段(agent 侧 L333、hub 侧 L328)之后追加:

```rust
        /// Whether the bound client can host a remote-MCP backend
        /// subprocess (it has a backend command configured). Captured
        /// from `Hello.remote_mcp_capable`. `#[serde(default)]` so a
        /// pre-negotiation hub degrades to "not capable". Phase D gates
        /// MCP injection on it; Phase E flips injection to always-on
        /// and uses this only for attach/list_changed tracking.
        #[serde(default)]
        remote_mcp_capable: bool,
```

- [ ] **B3-4 修复 hub 编译(构造点 + 管道穿透)**:修改 `crates/hub/src/pty_session.rs` 四处:

  ① `ConnCtx` 结构体(约 L190,`active: Option<ActiveSession>,` 之后)加字段:

```rust
    /// client 在 `Hello.remote_mcp_capable` 宣称的能力位,连接建立时
    /// 捕获,随每次 `ServerMsg::PtyOpen` 转交 agent。
    remote_mcp_capable: bool,
```

  ② `authenticate` 返回类型 `Option<String>` 改为 `Option<(String, bool)>`,并:
  - Hello 解析处(搜索锚点 `Ok(ClientToHub::Hello { token, .. }) => token,`)改为:

```rust
    let (token, remote_mcp_capable) = match hello {
        Ok(Some(Ok(Message::Text(s)))) => match serde_json::from_str::<ClientToHub>(&s) {
            Ok(ClientToHub::Hello {
                token,
                remote_mcp_capable,
                ..
            }) => (token, remote_mcp_capable),
```

    (match 其余分支原样保留;只把绑定从单值换成元组。)
  - cookie 认证路径的 `return Some(account_name);` 改为 `return Some((account_name, remote_mcp_capable));`
  - token 认证路径的 `Some(name)` 改为 `Some((name, remote_mcp_capable))`

  ③ `handle_socket` 中调用点(约 L74)改为:

```rust
    let (account_name, remote_mcp_capable) =
        match authenticate(&state, &mut sink, &mut stream, pre_auth).await {
            Some(a) => a,
            None => return,
        };
```

  并在其下 `ConnCtx` 初始化(`active: None,` 之后)加一行 `remote_mcp_capable,`。

  ④ `PtyOpen` 发送点(约 L845-855,`env,` 字段之后)加一行:

```rust
                    remote_mcp_capable: ctx.remote_mcp_capable,
```

- [ ] **B3-5 修复 agent 编译(解构 + 透传)**:修改 `crates/agent/src/pty.rs` 两处:

  ① `handle()` 的 `PtyOpen` 解构(约 L186)与调用:在解构字段表 `env,` 之后加 `remote_mcp_capable,`,在 `open_session(...)` 实参表 `env,` 之后(`tx` 之前)加 `remote_mcp_capable,`。
  ② `open_session` 签名(约 L515):在 `env: HashMap<String, String>,` 之后、`tx: mpsc::Sender<OutFrame>,` 之前加形参 `remote_mcp_capable: bool,`;并在函数体 `let cwd = std::fs::canonicalize(&cwd_raw).unwrap_or(cwd_raw);` 之后加:

```rust
        // capability 在 Phase D 用于 MCP 注入、Phase E 用于 attach 标记;
        // 此处先行记录,排障时可直接看到协商结果。
        tracing::debug!(%session_id, remote_mcp_capable, "open_session: client capability");
```

- [ ] **B3-6 跑测试确认通过**:`cargo test -p cloudcode-agent pty_open_without_capability && cargo test -p cloudcode-hub pty_open_without_capability` 各 1 个 PASS;`cargo test --workspace` 全绿。
- [ ] **B3-7 commit**:

```bash
git add crates/agent/src/tunnel.rs crates/hub/src/tunnel.rs crates/hub/src/pty_session.rs crates/agent/src/pty.rs
git commit -m "hub: negotiate remote_mcp_capable from client Hello into PtyOpen

authenticate() now returns (account, capability); ConnCtx carries it and
every PtyOpen forwards it to the agent. Old hubs omit the field and the
agent's serde default degrades to not-capable (no MCP traffic ever sent
toward an incapable client). Agent threads it into open_session for
Phase D/E use.

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

### Task 4: RemoteMcp 双向哑转发(替换临时哑臂)

**Files:**
- Modify: `crates/hub/src/pty_session.rs`(`handle_client_frame` 内 Task 2 的临时哑臂;`handle_agent_event` 约 L1467-1521;新增 3 个纯映射函数 + 测试 mod)
- Test: `crates/hub/src/pty_session.rs` 内 `#[cfg(test)] mod remote_mcp_relay_tests`

设计说明:`handle_client_frame` 需要 `ConnCtx`(内含 `Arc<AppState>`,挂着 db/audit/registry),无法轻量单测;因此把**映射本身**抽成纯函数钉死「字节不动」不变量,match 臂只剩一行调用 + 既有的 `conn.send` / `send_client` 管道(它们已被现有代码路径覆盖)。

- [ ] **B4-1 写失败测试**:在 `crates/hub/src/pty_session.rs` 文件**末尾**追加:

```rust
#[cfg(test)]
mod remote_mcp_relay_tests {
    use super::*;

    // 故意包含:乱序键、unicode、转义引号、内嵌换行 —— 任何"解析后重组"
    // 的实现都会在至少一处露馅。
    const TRICKY: &str = r#"{"jsonrpc":"2.0","id":"αβ\"esc\"","method":"tools/call","params":{"zebra":1,"alpha":{"text":"line1\nline2"}}}"#;

    #[test]
    fn to_agent_mapping_is_byte_identical() {
        let sid = Uuid::new_v4();
        match remote_mcp_to_agent(sid, "cc-browser".to_string(), TRICKY.to_string()) {
            ServerMsg::RemoteMcp { session_id, server, payload } => {
                assert_eq!(session_id, sid);
                assert_eq!(server, "cc-browser");
                assert_eq!(payload, TRICKY, "hub must not rewrite the payload");
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn closed_to_agent_keeps_server_and_reason() {
        let sid = Uuid::new_v4();
        match remote_mcp_closed_to_agent(sid, "cc-browser".to_string(), Some("bye".to_string())) {
            ServerMsg::RemoteMcpClosed { session_id, server, reason } => {
                assert_eq!(session_id, sid);
                assert_eq!(server, "cc-browser");
                assert_eq!(reason.as_deref(), Some("bye"));
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn to_client_mapping_is_byte_identical() {
        match remote_mcp_to_client("cc-browser".to_string(), TRICKY.to_string()) {
            HubToClient::RemoteMcp { server, payload } => {
                assert_eq!(server, "cc-browser");
                assert_eq!(payload, TRICKY, "hub must not rewrite the payload");
            }
            _ => panic!("wrong variant"),
        }
    }
}
```

- [ ] **B4-2 跑测试确认失败**:`cargo test -p cloudcode-hub remote_mcp_relay` —— 预期编译失败:``error[E0425]: cannot find function `remote_mcp_to_agent` in this scope``。
- [ ] **B4-3 最小实现**:三处修改 `crates/hub/src/pty_session.rs`:

  ① 在 `handle_agent_event` 函数(约 L1467)**之前**插入纯映射函数:

```rust
/// 远程-MCP 中继的纯映射(单测钉死不变量):hub 是哑管道,`server` 与
/// `payload` 必须字节不动地穿过 —— 任何改写都会破坏 claude⇄后端的
/// 端到端 MCP 握手与 id 配对。
fn remote_mcp_to_agent(session_id: Uuid, server: String, payload: String) -> ServerMsg {
    ServerMsg::RemoteMcp { session_id, server, payload }
}

fn remote_mcp_closed_to_agent(
    session_id: Uuid,
    server: String,
    reason: Option<String>,
) -> ServerMsg {
    ServerMsg::RemoteMcpClosed { session_id, server, reason }
}

fn remote_mcp_to_client(server: String, payload: String) -> HubToClient {
    HubToClient::RemoteMcp { server, payload }
}
```

  ② 把 Task 2 留下的临时哑臂(`ClientToHub::RemoteMcp { .. } | ClientToHub::RemoteMcpClosed { .. } => true,`)**整体替换**为:

```rust
        ClientToHub::RemoteMcp { server, payload } => {
            // 仅在「已绑定 agent + 有活动会话」时转发;否则静默丢弃
            // (会话都没开,agent 侧不可能有在飞请求等这帧)。
            if let (Some(conn), Some(active)) =
                (ctx.selected_agent.as_ref(), ctx.active.as_ref())
            {
                let _ = conn
                    .send(remote_mcp_to_agent(active.session_id, server, payload))
                    .await;
            }
            true
        }
        ClientToHub::RemoteMcpClosed { server, reason } => {
            tracing::debug!(%server, ?reason, "client closed remote-MCP channel; forwarding to agent");
            if let (Some(conn), Some(active)) =
                (ctx.selected_agent.as_ref(), ctx.active.as_ref())
            {
                let _ = conn
                    .send(remote_mcp_closed_to_agent(active.session_id, server, reason))
                    .await;
            }
            true
        }
```

  ③ `handle_agent_event` 中,在兜底臂 `PtyEventOut::Frame(_) => true,`(约 L1520)**之前**插入(顺序关键:放在兜底臂后面会被它吞掉、帧静默丢失且无编译报错):

```rust
        PtyEventOut::Frame(ClientMsg::RemoteMcp { server, payload, .. }) => {
            // session_id 在出 registry 路由时已消费(classify→Session),
            // client 连接本身就等价于会话身份,故只下发 server+payload。
            let _ = send_client(sink, &remote_mcp_to_client(server, payload)).await;
            true
        }
```

- [ ] **B4-4 跑测试确认通过**:`cargo test -p cloudcode-hub remote_mcp_relay` —— 预期 3 个 PASS;`cargo test --workspace` 全绿。
- [ ] **B4-5 commit**:

```bash
git add crates/hub/src/pty_session.rs
git commit -m "hub: relay RemoteMcp frames both ways, byte-identical

ClientToHub::RemoteMcp/RemoteMcpClosed forward to the bound agent tagged
with the active session_id; agent-side ClientMsg::RemoteMcp (routed by
classify as Session) forwards down to the client. Mapping extracted as
pure functions with byte-identity tests pinning the dumb-relay
invariant. Explicit arm sits before the Frame(_) catch-all on purpose.

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

# Phase C — client `McpHost`(桩后端)

目标:client 侧通用 MCP 宿主——拉起配置的后端子进程、桥 stdio⇄隧道帧、握手缓存重放、退避重启;并接进 relay 循环与 Hello 能力位。移植源 = M1-M3 `cc_browser.rs` + relay 接线,**通用化**(类型不带 Browser 字样)、**去授权门**(无 consent pill、无 AuthGate)、**去 M3 浏览器专属逻辑**(无 handoff、无 last_url、无 tools/list 改写、无 login-hint)。后端在计划①里就是「任意命令」,测试用 echo 桩。

### Task 5: echo 桩 + `mcp_host.rs` 地基(McpProcess / backend_command)

**Files:**
- Create: `test-fixtures/echo-mcp.mjs`(从 `feature/local-browser` 逐字移植)
- Create: `crates/client/src/mcp_host.rs`
- Modify: `crates/client/src/main.rs`(L1-7 模块声明区:加 `mod mcp_host;`)
- Test: `crates/client/src/mcp_host.rs` 内 `#[cfg(test)] mod tests`

- [ ] **C5-1 创建桩**:新建 `test-fixtures/echo-mcp.mjs`(目录不存在则一并创建),内容逐字如下:

```js
// test-fixtures/echo-mcp.mjs
// Minimal MCP-over-stdio echo stub for browser-pipe testing (M1).
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

- [ ] **C5-2 写失败测试**:新建 `crates/client/src/mcp_host.rs`,**先只写**模块头与测试(实现留空着不写,让测试编译失败):

```rust
//! 通用 MCP 宿主(client 侧):拉起配置的 MCP-over-stdio 后端子进程,
//! 把不透明 JSON-RPC 帧(原文行)泵进/泵出。backend 无关:本模块不
//! 认识任何具体工具语义,只做 spawn / stdio 泵 / 握手缓存重放 / 退避
//! 重启。移植自 feature/local-browser:crates/client/src/cc_browser.rs,
//! 通用化并剥离授权门(决策 D2/D3)。

use std::sync::{Arc, Mutex};

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::mpsc;

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
```

  并在 `crates/client/src/main.rs` 的模块声明区(`mod menu;` 与 `mod mouse_filter;` 之间)加一行:

```rust
mod mcp_host;
```

- [ ] **C5-3 跑测试确认失败**:`cargo test -p cloudcode-client mcp_host` —— 预期编译失败:``error[E0425]: cannot find function `parse_backend` in this scope`` 与 ``cannot find struct... `McpProcess` ``。
- [ ] **C5-4 最小实现**:在 `crates/client/src/mcp_host.rs` 的 `use` 区之后、`#[cfg(test)]` 之前插入:

```rust
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
```

  (`json_id`/`json_method` 本任务先随地基落盘,Task 6 的重放与缓存逻辑使用;若编译器对它们报 dead_code 警告,属预期,Task 6 即消除。`Arc`/`Mutex`/`json_*` 在 Task 6 前未被实现引用,如 `cargo build` 因 unused import 报警告亦属预期,不要删 use。)
- [ ] **C5-5 跑测试确认通过**:`cargo test -p cloudcode-client mcp_host` —— 预期 2 个 PASS(node 缺席时 `echo_stub_roundtrips_tools_list` 静默直过)。`cargo test --workspace` 全绿。
- [ ] **C5-6 commit**:

```bash
git add test-fixtures/echo-mcp.mjs crates/client/src/mcp_host.rs crates/client/src/main.rs
git commit -m "client: mcp_host foundation — backend command + stdio MCP subprocess

Ports McpProcess (spawn / line-framed feed / next_frame / shutdown)
from feature/local-browser cc_browser.rs, generalized: backend command
comes from CC_REMOTE_MCP_BACKEND (whitespace-split), no node probe, no
browser defaults (plan-2 adds the dev-browser preset). Adds the
echo-mcp.mjs stdio stub fixture for pipeline tests.

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

### Task 6: `McpChannel`——stdio 泵 + 握手缓存 + 重放重生

**Files:**
- Modify: `crates/client/src/mcp_host.rs`(`json_method` 之后追加实现;`mod tests` 追加测试)
- Test: 同文件 `mod tests`

- [ ] **C6-1 写失败测试**:在 `crates/client/src/mcp_host.rs` 的 `mod tests` 末尾追加:

```rust
    #[tokio::test]
    async fn channel_pumps_frames_both_ways() {
        if !node_available() {
            return;
        }
        let fixture = concat!(env!("CARGO_MANIFEST_DIR"), "/../../test-fixtures/echo-mcp.mjs");
        let (out_tx, mut out_rx) = mpsc::channel(8);
        let chan = McpChannel::start(
            "node",
            &[fixture.to_string()],
            out_tx,
            Arc::new(Mutex::new(Vec::new())),
        )
        .expect("start channel");
        chan.feed(r#"{"jsonrpc":"2.0","id":7,"method":"tools/list"}"#.to_string())
            .unwrap();
        let got = out_rx.recv().await.expect("a response frame");
        assert!(got.contains("echo"));
    }

    /// 重生必须:(1) 重放缓存握手进新子进程,(2) 把重放出的 initialize
    /// 响应按 id 吞掉(claude 手里已有一份,重复帧会污染配对)。echo 桩
    /// 对任何带 id 的请求按 id 应答、忽略无 id 帧,恰好压测重放管线。
    #[tokio::test]
    async fn start_replayed_replays_handshake_and_swallows_response() {
        if !node_available() {
            return;
        }
        let fixture = concat!(env!("CARGO_MANIFEST_DIR"), "/../../test-fixtures/echo-mcp.mjs");
        let args = vec![fixture.to_string()];
        let (out_tx, mut out_rx) = mpsc::channel(8);
        let cache = Arc::new(Mutex::new(Vec::new()));
        let chan = McpChannel::start("node", &args, out_tx.clone(), cache.clone()).expect("start");

        // 正常握手:initialize 响应到 out_rx,两帧入缓存。
        chan.feed(
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"t","version":"0"}}}"#
                .to_string(),
        )
        .unwrap();
        let init_resp = out_rx.recv().await.expect("initialize response");
        assert!(init_resp.contains("serverInfo"));
        chan.feed(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#.to_string())
            .unwrap();
        assert_eq!(cache.lock().unwrap().len(), 2);

        // 收摊后重生:握手重放,重放出的 initialize 响应(id 1)必须被吞。
        drop(chan);
        let chan = McpChannel::start_replayed("node", &args, out_tx.clone(), cache.clone())
            .await
            .expect("start_replayed");

        chan.feed(r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#.to_string())
            .unwrap();
        let next = tokio::time::timeout(std::time::Duration::from_secs(10), out_rx.recv())
            .await
            .expect("a frame within 10s")
            .expect("channel alive");
        // 重生后的第一帧必须是 tools/list 响应,不是重复的 initialize 响应。
        assert!(next.contains(r#""id":2"#), "expected tools/list response, got: {next}");
        assert!(!next.contains("serverInfo"), "duplicate initialize response leaked: {next}");
        assert!(next.contains("echo"));
        assert!(out_rx.try_recv().is_err(), "nothing else may be queued in between");
    }

    /// 握手缓存由宿主拥有、仅共享进通道:通道收摊(RemoteMcpClosed /
    /// 后端崩溃)不得丢缓存;重放路径把同一帧再喂回 feed 不得增长缓存。
    #[tokio::test]
    async fn shared_cache_survives_channel_drop_and_dedups() {
        if !node_available() {
            return;
        }
        let fixture = concat!(env!("CARGO_MANIFEST_DIR"), "/../../test-fixtures/echo-mcp.mjs");
        let (out_tx, mut out_rx) = mpsc::channel(8);
        let cache = Arc::new(Mutex::new(Vec::new()));
        let chan = McpChannel::start("node", &[fixture.to_string()], out_tx, cache.clone())
            .expect("start channel");
        let init = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#;
        chan.feed(init.to_string()).unwrap();
        let _ = out_rx.recv().await.expect("initialize response");
        assert_eq!(cache.lock().unwrap().len(), 1);
        chan.feed(init.to_string()).unwrap();
        assert_eq!(cache.lock().unwrap().len(), 1, "replayed frame must not grow cache");
        drop(chan);
        assert_eq!(cache.lock().unwrap().len(), 1, "cache outlives the channel");
    }
```

- [ ] **C6-2 跑测试确认失败**:`cargo test -p cloudcode-client mcp_host` —— 预期编译失败:``cannot find struct, variant or union type `McpChannel` ``。
- [ ] **C6-3 最小实现**:在 `crates/client/src/mcp_host.rs` 的 `json_method` 之后插入:

```rust
/// 一条在跑的后端 MCP 通道:pump 任务独占子进程,把子进程每帧输出
/// 转发到 `out_tx`;`feed` 把入站帧排队写给子进程。
///
/// 通道同时缓存途经 `feed` 的 MCP 握手帧(`initialize` 请求 +
/// `notifications/initialized`):claude 在一条活连接上绝不重发握手,
/// 后端(重)拉起时必须先重放缓存,在跑的 claude 会话才能无感续接。
/// 缓存由 `McpHost` 拥有、仅共享进每条通道(通道收摊不丢缓存)。
///
/// 与 M1-M3 `BrowserChannel` 的差异:去掉 `done_rx`(它只服务于 M3
/// headed/headless 切换时的有界等待,计划①无该路径;②如需再引入)。
pub struct McpChannel {
    in_tx: mpsc::Sender<String>,
    handshake: Arc<Mutex<Vec<String>>>,
}

impl McpChannel {
    /// 冷启动:直接 spawn 并接泵。缓存为空时用这个(真正首启,握手帧
    /// 正在路上,会经 `feed` 自然入缓存)。
    pub fn start(
        program: &str,
        args: &[String],
        out_tx: mpsc::Sender<String>,
        handshake: Arc<Mutex<Vec<String>>>,
    ) -> std::io::Result<Self> {
        tracing::info!(program, ?args, "starting MCP backend subprocess");
        let proc = McpProcess::spawn(program, args)?;
        Ok(Self::from_process(proc, out_tx, handshake))
    }

    /// 重生:spawn 新子进程,先把缓存握手帧重放进去(重放出的
    /// initialize 响应按 id 吞掉 —— claude 手里已有一份),再接泵。
    /// 重放期间冒出的无关帧(如 server 主动通知)照常转发 out_tx。
    pub async fn start_replayed(
        program: &str,
        args: &[String],
        out_tx: mpsc::Sender<String>,
        handshake: Arc<Mutex<Vec<String>>>,
    ) -> std::io::Result<Self> {
        tracing::info!(program, ?args, "restarting MCP backend subprocess (handshake replay)");
        let mut proc = McpProcess::spawn(program, args)?;
        let frames: Vec<String> = handshake.lock().expect("handshake mutex").clone();
        for frame in &frames {
            let init_id = if json_method(frame).as_deref() == Some("initialize") {
                json_id(frame)
            } else {
                None // notifications/initialized:无响应可等
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
                        break; // 吞掉:claude 已有自己的 initialize 响应
                    }
                    let _ = out_tx.send(resp).await;
                }
            }
        }
        Ok(Self::from_process(proc, out_tx, handshake))
    }

    fn from_process(
        mut proc: McpProcess,
        out_tx: mpsc::Sender<String>,
        handshake: Arc<Mutex<Vec<String>>>,
    ) -> Self {
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
                            None => break, // 子进程 EOF
                        }
                    }
                }
            }
            // 收尸 + 掉 stdin 管道;真正的后端按 stdin-close 约定异步退出。
            proc.shutdown().await;
        });
        Self { in_tx, handshake }
    }

    /// 非阻塞投递一帧。Err = 队列满或泵已死 —— 调用方应视为「通道死亡」
    /// 收摊(置 None),下一帧走惰性重生。
    pub fn feed(&self, frame: String) -> Result<(), ()> {
        self.maybe_cache_handshake(&frame);
        self.in_tx.try_send(frame).map_err(|_| ())
    }

    /// 缓存握手帧供重放。两帧齐(len>=2)后零解析开销;按帧等值去重
    /// (重放路径会把缓存帧再喂回 feed,不得增长缓存)。
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
}
```

- [ ] **C6-4 跑测试确认通过**:`cargo test -p cloudcode-client mcp_host` —— 预期 5 个 PASS(node 在场)。
- [ ] **C6-5 commit**:

```bash
git add crates/client/src/mcp_host.rs
git commit -m "client: McpChannel — stdio pump with handshake cache + replayed respawn

Ports BrowserChannel from cc_browser.rs, generalized to McpChannel.
start() for cold boot, start_replayed() respawns and replays the cached
initialize/initialized frames, swallowing the replayed initialize
response by id so claude never sees a duplicate. Cache is shared in and
owned by the caller so teardown never loses it.

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

### Task 7: `McpHost`——惰性拉起 + 退避上限 + deliver/shutdown

**Files:**
- Modify: `crates/client/src/mcp_host.rs`(`McpChannel` 实现之后追加;`mod tests` 追加测试)
- Test: 同文件 `mod tests`

- [ ] **C7-1 写失败测试**:在 `mod tests` 末尾追加:

```rust
    #[tokio::test]
    async fn host_lazy_spawns_and_roundtrips_via_echo_stub() {
        if !node_available() {
            return;
        }
        let fixture = concat!(env!("CARGO_MANIFEST_DIR"), "/../../test-fixtures/echo-mcp.mjs");
        let (out_tx, mut out_rx) = mpsc::channel(8);
        let mut host = McpHost::new(("node".to_string(), vec![fixture.to_string()]), out_tx);
        // 首帧触发惰性 spawn;echo 桩应答按 id 配对回来。
        host.deliver(
            r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"echo","arguments":{"text":"hi"}}}"#
                .to_string(),
        )
        .await
        .expect("deliver");
        let resp = out_rx.recv().await.expect("echo response");
        assert!(resp.contains(r#""id":3"#) && resp.contains("echo: hi"), "got: {resp}");
    }

    #[tokio::test]
    async fn host_spawn_failure_caps_then_cools_down() {
        // 不存在的程序:每次 deliver 都 spawn 失败;到上限后进入冷却,
        // 冷却中不再尝试 spawn、错误文案可区分(快速失败)。
        let (out_tx, _out_rx) = mpsc::channel(8);
        let mut host = McpHost::new(
            ("/nonexistent/cloudcode-test-backend".to_string(), vec![]),
            out_tx,
        );
        for _ in 0..MAX_CONSECUTIVE_SPAWN_FAILURES {
            let err = host
                .deliver(r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#.to_string())
                .await
                .expect_err("spawn must fail");
            assert!(err.to_string().contains("failed to start backend"), "got: {err}");
        }
        let err = host
            .deliver(r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#.to_string())
            .await
            .expect_err("must be cooling down");
        assert!(err.to_string().contains("cooldown"), "got: {err}");
    }

    #[tokio::test]
    async fn host_shutdown_keeps_handshake_cache_for_respawn() {
        if !node_available() {
            return;
        }
        let fixture = concat!(env!("CARGO_MANIFEST_DIR"), "/../../test-fixtures/echo-mcp.mjs");
        let (out_tx, mut out_rx) = mpsc::channel(8);
        let mut host = McpHost::new(("node".to_string(), vec![fixture.to_string()]), out_tx);
        host.deliver(
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"t","version":"0"}}}"#
                .to_string(),
        )
        .await
        .expect("deliver initialize");
        let init_resp = out_rx.recv().await.expect("initialize response");
        assert!(init_resp.contains("serverInfo"));
        host.deliver(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#.to_string())
            .await
            .expect("deliver initialized");

        // 模拟 HubToClient::RemoteMcpClosed:收摊。缓存必须健在。
        host.shutdown();
        assert_eq!(host.handshake.lock().unwrap().len(), 2);

        // 下一帧触发带重放的惰性重生:直接得到 tools/list 响应,且不
        // 泄漏重复的 initialize 响应。
        host.deliver(r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#.to_string())
            .await
            .expect("deliver after shutdown");
        let next = tokio::time::timeout(std::time::Duration::from_secs(10), out_rx.recv())
            .await
            .expect("frame within 10s")
            .expect("alive");
        assert!(next.contains(r#""id":2"#) && !next.contains("serverInfo"), "got: {next}");
    }
```

- [ ] **C7-2 跑测试确认失败**:`cargo test -p cloudcode-client mcp_host` —— 预期编译失败:``cannot find struct... `McpHost` `` / ``cannot find value `MAX_CONSECUTIVE_SPAWN_FAILURES` ``。
- [ ] **C7-3 最小实现**:在 `McpChannel` 实现之后插入:

```rust
/// 后端连续 spawn 失败的上限;达到后进入冷却,期间一律快速失败
/// (relay 回发 RemoteMcpClosed,agent 立刻把在飞请求转成 JSON-RPC
/// 错误,claude 毫秒级可见,绝不等满超时)。
const MAX_CONSECUTIVE_SPAWN_FAILURES: u32 = 3;
/// 冷却时长;到点后允许再试(计数清零重来)。
const SPAWN_COOLDOWN: std::time::Duration = std::time::Duration::from_secs(60);

/// 宿主投递失败 —— relay 据此回发 `ClientToHub::RemoteMcpClosed`。
#[derive(Debug)]
pub enum McpHostError {
    BackendUnavailable(String),
}

impl std::fmt::Display for McpHostError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            McpHostError::BackendUnavailable(why) => write!(f, "backend unavailable: {why}"),
        }
    }
}

/// 通用 MCP 宿主:一个插槽。惰性拉起后端子进程(首帧到达才 spawn)、
/// 桥 stdio⇄隧道、崩溃带退避重启(连续失败上限 + 冷却)、握手缓存重放。
pub struct McpHost {
    backend: (String, Vec<String>),
    chan: Option<McpChannel>,
    handshake: Arc<Mutex<Vec<String>>>,
    out_tx: mpsc::Sender<String>,
    consecutive_failures: u32,
    cooldown_until: Option<tokio::time::Instant>,
}

impl McpHost {
    pub fn new(backend: (String, Vec<String>), out_tx: mpsc::Sender<String>) -> Self {
        Self {
            backend,
            chan: None,
            handshake: Arc::new(Mutex::new(Vec::new())),
            out_tx,
            consecutive_failures: 0,
            cooldown_until: None,
        }
    }

    /// 投递一帧给后端;后端没在跑就先(按重放语义)拉起。
    /// Err = 后端不可用,调用方应回发 RemoteMcpClosed 快速失败。
    pub async fn deliver(&mut self, payload: String) -> Result<(), McpHostError> {
        if self.chan.is_none() {
            self.spawn_channel().await?;
        }
        let Some(chan) = self.chan.as_ref() else {
            return Err(McpHostError::BackendUnavailable("spawn failed".to_string()));
        };
        if chan.feed(payload).is_err() {
            // 泵死(子进程崩溃/EOF):收摊并计一次失败;下一帧惰性重生。
            self.chan = None;
            self.note_failure();
            return Err(McpHostError::BackendUnavailable(
                "backend subprocess died".to_string(),
            ));
        }
        // feed 成功视为后端活着:清零连续失败计数(上限只惩罚连续失败,
        // 偶发崩溃 + claude 主动重试 = 每次重试一次重生机会)。
        self.consecutive_failures = 0;
        Ok(())
    }

    /// 收摊当前后端(响应 HubToClient::RemoteMcpClosed)。握手缓存
    /// 保留,之后的惰性重生靠它重放续接。
    pub fn shutdown(&mut self) {
        self.chan = None; // drop → 泵退出 → kill_on_drop 收尸
    }

    fn note_failure(&mut self) {
        self.consecutive_failures += 1;
        if self.consecutive_failures >= MAX_CONSECUTIVE_SPAWN_FAILURES {
            self.cooldown_until = Some(tokio::time::Instant::now() + SPAWN_COOLDOWN);
        }
    }

    async fn spawn_channel(&mut self) -> Result<(), McpHostError> {
        if let Some(until) = self.cooldown_until {
            if tokio::time::Instant::now() < until {
                return Err(McpHostError::BackendUnavailable(
                    "backend restarting too fast; in cooldown".to_string(),
                ));
            }
            self.cooldown_until = None;
            self.consecutive_failures = 0;
        }
        let (prog, args) = self.backend.clone();
        let empty = self.handshake.lock().expect("handshake mutex").is_empty();
        let started = if empty {
            McpChannel::start(&prog, &args, self.out_tx.clone(), self.handshake.clone())
        } else {
            McpChannel::start_replayed(&prog, &args, self.out_tx.clone(), self.handshake.clone())
                .await
        };
        match started {
            Ok(ch) => {
                self.chan = Some(ch);
                Ok(())
            }
            Err(e) => {
                self.note_failure();
                tracing::warn!(
                    error = %e,
                    failures = self.consecutive_failures,
                    "failed to start MCP backend subprocess"
                );
                Err(McpHostError::BackendUnavailable(format!(
                    "failed to start backend: {e}"
                )))
            }
        }
    }
}
```

- [ ] **C7-4 跑测试确认通过**:`cargo test -p cloudcode-client mcp_host` —— 预期 8 个 PASS。`cargo test --workspace` 全绿。
- [ ] **C7-5 commit**:

```bash
git add crates/client/src/mcp_host.rs
git commit -m "client: McpHost — lazy spawn, capped backoff, deliver/shutdown

One-slot generic MCP host. First inbound frame lazily spawns the
backend (with handshake replay when the cache is non-empty); pump death
fails fast and respawns on the next frame; consecutive spawn failures
cap at 3 then enter a 60s cooldown so a broken backend command cannot
fork-bomb. shutdown() keeps the handshake cache for seamless respawn.

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

### Task 8: relay 接线 + Hello 能力位真值

**Files:**
- Modify: `crates/client/src/relay.rs`(`relay_loop` 约 L150-155 局部状态;`HubToClient` match 约 L218-233,在 `_ => {}` 前插臂;`tokio::select!` 约 L154-236,加一个 arm)
- Modify: `crates/client/src/wire.rs`(L44-49 `Hello` 构造:`remote_mcp_capable` 接真值)

说明:本任务是纯接线——全部行为逻辑都在已被 Task 5-7 测试覆盖的 `McpHost` 里;`relay_loop` 直接驱动真实终端 IO,无法在单测里实例化(M1-M3 同样未对其单测)。验证手段 = 编译 + clippy + 全仓既有测试 + Phase D 的端到端回路。

- [ ] **C8-1 relay 局部状态**:在 `crates/client/src/relay.rs` 的 `relay_loop` 内,`let (inject_tx, mut inject_rx) = mpsc::channel::<Vec<u8>>(16);`(L151)之后插入:

```rust
    // 远程-MCP 宿主(Phase C)。后端命令来自 CC_REMOTE_MCP_BACKEND
    // (决策 D9);未配置 → None,Hello 能力位为 false,hub/agent 不会
    // 给我们发 RemoteMcp 帧 —— 万一异常发来,走下方防御性快速失败臂。
    // 注意:host_out_tx 在本作用域常驻(host 内只持 clone),保证
    // host_out_rx.recv() 永不返回 None 而空转。
    let (host_out_tx, mut host_out_rx) = tokio::sync::mpsc::channel::<String>(64);
    let mut mcp_host: Option<crate::mcp_host::McpHost> = crate::mcp_host::backend_command()
        .map(|b| crate::mcp_host::McpHost::new(b, host_out_tx.clone()));
```

- [ ] **C8-2 入站帧分发**:同文件 `HubToClient` 的 match(L218 起),在兜底臂 `_ => {}`(L233)**之前**插入:

```rust
                    HubToClient::RemoteMcp { server, payload } => {
                        if server != crate::mcp_host::CC_BROWSER_SERVER {
                            // 计划①只有一个插槽;未知 server 名立即回
                            // Closed,agent 把该会话在飞请求快速失败。
                            let _ = wire
                                .out_tx
                                .send(OutFrame::Text(ClientToHub::RemoteMcpClosed {
                                    server,
                                    reason: Some("unknown remote-MCP server".to_string()),
                                }))
                                .await;
                        } else if let Some(host) = mcp_host.as_mut() {
                            if let Err(e) = host.deliver(payload).await {
                                let _ = wire
                                    .out_tx
                                    .send(OutFrame::Text(ClientToHub::RemoteMcpClosed {
                                        server,
                                        reason: Some(e.to_string()),
                                    }))
                                    .await;
                            }
                        } else {
                            // 能力位为 false 仍收到帧:防御性快速失败。
                            let _ = wire
                                .out_tx
                                .send(OutFrame::Text(ClientToHub::RemoteMcpClosed {
                                    server,
                                    reason: Some(
                                        "no MCP backend configured (set CC_REMOTE_MCP_BACKEND)"
                                            .to_string(),
                                    ),
                                }))
                                .await;
                        }
                    }
                    HubToClient::RemoteMcpClosed { .. } => {
                        if let Some(host) = mcp_host.as_mut() {
                            host.shutdown();
                        }
                    }
```

  (`OutFrame`、`ClientToHub`、`wire` 均已在 `relay_loop` 现有代码中使用,无需新增 import。)
- [ ] **C8-3 出站泵 select 臂**:同文件 `tokio::select!` 中,`_ = winch_tick(&mut winch) => {`(L236)这个 arm **之前**插入:

```rust
            out = host_out_rx.recv() => {
                // host_out_tx 常驻本作用域,recv 不会得 None;防御写法。
                if let Some(payload) = out {
                    let _ = wire
                        .out_tx
                        .send(OutFrame::Text(ClientToHub::RemoteMcp {
                            server: crate::mcp_host::CC_BROWSER_SERVER.to_string(),
                            payload,
                        }))
                        .await;
                }
            }
```

- [ ] **C8-4 Hello 真值**:`crates/client/src/wire.rs` 的 `Hello` 构造中,把 Task 2 留下的 `remote_mcp_capable: false,`(连同那两行注释)替换为:

```rust
        // 配置了后端命令 = 本机能承载远程-MCP 后端(决策 D9)。
        remote_mcp_capable: crate::mcp_host::backend_command().is_some(),
```

- [ ] **C8-5 验证**:依次跑并确认:
  - `cargo build -p cloudcode-client` —— 编译通过;
  - `cargo clippy -p cloudcode-client -- -D warnings` —— 无告警(若 clippy 报 Task 5 留下的 `#[allow(dead_code)]` 项已被使用,可顺手删掉对应 allow;`CC_BROWSER_SERVER` 与 `backend_command` 本任务起已被引用);
  - `cargo test --workspace` —— 全绿。
- [ ] **C8-6 commit**:

```bash
git add crates/client/src/relay.rs crates/client/src/wire.rs crates/client/src/mcp_host.rs
git commit -m "client: wire McpHost into relay_loop + announce capability in Hello

HubToClient::RemoteMcp routes to the single cc-browser slot
(host.deliver, lazy spawn); deliver errors and unknown server names
fast-fail back as ClientToHub::RemoteMcpClosed so the agent can fail
pending requests immediately. Host output pumps back as
ClientToHub::RemoteMcp. Hello.remote_mcp_capable is now
backend_command().is_some().

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

# Phase D — agent proxy + 注入(端到端)

目标:移植并通用化 M1-M3 的 `mcp_endpoint.rs` 为 `mcp_proxy.rs`(进程内 localhost HTTP、200-错误体、分层超时、token 路由、id 配对),接通 config/main/ws,向 claude 注入 `--mcp-config` + `--strict-mcp-config` + 通用引导 prompt,最后用「真 HTTP + 真 node 桩」的进程内 loopback 测试打穿全管道。本阶段注入仍以 client capability 为门(保守,行为与 M1-M3 同构);Phase E 翻转为始终注入。

### Task 9: `mcp_proxy.rs` 核心移植(McpProxy / handle_post / 分层超时 / token 工具)

**Files:**
- Modify: `crates/agent/Cargo.toml`(`[dependencies]` 区:加 axum)
- Create: `crates/agent/src/mcp_proxy.rs`
- Modify: `crates/agent/src/main.rs`(L1-11 模块声明区:加 `mod mcp_proxy;`)
- Test: `crates/agent/src/mcp_proxy.rs` 内 `#[cfg(test)] mod tests`

- [ ] **D9-1 依赖**:`crates/agent/Cargo.toml` 的 `[dependencies]` 中,`dashmap.workspace = true` 一行之后加:

```toml
axum.workspace = true
```

  (workspace 根 `Cargo.toml:22` 已有 `axum = { version = "0.7", features = ["ws", "multipart"] }`;`reqwest` 已是 agent 主依赖,测试可直接用,无需进 dev-dependencies。)
- [ ] **D9-2 写失败测试**:新建 `crates/agent/src/mcp_proxy.rs`,先写模块头 + 测试(实现留到 D9-4):

```rust
//! 远程-MCP proxy(agent 侧):进程内 localhost HTTP MCP 端点。
//! claude(MCP client)连到这里;帧经既有 agent<->hub ws 以
//! ClientMsg::RemoteMcp 隧道给绑定 client 的后端子进程。
//!
//! 传输:Streamable HTTP、POST 阻塞式。claude POST 一条 JSON-RPC 请求,
//! proxy 转发给 client 并**阻塞**到按 JSON-RPC `id` 配对的响应回来,把
//! 响应体作为 POST 响应返回;通知(无 `id`)转发后立刻 202 无体。
//!
//! proxy 是哑中继 —— 不实现 MCP 语义(握手、工具 schema 都在 claude 与
//! client 后端之间端到端流动),只:按 token→session_id 路由、按
//! (session, server, id) 配对、隧道不透明 JSON 文本、按 method 选超时档。
//!
//! 【铁坑,绝不回退】传输层故障(token 未注册、超时、通道拆除)对
//! JSON-RPC **请求**一律返回 HTTP 200 + JSON-RPC error 对象,绝不裸回
//! 非 2xx:claude 把 MCP POST 的任何非 2xx 当成「需要认证」,触发 OAuth
//! 探测瀑布并报误导性 `SDK auth failed: HTTP 404`(M1-M3 实测教训)。
//!
//! 移植自 feature/local-browser:crates/agent/src/mcp_endpoint.rs,
//! 通用化:Browser* → RemoteMcp*,帧带 server 字段,长档超时改为
//! LONG_CALL_TOOLS 名单驱动(决策 D3/D13/D14)。

use dashmap::DashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{mpsc, oneshot, RwLock};
use uuid::Uuid;

use crate::pty::OutFrame;
use crate::tunnel::ClientMsg;

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn unknown_token_request_gets_jsonrpc_error_not_404() {
        // 带 id 的请求打到未知 token:绝不裸回非 2xx(OAuth 误判坑),
        // 而是 HTTP 200 + JSON-RPC error(-32001)。
        let state = McpProxy::new();
        let out = handle_post(
            "nope",
            r#"{"jsonrpc":"2.0","id":1,"method":"x"}"#.to_string(),
            &state,
        )
        .await;
        match out {
            PostOutcome::Response(body) => {
                assert!(body.contains("\"error\""), "carries an error object: {body}");
                assert!(body.contains("-32001"), "unknown-token code: {body}");
                assert!(body.contains("\"id\":1"), "keyed to the request id: {body}");
                let v: serde_json::Value = serde_json::from_str(&body).expect("valid JSON");
                assert_eq!(v["jsonrpc"], "2.0");
            }
            _ => panic!("expected a Response carrying a JSON-RPC error"),
        }
    }

    #[tokio::test]
    async fn notification_to_unknown_token_is_accepted_not_404() {
        let state = McpProxy::new();
        let out = handle_post(
            "nope",
            r#"{"jsonrpc":"2.0","method":"notify"}"#.to_string(),
            &state,
        )
        .await;
        assert!(matches!(out, PostOutcome::Accepted));
    }

    #[test]
    fn jsonrpc_error_has_valid_shape() {
        let body = jsonrpc_error("1", -32000, "remote MCP request timed out");
        let v: serde_json::Value = serde_json::from_str(&body).expect("valid JSON");
        assert_eq!(v["jsonrpc"], "2.0");
        assert_eq!(v["id"], 1);
        assert_eq!(v["error"]["code"], -32000);
        assert_eq!(v["error"]["message"], "remote MCP request timed out");

        // 字符串 id 原样穿回(id_raw 已是带引号的规范键)。
        let body = jsonrpc_error("\"abc\"", -32001, "x");
        let v: serde_json::Value = serde_json::from_str(&body).expect("valid JSON");
        assert_eq!(v["id"], "abc");

        // 文案含 JSON 破坏字符也要保持合法(转义)。
        let body = jsonrpc_error("1", -1, "has \"quotes\" and \\ backslash");
        let v: serde_json::Value = serde_json::from_str(&body).expect("valid JSON");
        assert_eq!(v["error"]["message"], "has \"quotes\" and \\ backslash");
    }

    #[tokio::test]
    async fn notification_is_forwarded_and_accepted() {
        let state = McpProxy::new();
        let sid = Uuid::new_v4();
        state.register("t".into(), sid);
        let (hub_tx, mut hub_rx) = mpsc::channel(4);
        state.set_hub_sender(hub_tx).await;
        let out = handle_post(
            "t",
            r#"{"jsonrpc":"2.0","method":"notify"}"#.to_string(),
            &state,
        )
        .await;
        assert!(matches!(out, PostOutcome::Accepted));
        // 已转发,且帧上带固定 server 名。
        match hub_rx.recv().await.expect("forwarded") {
            OutFrame::Text(ClientMsg::RemoteMcp { session_id, server, .. }) => {
                assert_eq!(session_id, sid);
                assert_eq!(server, CC_BROWSER_SERVER);
            }
            _ => panic!("expected a RemoteMcp frame"),
        }
    }

    #[tokio::test]
    async fn request_blocks_then_resolves_on_matching_response() {
        let state = McpProxy::new();
        let sid = Uuid::new_v4();
        state.register("t".into(), sid);
        let (hub_tx, mut hub_rx) = mpsc::channel(4);
        state.set_hub_sender(hub_tx).await;

        let st2 = state.clone();
        let poster = tokio::spawn(async move {
            handle_post(
                "t",
                r#"{"jsonrpc":"2.0","id":42,"method":"tools/list"}"#.to_string(),
                &st2,
            )
            .await
        });

        match hub_rx.recv().await.expect("forwarded to hub") {
            OutFrame::Text(ClientMsg::RemoteMcp { session_id, .. }) => assert_eq!(session_id, sid),
            _ => panic!("expected a RemoteMcp frame"),
        }

        // 模拟 client 应答经 ws 回来(同 id 配对)。
        let resolved = state.resolve_response(
            sid,
            CC_BROWSER_SERVER,
            r#"{"jsonrpc":"2.0","id":42,"result":{"tools":[]}}"#.to_string(),
        );
        assert!(resolved);

        match poster.await.unwrap() {
            PostOutcome::Response(b) => assert!(b.contains("\"id\":42") && b.contains("tools")),
            _ => panic!("expected a Response"),
        }
    }

    #[tokio::test]
    async fn fail_pending_fails_one_session_and_leaves_other_intact() {
        let state = McpProxy::new();
        let sid_a = Uuid::new_v4();
        let sid_b = Uuid::new_v4();
        let srv = CC_BROWSER_SERVER.to_string();

        let (tx_a1, rx_a1) = oneshot::channel::<String>();
        let (tx_a2, rx_a2) = oneshot::channel::<String>();
        state.pending.insert((sid_a, srv.clone(), "1".to_string()), tx_a1);
        state.pending.insert((sid_a, srv.clone(), "2".to_string()), tx_a2);
        let (tx_b, _rx_b) = oneshot::channel::<String>();
        state.pending.insert((sid_b, srv.clone(), "3".to_string()), tx_b);

        state.fail_pending(sid_a, "backend unavailable");

        let body_a1 = rx_a1.await.expect("a1 resolved");
        assert!(body_a1.contains("-32002"), "expected -32002 in: {body_a1}");
        assert!(body_a1.contains("backend unavailable"), "reason in: {body_a1}");
        let body_a2 = rx_a2.await.expect("a2 resolved");
        assert!(body_a2.contains("-32002"), "expected -32002 in: {body_a2}");

        assert!(!state.pending.contains_key(&(sid_a, srv.clone(), "1".to_string())));
        assert!(!state.pending.contains_key(&(sid_a, srv.clone(), "2".to_string())));
        assert!(state.pending.contains_key(&(sid_b, srv, "3".to_string())));
    }

    #[test]
    fn timeout_for_is_method_and_tool_aware() {
        // 长档:LONG_CALL_TOOLS 名单内的 tools/call(②的人工接管)。
        assert_eq!(
            timeout_for(
                r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"request_handoff","arguments":{"reason":"login"}}}"#
            ),
            LONG_CALL_TIMEOUT
        );
        assert_eq!(LONG_CALL_TIMEOUT, Duration::from_secs(600));
        // 中档:其余 tools/call(首调可能触发后端拉起)。
        assert_eq!(
            timeout_for(
                r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"browser_navigate"}}"#
            ),
            CALL_TIMEOUT
        );
        assert_eq!(
            timeout_for(r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{}}"#),
            CALL_TIMEOUT
        );
        assert_eq!(CALL_TIMEOUT, Duration::from_secs(120));
        // 短档:握手/元数据/垃圾(低于 claude 自身 ~30s 连接超时)。
        assert_eq!(
            timeout_for(r#"{"jsonrpc":"2.0","id":1,"method":"initialize"}"#),
            REQUEST_TIMEOUT
        );
        assert_eq!(
            timeout_for(r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#),
            REQUEST_TIMEOUT
        );
        assert_eq!(timeout_for("not json"), REQUEST_TIMEOUT);
        assert_eq!(REQUEST_TIMEOUT, Duration::from_secs(25));
    }

    #[test]
    fn config_has_http_url_with_token_under_cc_browser() {
        let s = mcp_config_json(7110, "abc123");
        assert!(s.contains("\"cc-browser\""));
        assert!(s.contains("\"type\":\"http\""));
        assert!(s.contains("http://127.0.0.1:7110/mcp/abc123"));
        let _: serde_json::Value = serde_json::from_str(&s).unwrap();
    }

    #[test]
    fn claude_args_carry_strict_flag_and_guidance() {
        let args = claude_mcp_args(std::path::Path::new("/ws/.cloudcode/mcp-remote.json"));
        assert_eq!(
            args,
            vec![
                "--mcp-config".to_string(),
                "/ws/.cloudcode/mcp-remote.json".to_string(),
                "--strict-mcp-config".to_string(),
                "--append-system-prompt".to_string(),
                GUIDANCE_PROMPT.to_string(),
            ]
        );
        // 引导文案通用化:点名 server,不写死任何工具名(决策 D11)。
        assert!(GUIDANCE_PROMPT.contains("cc-browser"));
        assert!(!GUIDANCE_PROMPT.contains("browser_navigate"));
    }

    #[test]
    fn extract_token_roundtrip_and_garbage() {
        let json = mcp_config_json(7110, "abc123");
        assert_eq!(extract_token_from_config(&json), Some("abc123".to_string()));
        assert_eq!(extract_token_from_config("not json at all"), None);
        assert_eq!(extract_token_from_config(""), None);
        assert_eq!(extract_token_from_config(r#"{"other":"value"}"#), None);
        assert_eq!(
            extract_token_from_config(r#"{"mcpServers":{"cc-browser":{"type":"http"}}}"#),
            None
        );
        assert_eq!(
            extract_token_from_config(
                r#"{"mcpServers":{"cc-browser":{"url":"http://127.0.0.1:7110/mcp/"}}}"#
            ),
            None
        );
    }

    #[test]
    fn token_validation_accepts_minted_rejects_malformed() {
        let minted = Uuid::new_v4().simple().to_string();
        assert!(is_valid_token(&minted));
        assert!(is_valid_token("ABCDEF0123456789abcdef0123456789"));
        assert!(!is_valid_token(""));
        assert!(!is_valid_token("abc123"));
        assert!(!is_valid_token(&"a".repeat(31)));
        assert!(!is_valid_token(&"a".repeat(33)));
        assert!(!is_valid_token("g".repeat(32).as_str()));
        assert!(!is_valid_token("../../../../etc/passwd00000000000"));
    }

    #[test]
    fn register_overwrite_unregister_routing() {
        let st = McpProxy::new();
        let sid1 = Uuid::new_v4();
        let sid2 = Uuid::new_v4();
        let tok = "stable-workspace-token".to_string();
        st.register(tok.clone(), sid1);
        assert_eq!(st.session_for(&tok), Some(sid1));
        // reattach:同 token 对 hub 新铸的 session_id 重注册 = 覆盖改路由。
        st.register(tok.clone(), sid2);
        assert_eq!(st.session_for(&tok), Some(sid2));
        st.unregister(&tok);
        assert_eq!(st.session_for(&tok), None);
    }

    #[test]
    fn id_key_distinguishes_number_and_string() {
        assert_eq!(extract_id_key(r#"{"id":1}"#), Some("1".to_string()));
        assert_eq!(extract_id_key(r#"{"id":"a"}"#), Some("\"a\"".to_string()));
        assert_eq!(extract_id_key(r#"{"method":"x"}"#), None);
        assert_eq!(extract_id_key(r#"{"id":null}"#), None);
    }

    /// 绑 :0 拿一个空闲端口再放掉给 serve 重绑。存在极小 TOCTOU 窗口,
    /// 单测试进程内可忽略,远比写死端口稳。
    pub(super) fn free_port() -> u16 {
        let l = std::net::TcpListener::bind("127.0.0.1:0").expect("bind :0");
        l.local_addr().expect("local_addr").port()
    }

    /// 轮询 /healthz 直到 serve 绑定完成(连接拒绝则重试)。
    pub(super) async fn wait_healthz(client: &reqwest::Client, base: &str) -> String {
        for _ in 0..50 {
            match client.get(format!("{base}/healthz")).send().await {
                Ok(resp) => return resp.text().await.unwrap(),
                Err(_) => tokio::time::sleep(Duration::from_millis(20)).await,
            }
        }
        panic!("endpoint never came up on {base}");
    }

    /// 唯一走真 TCP + axum 路由的测试(其余直接调 handle_post)。
    #[tokio::test]
    async fn real_http_post_roundtrips_via_endpoint() {
        let state = McpProxy::new();
        let sid = Uuid::new_v4();
        let token = "tok-e2e";
        state.register(token.into(), sid);

        let (hub_tx, mut hub_rx) = mpsc::channel(4);
        state.set_hub_sender(hub_tx).await;

        let port = free_port();
        let serve_state = state.clone();
        tokio::spawn(async move {
            let _ = serve(serve_state, port).await;
        });

        // 模拟 client+hub:取走转发帧,按 id 喂回一条合成响应。
        let resp_state = state.clone();
        tokio::spawn(async move {
            if let Some(OutFrame::Text(ClientMsg::RemoteMcp { session_id, server, payload })) =
                hub_rx.recv().await
            {
                assert_eq!(session_id, sid);
                let id = extract_id_key(&payload).expect("request had an id");
                let body = format!(
                    r#"{{"jsonrpc":"2.0","id":{id},"result":{{"tools":[{{"name":"echo"}}]}}}}"#
                );
                assert!(resp_state.resolve_response(session_id, &server, body));
            }
        });

        let client = reqwest::Client::new();
        let base = format!("http://127.0.0.1:{port}");
        assert_eq!(wait_healthz(&client, &base).await, "ok");

        let resp = client
            .post(format!("{base}/mcp/{token}"))
            .body(r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#)
            .send()
            .await
            .expect("POST to endpoint");
        assert_eq!(resp.status(), reqwest::StatusCode::OK);
        let text = resp.text().await.unwrap();
        assert!(text.contains("\"id\":1"), "response keeps the request id: {text}");
        assert!(text.contains("echo"), "carries the simulated result: {text}");

        // 未知 token 的请求:HTTP 200 + JSON-RPC error,绝不 404。
        let unknown = client
            .post(format!("{base}/mcp/does-not-exist"))
            .body(r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#)
            .send()
            .await
            .expect("POST unknown token");
        assert_eq!(unknown.status(), reqwest::StatusCode::OK);
        let body = unknown.text().await.unwrap();
        assert!(body.contains("\"error\""), "JSON-RPC error body: {body}");
        assert!(body.contains("-32001"), "unknown-token code: {body}");
    }
}
```

  并在 `crates/agent/src/main.rs` 模块声明区(`mod jsonl;` 与 `mod name;` 之间)加:

```rust
mod mcp_proxy;
```

- [ ] **D9-3 跑测试确认失败**:`cargo test -p cloudcode-agent mcp_proxy` —— 预期编译失败:``cannot find struct... `McpProxy` ``、``cannot find function `handle_post` `` 等。
- [ ] **D9-4 完整实现**:在 `crates/agent/src/mcp_proxy.rs` 的 `use` 区与 `#[cfg(test)]` 之间插入(整块,无省略):

```rust
/// claude 眼里固定的 MCP server 名(计划①唯一插槽)。与 client 侧
/// `crates/client/src/mcp_host.rs::CC_BROWSER_SERVER` 手工 lockstep。
pub const CC_BROWSER_SERVER: &str = "cc-browser";

/// 注入给 claude 的通用引导(决策 D11):说明 cc-browser 的工具在用户
/// 本地机器执行、收到「未连接」错误时如何转告用户。不写死任何工具名
/// —— 工具表由后端运行时决定。
pub const GUIDANCE_PROMPT: &str = "The `cc-browser` MCP server provides tools (such as web \
browsing) that run on the USER'S LOCAL machine through the cloudcode CLI — not on this host. \
Prefer these tools when the user asks for anything involving their local browser or web pages. \
If a cc-browser tool call returns a 'not connected' style error, relay its instructions to the \
user (they need to open the cloudcode CLI on their local machine), then retry after they \
confirm.";

/// 在飞请求的配对键:(session_id, server 名, 规范化 JSON-RPC id)。
/// server 进键位是为计划②同会话多 server 时 id 互不冲突。
type PendingKey = (Uuid, String, String);

#[derive(Clone)]
pub struct McpProxy {
    /// claude 持有的 token → session_id 路由(注册即覆盖:工作区稳定
    /// token 在每次 reattach 重指向 hub 新铸的 session_id)。
    routes: Arc<DashMap<String, Uuid>>,
    /// 阻塞中的 POST,等响应按 (session, server, id) 配对。
    pending: Arc<DashMap<PendingKey, oneshot::Sender<String>>>,
    /// agent ws 起来后注入:让 proxy 能向 hub 发帧。
    to_hub: Arc<RwLock<Option<mpsc::Sender<OutFrame>>>>,
}

impl Default for McpProxy {
    fn default() -> Self {
        Self::new()
    }
}

impl McpProxy {
    pub fn new() -> Self {
        Self {
            routes: Arc::new(DashMap::new()),
            pending: Arc::new(DashMap::new()),
            to_hub: Arc::new(RwLock::new(None)),
        }
    }

    /// token → session 路由注册(会话打开时)。已知 token 重注册 =
    /// 覆盖改路由(reattach 语义,决策 D12)。
    pub fn register(&self, token: String, session_id: Uuid) {
        self.routes.insert(token, session_id);
    }

    pub fn unregister(&self, token: &str) {
        self.routes.remove(token);
    }

    pub fn session_for(&self, token: &str) -> Option<Uuid> {
        self.routes.get(token).map(|r| *r.value())
    }

    pub async fn set_hub_sender(&self, tx: mpsc::Sender<OutFrame>) {
        *self.to_hub.write().await = Some(tx);
    }

    async fn send_to_hub(&self, frame: OutFrame) {
        // 先 clone 出 sender、放掉读锁再 await send —— 否则并发重连在
        // set_hub_sender 拿写锁会被一个在飞 send 卡住。
        let tx = self.to_hub.read().await.as_ref().cloned();
        if let Some(tx) = tx {
            let _ = tx.send(frame).await;
        }
    }

    /// 把 `session_id` 的所有在飞请求以 JSON-RPC 错误(-32002)收尾
    /// (client 拆通道 / 后端崩溃 / client 掉线)。
    pub fn fail_pending(&self, session_id: Uuid, reason: &str) {
        let keys: Vec<PendingKey> = self
            .pending
            .iter()
            .filter(|e| e.key().0 == session_id)
            .map(|e| e.key().clone())
            .collect();
        for key in keys {
            if let Some((k, tx)) = self.pending.remove(&key) {
                let _ = tx.send(jsonrpc_error(&k.2, -32002, reason));
            }
        }
    }

    /// 把一条回程帧配对到阻塞中的 POST。ws.rs 收到
    /// ServerMsg::RemoteMcp 时调用。true = 配对成功;false = 无人等
    /// (如 server 主动通知,计划①丢弃 —— 与 M1-M3 一致)。
    pub fn resolve_response(&self, session_id: Uuid, server: &str, payload: String) -> bool {
        let id = extract_id_key(&payload);
        tracing::debug!(%session_id, server, has_id = id.is_some(), "remote MCP response from hub");
        let Some(id) = id else {
            return false;
        };
        if let Some((_, tx)) = self
            .pending
            .remove(&(session_id, server.to_string(), id))
        {
            return tx.send(payload).is_ok();
        }
        tracing::debug!(%session_id, "remote MCP response had no pending waiter");
        false
    }
}

/// 短档:握手/元数据/垃圾。低于 claude 自身 ~30s 的 MCP 连接超时,
/// 保证我们的 JSON-RPC 错误先于其客户端超时到达。
const REQUEST_TIMEOUT: Duration = Duration::from_secs(25);

/// 中档:tools/call(首调可能触发 client 侧后端拉起,放宽)。
const CALL_TIMEOUT: Duration = Duration::from_secs(120);

/// 长档:阻塞等真人操作的工具调用(分钟级)。
const LONG_CALL_TIMEOUT: Duration = Duration::from_secs(600);

/// 长档工具名单(数据,非浏览器代码):计划②的「请用户接管」工具在此
/// 登记;计划①不提供该工具,名单仅保证机制就绪(决策 D14)。
const LONG_CALL_TOOLS: &[&str] = &["request_handoff"];

/// method(+ 工具名)感知的三档超时选择。
fn timeout_for(body: &str) -> Duration {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(body) else {
        return REQUEST_TIMEOUT;
    };
    if v.get("method").and_then(|m| m.as_str()) != Some("tools/call") {
        return REQUEST_TIMEOUT;
    }
    let tool = v
        .get("params")
        .and_then(|p| p.get("name"))
        .and_then(|n| n.as_str());
    match tool {
        Some(t) if LONG_CALL_TOOLS.contains(&t) => LONG_CALL_TIMEOUT,
        _ => CALL_TIMEOUT,
    }
}

/// 按请求 id(extract_id_key 的规范键,如 `1` 或 `"abc"`)构造
/// JSON-RPC 错误响应体。
fn jsonrpc_error(id_raw: &str, code: i64, message: &str) -> String {
    format!(
        r#"{{"jsonrpc":"2.0","id":{id_raw},"error":{{"code":{code},"message":{msg}}}}}"#,
        msg = serde_json::to_string(message).unwrap_or_else(|_| "\"error\"".to_string())
    )
}

/// token 前 8 字符,日志用(不泄密)。
fn token_prefix(token: &str) -> &str {
    let end = token
        .char_indices()
        .nth(8)
        .map(|(i, _)| i)
        .unwrap_or(token.len());
    &token[..end]
}

/// 取 JSON-RPC `id` 的规范字符串键(数字→`1`,字符串→`"abc"`)。
/// 通知(无 id / id=null)与坏 JSON → None。
fn extract_id_key(body: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(body).ok()?;
    match v.get("id") {
        Some(serde_json::Value::Null) | None => None,
        Some(id) => Some(id.to_string()),
    }
}

/// 生成 claude 要加载的 `--mcp-config` JSON(Streamable HTTP 指向本
/// proxy)。server 名固定 cc-browser。
pub fn mcp_config_json(port: u16, token: &str) -> String {
    format!(
        r#"{{"mcpServers":{{"{CC_BROWSER_SERVER}":{{"type":"http","url":"http://127.0.0.1:{port}/mcp/{token}"}}}}}}"#
    )
}

/// 从先前写盘的 mcp-remote.json 把 token 捞回来:agent 重启后重新采用,
/// 而不是铸新(tmux 里幸存的 claude 内存里还持着旧 token)。
pub fn extract_token_from_config(json: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(json).ok()?;
    let url = v
        .get("mcpServers")?
        .get(CC_BROWSER_SERVER)?
        .get("url")?
        .as_str()?;
    let token = url.rsplit('/').next()?;
    if token.is_empty() {
        return None;
    }
    Some(token.to_string())
}

/// 合法工作区 token:恰 32 个 ASCII hex(`Uuid::new_v4().simple()`
/// 铸造格式)。守住 pty.rs 自愈采用路径,防止被篡改的配置把任意
/// (可猜)token 走私进路由表。
pub fn is_valid_token(token: &str) -> bool {
    token.len() == 32 && token.bytes().all(|b| b.is_ascii_hexdigit())
}

/// 拼装 claude 的每会话 MCP 注入参数(纯函数,单测)。铁律(D11):
/// 进程级 --mcp-config + --strict-mcp-config,即用即弃;绝不写全局
/// ~/.claude.json,绝不使用 `claude mcp add`。
pub fn claude_mcp_args(cfg_path: &std::path::Path) -> Vec<String> {
    vec![
        "--mcp-config".to_string(),
        cfg_path.to_string_lossy().to_string(),
        "--strict-mcp-config".to_string(),
        "--append-system-prompt".to_string(),
        GUIDANCE_PROMPT.to_string(),
    ]
}

/// 一次 claude POST 的结果,axum handler 映射成 HTTP 响应。
///
/// 对 JSON-RPC **请求**的传输层故障(token 未注册、超时)以
/// `Response`(JSON-RPC error 对象 @ HTTP 200)返回,绝不裸非 2xx
/// (模块头的 OAuth 误判坑)。
pub enum PostOutcome {
    /// 一个 JSON-RPC 响应体(application/json, 200):client 转回的真
    /// 响应,或本地为传输层故障合成的 JSON-RPC error。
    Response(String),
    /// 通知已受理,无体(202)。
    Accepted,
}

/// 核心 POST 处理,抽出便于单测。
pub async fn handle_post(token: &str, body: String, state: &McpProxy) -> PostOutcome {
    let id = extract_id_key(&body);
    let session = state.session_for(token);
    tracing::debug!(
        token = %token_prefix(token),
        is_request = id.is_some(),
        session = ?session,
        "remote MCP POST"
    );

    match (id, session) {
        // 未知 token 的请求:200 + JSON-RPC error(绝不 404)。
        (Some(id), None) => {
            tracing::warn!(token = %token_prefix(token), "remote MCP POST for unknown token");
            PostOutcome::Response(jsonrpc_error(
                &id,
                -32001,
                "remote MCP session not registered (token unknown or expired)",
            ))
        }
        // 已知会话的请求:转发并阻塞等配对响应,method 感知选档。
        (Some(id), Some(session_id)) => {
            let timeout = timeout_for(&body);
            let (tx, rx) = oneshot::channel();
            state
                .pending
                .insert((session_id, CC_BROWSER_SERVER.to_string(), id.clone()), tx);
            state
                .send_to_hub(OutFrame::Text(ClientMsg::RemoteMcp {
                    session_id,
                    server: CC_BROWSER_SERVER.to_string(),
                    payload: body,
                }))
                .await;
            match tokio::time::timeout(timeout, rx).await {
                Ok(Ok(resp)) => PostOutcome::Response(resp),
                _ => {
                    state
                        .pending
                        .remove(&(session_id, CC_BROWSER_SERVER.to_string(), id.clone()));
                    tracing::warn!(
                        token = %token_prefix(token),
                        %session_id,
                        timeout_secs = timeout.as_secs(),
                        "remote MCP request timed out awaiting client response"
                    );
                    PostOutcome::Response(jsonrpc_error(
                        &id,
                        -32000,
                        "remote MCP request timed out (the backend may still be starting \
                         on the user's machine — retrying usually succeeds)",
                    ))
                }
            }
        }
        // 未知 token 的通知:没东西可投也没东西可回;202 而非 404。
        (None, None) => {
            tracing::warn!(
                token = %token_prefix(token),
                "remote MCP notification for unknown token; dropping"
            );
            PostOutcome::Accepted
        }
        // 已知会话的通知:转发,无响应可等。
        (None, Some(session_id)) => {
            state
                .send_to_hub(OutFrame::Text(ClientMsg::RemoteMcp {
                    session_id,
                    server: CC_BROWSER_SERVER.to_string(),
                    payload: body,
                }))
                .await;
            PostOutcome::Accepted
        }
    }
}

/// 绑 localhost MCP 监听。POST `/mcp/:token` 即阻塞式 JSON-RPC 中继;
/// GET 同路径暂回 405(Phase E Task 15 换成 SSE 通知流);`/healthz`
/// 供探活。仅 127.0.0.1,不开新公网监听面。
pub async fn serve(state: McpProxy, port: u16) -> std::io::Result<()> {
    use axum::extract::{Path, State};
    use axum::http::StatusCode;
    use axum::response::IntoResponse;
    use axum::routing::{get, post};

    let app = axum::Router::new()
        .route("/healthz", get(|| async { "ok" }))
        .route(
            "/mcp/:token",
            post(
                |Path(token): Path<String>, State(st): State<McpProxy>, body: String| async move {
                    match handle_post(&token, body, &st).await {
                        PostOutcome::Response(b) => (
                            [(axum::http::header::CONTENT_TYPE, "application/json")],
                            b,
                        )
                            .into_response(),
                        PostOutcome::Accepted => StatusCode::ACCEPTED.into_response(),
                    }
                },
            )
            .get(|| async { StatusCode::METHOD_NOT_ALLOWED }),
        )
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(("127.0.0.1", port)).await?;
    tracing::info!(port, "remote MCP proxy endpoint listening on 127.0.0.1");
    axum::serve(listener, app)
        .await
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))
}
```

- [ ] **D9-5 跑测试确认通过**:`cargo test -p cloudcode-agent mcp_proxy` —— 预期 14 个 PASS(含真 HTTP 的 `real_http_post_roundtrips_via_endpoint`)。`cargo test --workspace` 全绿。
- [ ] **D9-6 commit**:

```bash
git add crates/agent/Cargo.toml crates/agent/src/mcp_proxy.rs crates/agent/src/main.rs
git commit -m "agent: mcp_proxy — resident localhost HTTP MCP endpoint, generalized

Port of feature/local-browser mcp_endpoint.rs: token->session routing,
POST-blocking id correlation, tiered method-aware timeouts
(25s/120s/600s via LONG_CALL_TOOLS), JSON-RPC-error-at-HTTP-200 for
every request-level transport failure (claude treats any non-2xx MCP
POST as needs-OAuth). RemoteMcp frames carry the cc-browser server
name; pending keys include it for plan-2 multi-server safety. Adds
claude_mcp_args (mcp-config + strict-mcp-config + generic guidance
prompt) as a pure, tested function.

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

### Task 10: `[remote_mcp]` 配置 + main/ws 接线

**Files:**
- Modify: `crates/agent/src/config.rs`(`Config` 结构体 L5-30:加字段;文件内追加段结构体与测试)
- Modify: `crates/agent/src/main.rs`(`AppState` 约 L20-30;`serve()` 约 L200-250)
- Modify: `crates/agent/src/ws.rs`(`run_once` 约 L99 起;读循环 `ServerMsg` match 约 L173-250)
- Test: `crates/agent/src/config.rs` 内 `#[cfg(test)]`

- [ ] **D10-1 写失败测试**:在 `crates/agent/src/config.rs` 文件末尾追加:

```rust
#[cfg(test)]
mod remote_mcp_config_tests {
    use super::*;

    #[test]
    fn remote_mcp_defaults() {
        // 段缺省整体:Default 实现。
        let d = RemoteMcpConfig::default();
        assert!(d.enabled);
        assert_eq!(d.port, 7110);
        assert_eq!(d.tools_manifest, None);

        // 段存在但字段缺省:serde 字段默认。
        let c: RemoteMcpConfig = toml::from_str("").unwrap();
        assert!(c.enabled);
        assert_eq!(c.port, 7110);

        // 显式覆盖。
        let c: RemoteMcpConfig =
            toml::from_str("enabled = false\nport = 7200\ntools_manifest = \"/etc/cc/tools.json\"")
                .unwrap();
        assert!(!c.enabled);
        assert_eq!(c.port, 7200);
        assert_eq!(c.tools_manifest, Some(PathBuf::from("/etc/cc/tools.json")));
    }
}
```

- [ ] **D10-2 跑测试确认失败**:`cargo test -p cloudcode-agent remote_mcp_config` —— 预期编译失败:``cannot find struct... `RemoteMcpConfig` ``。
- [ ] **D10-3 实现配置段**:在 `crates/agent/src/config.rs`:

  ① `Config` 结构体中 `pub sandbox: SandboxConfig,` 之后加:

```rust
    /// `[remote_mcp]` 段:agent 侧远程-MCP proxy(cc-browser 管道)
    /// 开关与端点(决策 D10)。整段缺省 = 全默认(零配置即用)。
    #[serde(default)]
    pub remote_mcp: RemoteMcpConfig,
```

  ② 文件中其他段结构体(如 `SandboxConfig`)定义附近追加:

```rust
fn remote_mcp_default_enabled() -> bool {
    true
}

fn remote_mcp_default_port() -> u16 {
    7110
}

/// `[remote_mcp]` 段:远程-MCP proxy 设置。
#[derive(Debug, Clone, Deserialize)]
pub struct RemoteMcpConfig {
    /// 总开关:false 时不启动 localhost 端点、不向 claude 注入任何
    /// MCP 配置。默认 true。
    #[serde(default = "remote_mcp_default_enabled")]
    pub enabled: bool,
    /// proxy 监听端口(仅绑定 127.0.0.1)。
    #[serde(default = "remote_mcp_default_port")]
    pub port: u16,
    /// 静态工具表(JSON **数组**文件)路径:Phase E 在无 client 时用
    /// 它应答 tools/list;缺省 = 空表。dev-browser 的 manifest 内容
    /// 属计划②(决策 D17)。
    #[serde(default)]
    pub tools_manifest: Option<PathBuf>,
}

impl Default for RemoteMcpConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            port: 7110,
            tools_manifest: None,
        }
    }
}
```

- [ ] **D10-4 main.rs 接线**:修改 `crates/agent/src/main.rs`:

  ① `AppState`(约 L20-30)加字段(`pub audit_slot: audit::SenderSlot,` 之后):

```rust
    /// 远程-MCP proxy 状态:PtyManager(注册每会话 token、写
    /// --mcp-config)与 HTTP handler / ws 层共享同一实例。
    pub mcp: mcp_proxy::McpProxy,
```

  ② `serve()` 中,`let manager = Arc::new(PtyManager::new(` **之前**加:

```rust
    // McpProxy 与 PtyManager 必须共享同一实例(开会话时注册的路由要
    // 对 HTTP handler 可见),先于两者构建。
    let mcp = mcp_proxy::McpProxy::new();
```

  ③ `PtyManager::new(...)` 调用的实参表:在 `config.sandbox.clone(),` 之后(右括号前)追加两个实参:

```rust
        mcp.clone(),
        config.remote_mcp.clone(),
```

  ④ `AppState` 构造(`audit_slot,` 之后)加一行 `mcp,`。
  ⑤ `ws::run(state).await` **之前**插入:

```rust
    // 进程内 localhost 远程-MCP proxy 端点:claude 连这里,帧经
    // agent<->hub ws 隧道到绑定 client。enabled=false 时整个监听面
    // 都不存在。
    if state.config.remote_mcp.enabled {
        let mcp_state = state.mcp.clone();
        let port = state.config.remote_mcp.port;
        tokio::spawn(async move {
            if let Err(e) = mcp_proxy::serve(mcp_state, port).await {
                tracing::error!(error = %e, "remote MCP proxy endpoint exited");
            }
        });
    }
```

- [ ] **D10-5 pty.rs 收下新参数**:修改 `crates/agent/src/pty.rs`:

  ① `PtyManager` 结构体(L42 起)加字段(`write_sessions: crate::fs::WriteSessions,` 之后):

```rust
    /// 远程-MCP proxy(与 AppState.mcp 共享 Arc 内部):open_session
    /// 在此注册工作区 token 路由,HTTP handler 即时可见。
    mcp: crate::mcp_proxy::McpProxy,
    /// `[remote_mcp]` 配置快照(enabled / port / manifest 路径)。
    remote_mcp: crate::config::RemoteMcpConfig,
    /// 每工作区一枚稳定 remote-MCP token,键 (account, workspace)。
    /// 首次注入时铸造、之后每次 open 复用并对新 session_id 重注册
    /// (决策 D12);仅 workspace delete/reset 时移除并注销。
    workspace_tokens: DashMap<(String, String), String>,
```

  ② `PtyManager::new` 签名:`_sandbox: SandboxConfig,` 之后加两个形参:

```rust
        mcp: crate::mcp_proxy::McpProxy,
        remote_mcp: crate::config::RemoteMcpConfig,
```

  ③ `new` 末尾的 `Ok(Self { ... })` 字段表(`write_sessions: crate::fs::new_write_sessions(),` 之后)加:

```rust
            mcp,
            remote_mcp,
            workspace_tokens: DashMap::new(),
```

- [ ] **D10-6 ws.rs 拦截接线**:修改 `crates/agent/src/ws.rs`:

  ① `run_once` 中 `let (tx, mut rx) = mpsc::channel::<OutFrame>(SEND_QUEUE);` 之后插入:

```rust
    // 武装 proxy:handle_post 经这条活连接向 hub 发 RemoteMcp 帧。
    // 每次重连都用新 sender 重新武装。
    state.mcp.set_hub_sender(tx.clone()).await;
```

  ② 读循环的 `ServerMsg` match(L173 起)中,`Ok(ServerMsg::Rejected { reason }) => { ... }` 臂之后插入两个拦截臂:

```rust
                // client 后端子进程的回程帧:按 (session, server, id)
                // 配对交还阻塞中的 claude POST。在此拦截,不进
                // PtyManager(那边只有穷尽性空臂)。
                Ok(ServerMsg::RemoteMcp {
                    session_id,
                    server,
                    payload,
                }) => {
                    state.mcp.resolve_response(session_id, &server, payload);
                }
                // client 的远程-MCP 通道没了(后端不可用 / 子进程死亡 /
                // client 收摊):立刻 fail 该会话所有在飞请求,claude
                // 拿到干净的 JSON-RPC 错误而不是干等超时。
                Ok(ServerMsg::RemoteMcpClosed {
                    session_id,
                    server: _,
                    reason,
                }) => {
                    state.mcp.fail_pending(
                        session_id,
                        reason.as_deref().unwrap_or("remote MCP channel closed"),
                    );
                }
```

- [ ] **D10-7 跑测试确认通过**:`cargo test -p cloudcode-agent remote_mcp_config` 预期 1 个 PASS;`cargo build --workspace && cargo test --workspace` 全绿(确认 main/pty/ws 接线编译)。
- [ ] **D10-8 commit**:

```bash
git add crates/agent/src/config.rs crates/agent/src/main.rs crates/agent/src/pty.rs crates/agent/src/ws.rs
git commit -m "agent: [remote_mcp] config + proxy wiring through main/pty/ws

RemoteMcpConfig (enabled/port/tools_manifest, all defaulted) hangs off
agent.toml. main builds one McpProxy shared by PtyManager and the
spawned localhost endpoint (skipped entirely when disabled). ws.rs arms
the hub sender on every (re)connect and intercepts
RemoteMcp/RemoteMcpClosed into resolve_response/fail_pending.

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

### Task 11: pty 注入——工作区稳定 token + mcp-remote.json + strict 参数

**Files:**
- Modify: `crates/agent/src/pty.rs`(`open_session` 约 L552-625:`claude_args` 改 mut、注入块;workspace delete/reset 两分支:token 注销;文件末尾测试 mod)
- Test: `crates/agent/src/pty.rs` 内 `#[cfg(test)] mod remote_mcp_inject_tests`

- [ ] **D11-1 写失败测试**:在 `crates/agent/src/pty.rs` 文件末尾追加:

```rust
#[cfg(test)]
mod remote_mcp_inject_tests {
    use super::*;

    #[test]
    fn inject_gates_on_enabled_capability_and_claude() {
        // Phase D 语义:三个条件齐才注入。Phase E(Task 14)翻转为
        // 始终广告(去掉 capability 条件),届时本测试同步更新。
        assert!(should_inject_mcp(true, true, "claude"));
        assert!(!should_inject_mcp(false, true, "claude"), "disabled kills injection");
        assert!(!should_inject_mcp(true, false, "claude"), "incapable client: no injection");
        assert!(
            !should_inject_mcp(true, true, "codex"),
            "claude-only flags must never reach other tools"
        );
    }
}
```

- [ ] **D11-2 跑测试确认失败**:`cargo test -p cloudcode-agent remote_mcp_inject` —— 预期编译失败:``cannot find function `should_inject_mcp` ``。
- [ ] **D11-3 实现注入**:三处修改 `crates/agent/src/pty.rs`:

  ① 在 `impl PtyManager` 之外(文件顶部 `validate_name` 等自由函数附近)加纯函数:

```rust
/// 注入决策(纯函数,单测):Phase D = enabled && capable && claude。
/// Phase E(Task 14)翻转为 enabled && claude(始终广告,决策 D7)。
/// tool 门是硬条件:--mcp-config/--strict-mcp-config/
/// --append-system-prompt 是 claude 专属 flag,喂给 codex 等其他工具
/// 会直接启动失败(决策 D11;M1-M3 未做此门控,本计划修正)。
fn should_inject_mcp(enabled: bool, remote_mcp_capable: bool, tool_name: &str) -> bool {
    enabled && remote_mcp_capable && tool_name == "claude"
}
```

  ② `open_session` 签名中 `claude_args: Vec<String>,` 改为 `mut claude_args: Vec<String>,`;并把 B3-5 加过的 `tracing::debug!(... "open_session: client capability");` 一行**之后**插入注入块:

```rust
        // 注入本会话的远程-MCP 端点配置。token 是工作区稳定 token
        // (决策 D12):hub 每次 OpenSession(含 reattach)都铸新
        // session_id,这里把同一 token 对新 session_id 重注册(覆盖式)
        // —— tmux 里活着的 claude(内存持 token)与重启的 claude(从
        // 字节稳定的 mcp-remote.json 重读)都路由到活会话。
        if should_inject_mcp(self.remote_mcp.enabled, remote_mcp_capable, &tool_name) {
            let token = self
                .workspace_tokens
                .entry((account.clone(), workspace.clone()))
                .or_insert_with(|| {
                    // agent 重启自愈:优先采用本工作区 mcp-remote.json
                    // 已持久化的 token。只接受我们铸造的格式(32 ascii
                    // hex):被篡改/损坏的配置必须铸新,不得把任意
                    // (可猜)token 走私进路由表。
                    std::fs::read_to_string(cwd.join(".cloudcode").join("mcp-remote.json"))
                        .ok()
                        .and_then(|s| crate::mcp_proxy::extract_token_from_config(&s))
                        .filter(|t| crate::mcp_proxy::is_valid_token(t))
                        .unwrap_or_else(|| Uuid::new_v4().simple().to_string())
                })
                .clone();
            self.mcp.register(token.clone(), session_id);
            let mcp_cfg = crate::mcp_proxy::mcp_config_json(self.remote_mcp.port, &token);
            let mcp_cfg_path = cwd.join(".cloudcode").join("mcp-remote.json");
            if let Some(parent) = mcp_cfg_path.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            // 配置内含 bearer token:从创建那一刻就是 0600(不留
            // 「先写后 chmod」的全局可读窗口)。
            #[cfg(unix)]
            let write_res = {
                use std::io::Write;
                use std::os::unix::fs::OpenOptionsExt;
                std::fs::OpenOptions::new()
                    .write(true)
                    .create(true)
                    .truncate(true)
                    .mode(0o600)
                    .open(&mcp_cfg_path)
                    .and_then(|mut f| f.write_all(mcp_cfg.as_bytes()))
            };
            #[cfg(not(unix))]
            let write_res = std::fs::write(&mcp_cfg_path, &mcp_cfg);
            if let Err(e) = write_res {
                tracing::warn!(error = %e, "failed to write remote MCP config");
            }
            // `.mode(0o600)` 只在创建时生效;老文件穿过 truncate 仍保
            // 留原 mode(可能 0644),显式修一遍。
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let _ = std::fs::set_permissions(
                    &mcp_cfg_path,
                    std::fs::Permissions::from_mode(0o600),
                );
            }
            // 进程级注入:--mcp-config + --strict-mcp-config + 通用
            // 引导 prompt。绝不写全局 ~/.claude.json,绝不 `claude mcp
            // add`(D11 铁律)。strict 保证 claude 只看到这份配置 ——
            // 同机其他 claude 进程(没带这些 flag)零影响。
            claude_args.extend(crate::mcp_proxy::claude_mcp_args(&mcp_cfg_path));
        }
```

  注意位置约束:该块必须在 `tool_name` 解析(`let tool_name = tool.unwrap_or_else(...)`,约 L547)与 `let cwd = std::fs::canonicalize(...)` 之后、`// Open the PTY.` 注释之前——两者它都依赖。
  ③ token 生命周期收口(workspace 真死才注销):
  - **delete 分支**:找到 delete 处理里的 `kill-server`(其后紧跟注释 `// Wipe claude's per-project conversation history`),在 `kill-server` 的 `.output();` 与该注释**之间**插入:

```rust
                    // 工作区真死:移除其稳定 token 并注销端点路由。
                    // 同名重建的工作区会铸全新 token。
                    if let Some((_, tok)) = self
                        .workspace_tokens
                        .remove(&(account.clone(), name.clone()))
                    {
                        self.mcp.unregister(&tok);
                    }
```

  - **reset 分支**:找到 reset 处理里的 `kill-server`(其后紧跟注释 `// Keep ~/.claude/projects/<encoded-cwd>/ intact`),同样在两者之间插入:

```rust
                    // reset = 旧 claude(及一切持旧 token 者)永久消失:
                    // 退役稳定 token,下次 open 从全新 token 开始。
                    if let Some((_, tok)) = self
                        .workspace_tokens
                        .remove(&(account.clone(), name.clone()))
                    {
                        self.mcp.unregister(&tok);
                    }
```

  - **不要**在 `close()`(client detach 路径)或 PTY EOF 路径或 idle reaper 里注销 token:hub 在每次 client 掉线(合盖也算)都发 PtyClose,而 claude 还活在 tmux 里——注销会杀死活 claude 的路由。这是 M1-M3 用血换来的语义,原样保持。
- [ ] **D11-4 跑测试确认通过**:`cargo test -p cloudcode-agent remote_mcp_inject` 预期 1 个 PASS;`cargo test --workspace` 全绿。
- [ ] **D11-5 commit**:

```bash
git add crates/agent/src/pty.rs
git commit -m "agent: inject per-session MCP config into claude spawn

Workspace-stable token (minted once per (account,workspace), re-pointed
at each fresh session_id, self-healed from mcp-remote.json across agent
restarts, retired only on workspace delete/reset). Config written 0600
under <ws>/.cloudcode/mcp-remote.json; claude gets --mcp-config +
--strict-mcp-config + a generic guidance prompt — process-scoped,
never the global ~/.claude.json, never `claude mcp add`, and only when
the launched tool is claude.

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

### Task 12: 端到端 loopback——真 HTTP × 真 echo 后端

**Files:**
- Modify: `crates/agent/src/mcp_proxy.rs`(`mod tests` 追加;见决策 D15:agent 是纯 bin crate,`tests/` 无法 import 内部模块,端到端测试随 M1-M3 先例放模块内)
- Test: 同上

- [ ] **D12-1 写失败测试**:在 `crates/agent/src/mcp_proxy.rs` 的 `mod tests` 末尾追加:

```rust
    /// 测试环境探测:PATH 上有无 node(echo 桩需要)。无则 skip。
    fn node_available() -> bool {
        let Some(path) = std::env::var_os("PATH") else { return false };
        std::env::split_paths(&path).any(|d| d.join("node").is_file())
    }

    /// 端到端 loopback:真 axum HTTP 端点 ← reqwest POST tools/call;
    /// 「hub+client」由测试体内联扮演 —— 从 to_hub 通道取
    /// ClientMsg::RemoteMcp 帧,**原文**喂给真 node echo 桩,把桩的
    /// 应答经 resolve_response 配对回去。覆盖:HTTP 入口、id 配对、
    /// 帧封装、与真实 MCP-over-stdio 后端的字节级互通。(hub 的转发
    /// 线序由 Phase B 单测钉死;client 宿主由 Phase C 钉死 —— 三段
    /// 合起来即全管道。)
    #[tokio::test]
    async fn loopback_tools_call_roundtrips_through_pipe_and_echo_backend() {
        if !node_available() {
            return; // 无 node → skip
        }
        let state = McpProxy::new();
        let sid = Uuid::new_v4();
        let token = "tok-loopback";
        state.register(token.into(), sid);
        let (hub_tx, mut hub_rx) = mpsc::channel(4);
        state.set_hub_sender(hub_tx).await;

        let port = free_port();
        let serve_state = state.clone();
        tokio::spawn(async move {
            let _ = serve(serve_state, port).await;
        });

        // 内联 hub+client:隧道帧 → echo 桩 stdin;桩 stdout → 配对回包。
        let resp_state = state.clone();
        tokio::spawn(async move {
            use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
            let fixture =
                concat!(env!("CARGO_MANIFEST_DIR"), "/../../test-fixtures/echo-mcp.mjs");
            let mut child = tokio::process::Command::new("node")
                .arg(fixture)
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::piped())
                .kill_on_drop(true)
                .spawn()
                .expect("spawn echo backend");
            let mut stdin = child.stdin.take().expect("stdin");
            let stdout = child.stdout.take().expect("stdout");
            let mut lines = tokio::io::BufReader::new(stdout).lines();
            while let Some(OutFrame::Text(ClientMsg::RemoteMcp {
                session_id,
                server,
                payload,
            })) = hub_rx.recv().await
            {
                assert_eq!(server, CC_BROWSER_SERVER);
                stdin.write_all(payload.as_bytes()).await.unwrap();
                stdin.write_all(b"\n").await.unwrap();
                stdin.flush().await.unwrap();
                if let Ok(Some(line)) = lines.next_line().await {
                    resp_state.resolve_response(session_id, &server, line);
                }
            }
        });

        let client = reqwest::Client::new();
        let base = format!("http://127.0.0.1:{port}");
        assert_eq!(wait_healthz(&client, &base).await, "ok");

        let resp = client
            .post(format!("{base}/mcp/{token}"))
            .body(
                r#"{"jsonrpc":"2.0","id":11,"method":"tools/call","params":{"name":"echo","arguments":{"text":"pipe"}}}"#,
            )
            .send()
            .await
            .expect("POST tools/call");
        assert_eq!(resp.status(), reqwest::StatusCode::OK);
        let text = resp.text().await.unwrap();
        assert!(text.contains(r#""id":11"#), "response keeps id: {text}");
        assert!(text.contains("echo: pipe"), "echo result came back: {text}");
    }
```

- [ ] **D12-2 跑测试确认失败**:`cargo test -p cloudcode-agent loopback_tools_call` —— 这是纯新增测试,首跑应当**直接通过**(它只消费 Task 9 的实现)。若它失败,按失败信息修 Task 9 的实现;若你的环境无 node,它会静默 skip——此时必须先装 node 再继续(本测试是本阶段的验收锚,不允许以 skip 状态过关)。
- [ ] **D12-3 跑全量回归**:`cargo test --workspace` 全绿。
- [ ] **D12-4 commit**:

```bash
git add crates/agent/src/mcp_proxy.rs
git commit -m "agent: in-process loopback e2e — real HTTP POST through frames to a real MCP stub

reqwest -> axum endpoint -> ClientMsg::RemoteMcp frame -> node echo-mcp
stdin -> stdout -> resolve_response -> POST body. Pins the full opaque
round trip including byte-level interop with a real MCP-over-stdio
backend; hub forwarding and the client host are pinned by their own
phase tests.

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

# Phase E — 降级与健壮(始终广告 / 无 client 错误 / list_changed / 断连快失败)

目标:落实 spec 降级模型四条。① 始终广告:注入不再以 client capability 为门,claude 冷启动也能完成握手并看到(静态表/空表)工具;② 调用时无 client → `-32004` 可执行文案;③ client attach/detach → `notifications/tools/list_changed`(经新建的 GET SSE 流);④ 永不阻塞:detach 即 fail 在飞请求(分层超时 Phase D 已就位)。外加 D16 的冷启动握手缝合(client 宿主合成握手)。

### Task 13: 会话 attach 跟踪 + detach 快失败

**Files:**
- Modify: `crates/agent/src/mcp_proxy.rs`(`McpProxy` 结构体与 impl:`attached` 字段 + 三个方法;`mod tests` 追加)
- Modify: `crates/agent/src/pty.rs`(`open_session` 注入块之后:attach 标记;`close()` 入口:detach)
- Test: `crates/agent/src/mcp_proxy.rs` 内 `mod tests`

- [ ] **E13-1 写失败测试**:在 `crates/agent/src/mcp_proxy.rs` 的 `mod tests` 末尾追加:

```rust
    #[test]
    fn attach_detach_lifecycle() {
        let state = McpProxy::new();
        let sid = Uuid::new_v4();
        assert!(!state.is_attached(sid));
        state.set_attached(sid);
        assert!(state.is_attached(sid));
        state.detach(sid);
        assert!(!state.is_attached(sid));
    }

    #[tokio::test]
    async fn detach_fails_pending_requests() {
        // spec 降级④:client 掉线瞬间,在飞请求立刻以 JSON-RPC 错误
        // 收尾,绝不等满超时档。
        let state = McpProxy::new();
        let sid = Uuid::new_v4();
        state.set_attached(sid);
        let (tx, rx) = oneshot::channel::<String>();
        state
            .pending
            .insert((sid, CC_BROWSER_SERVER.to_string(), "7".to_string()), tx);
        state.detach(sid);
        let body = rx.await.expect("failed fast");
        assert!(body.contains("-32002"), "fail_pending error code: {body}");
        assert!(body.contains("client detached"), "reason: {body}");
        assert!(!state.is_attached(sid));
    }
```

- [ ] **E13-2 跑测试确认失败**:`cargo test -p cloudcode-agent attach_detach` —— 预期编译失败:``no method named `set_attached` found``。
- [ ] **E13-3 最小实现**:修改 `crates/agent/src/mcp_proxy.rs`:

  ① `McpProxy` 结构体加字段(`to_hub` 之后):

```rust
    /// 当前有 capable client 在线的会话集合(PtyOpen 标记 / PtyClose
    /// 摘除)。在线 → 帧转发;离线 → 权威 fallback(Task 14)。
    attached: Arc<DashMap<Uuid, ()>>,
```

  ② `McpProxy::new()` 的字段初始化加 `attached: Arc::new(DashMap::new()),`。
  ③ `impl McpProxy` 中(`fail_pending` 之前)加三个方法:

```rust
    /// capable client 已 attach 到该会话(来自 PtyOpen.remote_mcp_capable)。
    pub fn set_attached(&self, session_id: Uuid) {
        self.attached.insert(session_id, ());
    }

    /// 该会话此刻是否有 capable client 在线(转发 vs fallback 的开关)。
    pub fn is_attached(&self, session_id: Uuid) -> bool {
        self.attached.contains_key(&session_id)
    }

    /// client 离线(hub 在每次 client detach——含合盖——都发 PtyClose):
    /// 摘除在线标记并立刻 fail 该会话全部在飞请求(spec 降级④)。
    /// 注意与 RemoteMcpClosed 的分工:那个只 fail_pending、不摘标记
    /// (client 还在线,后端下一帧惰性重生);这个两者都做。
    pub fn detach(&self, session_id: Uuid) {
        self.attached.remove(&session_id);
        self.fail_pending(session_id, "client detached");
    }
```

  ④ `crates/agent/src/pty.rs`:
  - `open_session` 的注入块(Task 11)**之后**插入:

```rust
        // attach 跟踪(Phase E):capable client 上线 → 本会话进入
        // 「转发」模式;非 capable(webterm 等)开的会话保持「权威
        // fallback」模式。Task 15 在此补发 list_changed。
        if remote_mcp_capable {
            self.mcp.set_attached(session_id);
        }
```

  - `close()`(处理 `ServerMsg::PtyClose`)函数体**第一行**插入:

```rust
        // client 离线:摘在线标记 + 秒杀在飞请求(永不让 claude 干等)。
        // 工作区稳定 token 刻意不在这儿注销 —— claude 还活在 tmux 里。
        self.mcp.detach(session_id);
```

- [ ] **E13-4 跑测试确认通过**:`cargo test -p cloudcode-agent attach_detach && cargo test -p cloudcode-agent detach_fails_pending` 共 2 个 PASS;`cargo test --workspace` 全绿。
- [ ] **E13-5 commit**:

```bash
git add crates/agent/src/mcp_proxy.rs crates/agent/src/pty.rs
git commit -m "agent: per-session attach tracking; detach fails pending instantly

PtyOpen(remote_mcp_capable=true) marks the session attached; PtyClose
(sent by the hub on every client detach) clears it and fails all
in-flight requests with -32002 so claude never waits out a timeout.
RemoteMcpClosed keeps its narrower meaning (backend gone, client still
attached, lazy respawn on next frame).

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

### Task 14: 权威 fallback(始终广告)+ 注入翻转 + 冷启动握手缝合

**Files:**
- Modify: `crates/agent/src/mcp_proxy.rs`(`McpProxy` 加 `static_tools`;`handle_post` 加 detached 守卫臂;新增 `fallback_request`/`NO_CLIENT_MSG`/`load_tools_manifest`/`with_static_tools`;更新既有测试 + 新测试)
- Modify: `crates/agent/src/main.rs`(`McpProxy::new()` → 载入 manifest 的 `with_static_tools`)
- Modify: `crates/agent/src/pty.rs`(`should_inject_mcp` 去掉 capability 条件;更新测试)
- Modify: `crates/client/src/mcp_host.rs`(`McpHost::deliver` 冷启动握手合成;`mod tests` 追加)
- Test: `crates/agent/src/mcp_proxy.rs`、`crates/agent/src/pty.rs`、`crates/client/src/mcp_host.rs`

- [ ] **E14-1 写失败测试(agent fallback)**:在 `crates/agent/src/mcp_proxy.rs` 的 `mod tests` 末尾追加:

```rust
    #[tokio::test]
    async fn detached_initialize_is_answered_authoritatively() {
        // 冷启动(注册了路由、无 client 在线):initialize 由 proxy 权威
        // 应答 —— 回显请求的 protocolVersion、声明 tools.listChanged,
        // claude 才能完成握手并在之后消费 list_changed(决策 D16)。
        let state = McpProxy::with_static_tools(
            r#"[{"name":"echo","description":"d","inputSchema":{"type":"object"}}]"#.to_string(),
        );
        let sid = Uuid::new_v4();
        state.register("t".into(), sid);
        let out = handle_post(
            "t",
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"claude","version":"1"}}}"#
                .to_string(),
            &state,
        )
        .await;
        match out {
            PostOutcome::Response(b) => {
                let v: serde_json::Value = serde_json::from_str(&b).unwrap();
                assert_eq!(v["id"], 1);
                assert_eq!(
                    v["result"]["protocolVersion"], "2024-11-05",
                    "echoes the requested protocolVersion"
                );
                assert_eq!(v["result"]["capabilities"]["tools"]["listChanged"], true);
                assert_eq!(v["result"]["serverInfo"]["name"], "cc-browser");
            }
            _ => panic!("expected an authoritative response"),
        }
    }

    #[tokio::test]
    async fn detached_tools_list_serves_static_manifest_or_empty() {
        let state = McpProxy::with_static_tools(r#"[{"name":"echo"}]"#.to_string());
        let sid = Uuid::new_v4();
        state.register("t".into(), sid);
        match handle_post(
            "t",
            r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#.to_string(),
            &state,
        )
        .await
        {
            PostOutcome::Response(b) => {
                let v: serde_json::Value = serde_json::from_str(&b).unwrap();
                assert_eq!(v["result"]["tools"][0]["name"], "echo");
            }
            _ => panic!("expected a Response"),
        }
        // 缺省构造 = 空表(始终广告:server 健在、工具暂无),不是错误。
        let bare = McpProxy::new();
        bare.register("t".into(), Uuid::new_v4());
        match handle_post(
            "t",
            r#"{"jsonrpc":"2.0","id":3,"method":"tools/list"}"#.to_string(),
            &bare,
        )
        .await
        {
            PostOutcome::Response(b) => {
                let v: serde_json::Value = serde_json::from_str(&b).unwrap();
                assert_eq!(v["result"]["tools"], serde_json::json!([]));
            }
            _ => panic!("expected a Response"),
        }
    }

    #[tokio::test]
    async fn detached_tools_call_gets_actionable_error() {
        // spec 降级②:无 client 调用 → JSON-RPC 错误(非传输失败),
        // 文案可执行 —— claude 把"打开 cloudcode CLI"转达给用户;
        // webterm-only 接入恒走此路径。
        let state = McpProxy::new();
        let sid = Uuid::new_v4();
        state.register("t".into(), sid);
        let out = handle_post(
            "t",
            r#"{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"echo","arguments":{}}}"#
                .to_string(),
            &state,
        )
        .await;
        match out {
            PostOutcome::Response(b) => {
                let v: serde_json::Value = serde_json::from_str(&b).unwrap();
                assert_eq!(v["error"]["code"], -32004);
                let msg = v["error"]["message"].as_str().unwrap();
                assert!(msg.contains("cloudcode CLI"), "actionable wording: {msg}");
            }
            _ => panic!("expected a JSON-RPC error, not a transport failure"),
        }
    }

    #[tokio::test]
    async fn detached_notification_is_swallowed_not_forwarded() {
        let state = McpProxy::new();
        let sid = Uuid::new_v4();
        state.register("t".into(), sid);
        let (hub_tx, mut hub_rx) = mpsc::channel(4);
        state.set_hub_sender(hub_tx).await;
        let out = handle_post(
            "t",
            r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#.to_string(),
            &state,
        )
        .await;
        assert!(matches!(out, PostOutcome::Accepted));
        assert!(hub_rx.try_recv().is_err(), "nothing may be forwarded while detached");
    }

    #[test]
    fn tools_manifest_loading_tolerates_garbage() {
        assert_eq!(load_tools_manifest(None), "[]");
        let dir = tempfile::tempdir().unwrap();
        let good = dir.path().join("good.json");
        std::fs::write(&good, r#"[{"name":"t"}]"#).unwrap();
        assert_eq!(load_tools_manifest(Some(&good)), r#"[{"name":"t"}]"#);
        let bad = dir.path().join("bad.json");
        std::fs::write(&bad, r#"{"not":"array"}"#).unwrap();
        assert_eq!(load_tools_manifest(Some(&bad)), "[]");
        assert_eq!(load_tools_manifest(Some(&dir.path().join("missing.json"))), "[]");
    }
```

- [ ] **E14-2 跑测试确认失败**:`cargo test -p cloudcode-agent detached_` —— 预期编译失败:``no function or associated item named `with_static_tools` ``。
- [ ] **E14-3 实现 agent fallback**:修改 `crates/agent/src/mcp_proxy.rs`:

  ① `McpProxy` 结构体加字段(`attached` 之后):

```rust
    /// 无 client 在线时 tools/list 的权威应答内容:JSON **数组**原文
    /// (来自 [remote_mcp].tools_manifest,缺省 "[]")。数据不是代码,
    /// 不破坏 proxy 的 backend 无关性;dev-browser 的 manifest 内容
    /// 属计划②(决策 D17)。
    static_tools: Arc<String>,
```

  ② `new()` 改为委托,并新增构造器(替换原 `pub fn new()` 实现):

```rust
    pub fn new() -> Self {
        Self::with_static_tools("[]".to_string())
    }

    /// 带静态工具表构造(main.rs 启动时从 manifest 文件载入)。
    pub fn with_static_tools(static_tools: String) -> Self {
        Self {
            routes: Arc::new(DashMap::new()),
            pending: Arc::new(DashMap::new()),
            to_hub: Arc::new(RwLock::new(None)),
            attached: Arc::new(DashMap::new()),
            static_tools: Arc::new(static_tools),
        }
    }
```

  ③ 模块级新增(`jsonrpc_error` 附近):

```rust
/// 调用时无可用 client/后端的可执行文案(-32004,决策 D13)。claude
/// 会把它转达给用户;webterm-only 接入恒走此路径。
const NO_CLIENT_MSG: &str = "cc-browser backend is not connected: no cloudcode CLI client \
with a configured MCP backend is attached to this session (the web terminal cannot host \
local tools). Ask the user to open the cloudcode CLI on their local machine, then retry \
after they confirm it is connected.";

/// 无 client 在线时的权威应答(决策 D7/D16):initialize 本地应答
/// (回显请求的 protocolVersion、声明 tools.listChanged),tools/list
/// 用静态表,其余请求 → -32004 可执行文案。
fn fallback_request(id_raw: &str, body: &str, tools_json: &str) -> String {
    let v: Option<serde_json::Value> = serde_json::from_str(body).ok();
    let method = v
        .as_ref()
        .and_then(|x| x.get("method"))
        .and_then(|m| m.as_str())
        .unwrap_or("");
    match method {
        "initialize" => {
            let proto = v
                .as_ref()
                .and_then(|x| x.get("params"))
                .and_then(|p| p.get("protocolVersion"))
                .and_then(|s| s.as_str())
                .unwrap_or("2025-06-18");
            format!(
                r#"{{"jsonrpc":"2.0","id":{id_raw},"result":{{"protocolVersion":{proto_json},"capabilities":{{"tools":{{"listChanged":true}}}},"serverInfo":{{"name":"{CC_BROWSER_SERVER}","version":"{ver}"}}}}}}"#,
                proto_json = serde_json::Value::String(proto.to_string()),
                ver = env!("CARGO_PKG_VERSION"),
            )
        }
        "tools/list" => format!(
            r#"{{"jsonrpc":"2.0","id":{id_raw},"result":{{"tools":{tools_json}}}}}"#
        ),
        _ => jsonrpc_error(id_raw, -32004, NO_CLIENT_MSG),
    }
}

/// 读静态工具表(JSON 数组文件)。读不到 / 不是数组 → 告警 + 空表,
/// 坏 manifest 绝不拖垮 agent 启动。
pub fn load_tools_manifest(path: Option<&std::path::Path>) -> String {
    let Some(path) = path else {
        return "[]".to_string();
    };
    match std::fs::read_to_string(path) {
        Ok(s) => match serde_json::from_str::<Vec<serde_json::Value>>(&s) {
            Ok(_) => s.trim().to_string(),
            Err(e) => {
                tracing::warn!(error = %e, path = %path.display(),
                    "tools_manifest is not a JSON array; using empty list");
                "[]".to_string()
            }
        },
        Err(e) => {
            tracing::warn!(error = %e, path = %path.display(),
                "cannot read tools_manifest; using empty list");
            "[]".to_string()
        }
    }
}
```

  ④ `handle_post` 的 match 中,在 `(Some(id), Some(session_id)) => { ... }` 这个转发臂**之前**与 `(None, Some(session_id)) => { ... }` 转发臂**之前**各插入一个守卫臂:

```rust
        // 路由已注册但无 capable client 在线(冷启动 / webterm-only /
        // client 掉线后):权威 fallback —— 始终广告(决策 D7)。
        (Some(id), Some(session_id)) if !state.is_attached(session_id) => {
            tracing::debug!(%session_id, "remote MCP request while detached; fallback answering");
            PostOutcome::Response(fallback_request(&id, &body, state.static_tools.as_str()))
        }
```

```rust
        // 无 client 在线:通知本地吞掉(202),不投递。
        (None, Some(session_id)) if !state.is_attached(session_id) => PostOutcome::Accepted,
```

  ⑤ **更新 Phase D 既有测试**(它们建在「注册即转发」语义上,现在转发要求 attached):给下列 4 个测试在 `state.register(...)` 之后各加一行 `state.set_attached(sid);`:
  - `notification_is_forwarded_and_accepted`
  - `request_blocks_then_resolves_on_matching_response`
  - `real_http_post_roundtrips_via_endpoint`
  - `loopback_tools_call_roundtrips_through_pipe_and_echo_backend`
- [ ] **E14-4 注入翻转(始终广告)**:修改 `crates/agent/src/pty.rs`:

  ① `should_inject_mcp` 整体替换为:

```rust
/// 注入决策(纯函数,单测):Phase E 起 = enabled && claude ——
/// **不再**看 client capability(始终广告,决策 D7):claude 冷启动
/// 也要拿到 cc-browser 配置,无 client 时由 proxy 权威 fallback 应答。
/// tool 门是硬条件:这些是 claude 专属 flag(决策 D11)。
fn should_inject_mcp(enabled: bool, tool_name: &str) -> bool {
    enabled && tool_name == "claude"
}
```

  ② 注入块的调用处改为 `if should_inject_mcp(self.remote_mcp.enabled, &tool_name) {`。
  ③ `mod remote_mcp_inject_tests` 的测试整体替换为:

```rust
    #[test]
    fn inject_gates_on_enabled_and_claude_only() {
        // Phase E 语义:始终广告 —— capability 不再参与注入决策
        // (它只驱动 attach 跟踪与 list_changed)。
        assert!(should_inject_mcp(true, "claude"));
        assert!(!should_inject_mcp(false, "claude"), "disabled kills injection");
        assert!(
            !should_inject_mcp(true, "codex"),
            "claude-only flags must never reach other tools"
        );
    }
```

- [ ] **E14-5 main.rs 接 manifest**:`crates/agent/src/main.rs` 中把 Task 10 的 `let mcp = mcp_proxy::McpProxy::new();` 替换为:

```rust
    // McpProxy 与 PtyManager 必须共享同一实例,先于两者构建。静态
    // 工具表(始终广告的冷启动 tools/list 内容)启动时载入一次。
    let static_tools =
        mcp_proxy::load_tools_manifest(config.remote_mcp.tools_manifest.as_deref());
    let mcp = mcp_proxy::McpProxy::with_static_tools(static_tools);
```

- [ ] **E14-6 写失败测试(client 冷启动握手缝合)**:在 `crates/client/src/mcp_host.rs` 的 `mod tests` 末尾追加:

```rust
    #[tokio::test]
    async fn host_synthesizes_handshake_when_cold_started_mid_session() {
        if !node_available() {
            return;
        }
        // 决策 D16 的缝:claude 冷启动时 initialize 被 agent 侧 fallback
        // 权威应答(client 不在线),宿主缓存里没有真握手;client 上线后
        // 第一帧直接是 tools/list —— 宿主必须自己合成握手喂后端,并吞掉
        // 合成 initialize 的响应,否则后端报「未初始化」。
        let fixture = concat!(env!("CARGO_MANIFEST_DIR"), "/../../test-fixtures/echo-mcp.mjs");
        let (out_tx, mut out_rx) = mpsc::channel(8);
        let mut host = McpHost::new(("node".to_string(), vec![fixture.to_string()]), out_tx);
        host.deliver(r#"{"jsonrpc":"2.0","id":9,"method":"tools/list"}"#.to_string())
            .await
            .expect("deliver");
        let resp = tokio::time::timeout(std::time::Duration::from_secs(10), out_rx.recv())
            .await
            .expect("frame within 10s")
            .expect("alive");
        assert!(
            resp.contains(r#""id":9"#),
            "first visible frame is the tools/list response: {resp}"
        );
        assert!(
            !resp.contains("serverInfo"),
            "synthesized initialize response must be swallowed: {resp}"
        );
        assert_eq!(
            host.handshake.lock().unwrap().len(),
            2,
            "synthesized handshake cached for future respawns"
        );
    }
```

- [ ] **E14-7 跑测试确认失败**:`cargo test -p cloudcode-client host_synthesizes` —— 预期**断言失败**(非编译失败):echo 桩对没有握手前导的 tools/list 也会应答,但宿主缓存 len == 0,最后一条断言 `assertion failed ... left: 0, right: 2`。(真实后端会直接拒答;桩宽松,断言钉在缓存上。)
- [ ] **E14-8 实现握手合成**:修改 `crates/client/src/mcp_host.rs`:

  ① 模块级(`json_method` 之后)新增:

```rust
/// 宿主自有的合成握手(决策 D16)。id 用字符串 "cc-host-init":
/// start_replayed 的吞响应按 id 匹配,不会与 claude 的 id 冲突。
/// 只在「缓存为空且首帧不是 initialize」的冷启动缝隙调用;一经合成
/// 即入缓存,之后的重生走同一条重放路径。
fn synthesize_handshake(cache: &mut Vec<String>) {
    cache.push(format!(
        r#"{{"jsonrpc":"2.0","id":"cc-host-init","method":"initialize","params":{{"protocolVersion":"2025-06-18","capabilities":{{}},"clientInfo":{{"name":"cloudcode","version":"{}"}}}}}}"#,
        env!("CARGO_PKG_VERSION")
    ));
    cache.push(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#.to_string());
}
```

  ② `McpHost::deliver` 的开头(`if self.chan.is_none() {` 块内、`self.spawn_channel().await?;` 之前)插入:

```rust
            // 冷启动缝合(决策 D16):缓存为空而首帧不是 initialize ——
            // claude 的真握手被 agent fallback 吃掉了。合成宿主自有握手
            // 入缓存;spawn_channel 据此走 start_replayed(重放 + 按 id
            // 吞掉合成 initialize 的响应)。
            {
                let mut cache = self.handshake.lock().expect("handshake mutex");
                if cache.is_empty() && json_method(&payload).as_deref() != Some("initialize") {
                    synthesize_handshake(&mut cache);
                }
            }
```

- [ ] **E14-9 跑测试确认通过**:`cargo test -p cloudcode-agent detached_ && cargo test -p cloudcode-agent tools_manifest && cargo test -p cloudcode-agent remote_mcp_inject && cargo test -p cloudcode-client host_synthesizes` 全 PASS;`cargo test --workspace` 全绿(确认 E14-3 ⑤ 的四个既有测试已补 `set_attached`)。
- [ ] **E14-10 commit**:

```bash
git add crates/agent/src/mcp_proxy.rs crates/agent/src/pty.rs crates/agent/src/main.rs crates/client/src/mcp_host.rs
git commit -m "always-advertise: authoritative fallback when no client is attached

Injection no longer gates on client capability — claude always gets the
cc-browser config. While detached the proxy answers initialize
authoritatively (echoing protocolVersion, declaring tools.listChanged),
serves tools/list from the [remote_mcp].tools_manifest static array
(default empty), returns -32004 with open-the-CLI wording for any other
request, and swallows notifications. Client host stitches the cold-boot
seam by synthesizing its own backend handshake when the cache is empty.

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

### Task 15: `list_changed` 通知——GET SSE 流 + attach/detach 触发 + 手动验证

**Files:**
- Modify: `crates/agent/src/mcp_proxy.rs`(`McpProxy` 加 `notify` 字段与 `subscribe`/`notify_list_changed`;`detach` 补发通知;`serve` 的 GET 405 换成 SSE;`mod tests` 追加)
- Modify: `crates/agent/src/pty.rs`(`open_session` attach 标记后补发通知)
- Test: `crates/agent/src/mcp_proxy.rs` 内 `mod tests` + 手动验证清单

- [ ] **E15-1 写失败测试**:在 `crates/agent/src/mcp_proxy.rs` 的 `mod tests` 末尾追加:

```rust
    #[tokio::test]
    async fn attach_detach_pushes_list_changed_to_subscribed_stream() {
        let state = McpProxy::new();
        let sid = Uuid::new_v4();
        state.register("tok-n".into(), sid);
        let mut rx = state.subscribe("tok-n");

        // 模拟 client 上线(pty.rs 在 PtyOpen 时:set_attached + notify)。
        state.set_attached(sid);
        state.notify_list_changed(sid);
        let frame = rx.recv().await.expect("notification pushed");
        assert_eq!(frame, LIST_CHANGED_FRAME);
        let v: serde_json::Value = serde_json::from_str(&frame).unwrap();
        assert_eq!(v["method"], "notifications/tools/list_changed");

        // 模拟 client 掉线:detach 自带 list_changed。
        state.detach(sid);
        let frame = rx.recv().await.expect("notification on detach");
        assert_eq!(frame, LIST_CHANGED_FRAME);
    }

    #[tokio::test]
    async fn notify_without_subscriber_is_noop() {
        // claude 不开 GET 流(D8 的未验证假设不成立时)→ 通知静默丢弃,
        // 不 panic、不阻塞、无副作用。
        let state = McpProxy::new();
        let sid = Uuid::new_v4();
        state.register("tok-x".into(), sid);
        state.notify_list_changed(sid);
    }

    #[tokio::test]
    async fn sse_get_stream_delivers_notification_over_real_http() {
        let state = McpProxy::new();
        let sid = Uuid::new_v4();
        let token = "tok-sse";
        state.register(token.into(), sid);
        let port = free_port();
        let serve_state = state.clone();
        tokio::spawn(async move {
            let _ = serve(serve_state, port).await;
        });
        let client = reqwest::Client::new();
        let base = format!("http://127.0.0.1:{port}");
        assert_eq!(wait_healthz(&client, &base).await, "ok");

        let resp = client
            .get(format!("{base}/mcp/{token}"))
            .send()
            .await
            .expect("GET sse");
        assert_eq!(resp.status(), reqwest::StatusCode::OK);
        // GET handler 同步完成 subscribe,但经网络有传播窗:轮询直到
        // 订阅出现再触发通知。
        for _ in 0..50 {
            if state.notify.contains_key(token) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(state.notify.contains_key(token), "GET must register a subscription");
        state.notify_list_changed(sid);

        let mut stream = resp;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            let item = tokio::time::timeout_at(deadline, stream.chunk())
                .await
                .expect("sse chunk within 5s")
                .expect("chunk read ok");
            let Some(bytes) = item else { panic!("sse stream ended early") };
            let text = String::from_utf8_lossy(&bytes).to_string();
            if text.contains("tools/list_changed") {
                break; // SSE 事件携带通知帧,到达即过
            }
            // keepalive 注释行等:继续读。
        }

        // 未注册 token 的 GET:405(「本端点不提供流」;OAuth 误判坑只
        // 在 POST 的非 2xx,GET 405 是 M1 验证过的安全形态)。
        let nope = client
            .get(format!("{base}/mcp/unknown"))
            .send()
            .await
            .expect("GET unknown");
        assert_eq!(nope.status(), reqwest::StatusCode::METHOD_NOT_ALLOWED);
    }
```

- [ ] **E15-2 跑测试确认失败**:`cargo test -p cloudcode-agent list_changed` —— 预期编译失败:``no method named `subscribe` found`` / ``cannot find value `LIST_CHANGED_FRAME` ``。
- [ ] **E15-3 实现**:修改 `crates/agent/src/mcp_proxy.rs`:

  ① 模块级常量(`NO_CLIENT_MSG` 附近):

```rust
/// 服务端通知:工具集(真实可用性)变了,请重拉 tools/list。
const LIST_CHANGED_FRAME: &str =
    r#"{"jsonrpc":"2.0","method":"notifications/tools/list_changed"}"#;
```

  ② `McpProxy` 结构体加字段(`static_tools` 之后):

```rust
    /// 每 token 一条服务端通知流(claude 的 GET SSE 订阅)。键是
    /// token 而非 session_id:claude 的 GET 长连横跨多次 reattach
    /// (session_id 会换),token 才与 claude 进程同寿。
    notify: Arc<DashMap<String, mpsc::Sender<String>>>,
```

  并在 `with_static_tools` 初始化表加 `notify: Arc::new(DashMap::new()),`。
  ③ `impl McpProxy` 加两个方法(`detach` 之后):

```rust
    /// 订阅某 token 的服务端通知流(GET SSE handler 调用)。同 token
    /// 重订 = 覆盖旧 sender(旧流随之收尾),与 claude 重连语义一致。
    pub fn subscribe(&self, token: &str) -> mpsc::Receiver<String> {
        let (tx, rx) = mpsc::channel(8);
        self.notify.insert(token.to_string(), tx);
        rx
    }

    /// 向路由到 `session_id` 的所有 token 的订阅流推一条
    /// list_changed(attach/detach 时机)。没有订阅流(claude 未开
    /// GET,见 D8 验证项)→ 静默丢弃。
    pub fn notify_list_changed(&self, session_id: Uuid) {
        for e in self.routes.iter() {
            if *e.value() == session_id {
                if let Some(tx) = self.notify.get(e.key()) {
                    let _ = tx.try_send(LIST_CHANGED_FRAME.to_string());
                }
            }
        }
    }
```

  ④ `detach` 末尾(`self.fail_pending(...)` 之后)加:

```rust
        // 工具真实可用性变了:促使 claude 重拉 tools/list(spec 降级③)。
        self.notify_list_changed(session_id);
```

  ⑤ `serve` 中把 `.get(|| async { StatusCode::METHOD_NOT_ALLOWED })` 整体替换为:

```rust
            .get(
                |Path(token): Path<String>, State(st): State<McpProxy>| async move {
                    // streamable-HTTP 的可选服务端流:claude 若打开 GET,
                    // 我们经由它推 list_changed。未注册 token → 405
                    // (「不提供流」;GET 非 2xx 不触发 OAuth 误判 ——
                    // 那个坑只在 POST,且 405 是 M1 验证过的安全形态)。
                    if st.session_for(&token).is_none() {
                        return StatusCode::METHOD_NOT_ALLOWED.into_response();
                    }
                    let rx = st.subscribe(&token);
                    let stream = futures::stream::unfold(rx, |mut rx| async move {
                        rx.recv().await.map(|m| {
                            (
                                Ok::<_, std::convert::Infallible>(
                                    axum::response::sse::Event::default()
                                        .event("message")
                                        .data(m),
                                ),
                                rx,
                            )
                        })
                    });
                    axum::response::sse::Sse::new(stream)
                        .keep_alive(axum::response::sse::KeepAlive::default())
                        .into_response()
                },
            ),
```

  ⑥ `crates/agent/src/pty.rs` 的 `open_session` 中,Task 13 的 attach 标记块整体替换为:

```rust
        // attach 跟踪 + 通知(Phase E):capable client 上线 → 转发模式;
        // 任何一次 open 都把「工具真实可用性可能变了」翻译成
        // list_changed(无 GET 订阅流时为无害 no-op)。
        if remote_mcp_capable {
            self.mcp.set_attached(session_id);
        }
        self.mcp.notify_list_changed(session_id);
```

- [ ] **E15-4 跑测试确认通过**:`cargo test -p cloudcode-agent list_changed && cargo test -p cloudcode-agent sse_get_stream && cargo test -p cloudcode-agent notify_without` 全 PASS;`cargo test --workspace` 全绿;`cargo clippy --workspace -- -D warnings` 无告警。
- [ ] **E15-5 手动验证清单(D8,不进 CI;在本机或 dev 环境跑一遍并把结果记入 commit message 或 PR 描述)**:
  1. 起 dev hub + agent(`[remote_mcp]` 全默认);本地 `CC_REMOTE_MCP_BACKEND="node <仓库绝对路径>/test-fixtures/echo-mcp.mjs" cloudcode` 连入,开一个 workspace。
  2. **管道冒烟**:在 claude 里让它调用 cc-browser 的 `echo` 工具(如"用 echo 工具回显 hello")→ 应得到 `echo: hello`。
  3. **无 client 降级**:`Ctrl+C` 退出 CLI,在 webterm attach 同一 workspace,让 claude 再调 echo → claude 应收到 -32004 错误并转述"请打开 cloudcode CLI"。
  4. **断连快失败**:让 claude 发起调用的同时杀掉 CLI → 在飞调用应在 ~1s 内收错,而非等满 120s。
  5. **list_changed 消费验证(核心未验证假设)**:webterm-only 状态下问 claude 可用工具(空表/静态表)→ 重开 CLI client → 不重启 claude,观察它是否自动看到 echo 工具(可再问一次工具表,或在 agent 日志确认 GET SSE 订阅与通知推送、随后有无新的 `tools/list` POST)。
     - **若 claude 消费**:记录"验证通过",计划②可依赖该机制。
     - **若不消费**(无 GET 订阅或收通知后不重拉):记录实测行为;兜底已就位——`-32004` 文案本身引导"连接后重试",且 claude 重试 `tools/call` 不依赖 tools/list 刷新(strict 注入的工具表来自 fallback/后端,调用透传)。在 spec 开放问题 4 处补记结论,计划②的 manifest 策略据此调整。**两种结果都不阻塞本计划合并。**
- [ ] **E15-6 commit**:

```bash
git add crates/agent/src/mcp_proxy.rs crates/agent/src/pty.rs
git commit -m "agent: tools/list_changed over GET SSE on attach/detach

McpProxy keeps one notification stream per token (token outlives
session_ids across reattaches); GET /mcp/:token upgrades to an SSE
stream (unknown token stays 405). Every session open and every detach
pushes notifications/tools/list_changed so claude can re-pull the real
tool set; with no subscriber it degrades to a silent no-op. Whether
claude consumes the notification is verified manually per the plan's
checklist (fallback wording already covers the negative case).

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

## spec 覆盖对照(收尾自查表)

| spec 条目 | 落点 |
|-----------|------|
| 组件 1 agent 远程-MCP proxy 垫片 | Task 9(核心)/ 10(接线)/ 12(端到端) |
| 组件 2 agent pty spawn 注入(strict + 引导 prompt + 绝不写全局) | Task 11;Task 14 翻转为始终注入 |
| 组件 3 hub 远程-MCP 帧中继 | Task 1/2(帧)+ Task 3/4(路由) |
| 组件 4 client 通用 MCP 宿主 | Task 5/6/7(宿主)+ Task 8(接线)+ Task 14(握手缝合) |
| 组件 5 client 托管 Chrome 设施 | → 计划②(本计划后端 = 任意命令,attach 方式后端自理) |
| 组件 6 `[browser]` 配置段 | → 计划②(本计划:client 用 `CC_REMOTE_MCP_BACKEND`,agent 用 `[remote_mcp]`,见 D9/D10) |
| 组件 7 声明式工具 manifest | Task 14 通用机制(`tools_manifest` 静态表);dev-browser 内容 → 计划② |
| 降级① 始终广告 | Task 14 |
| 降级② 无 client 调用 → 可执行 JSON-RPC 错误 | Task 14(-32004) |
| 降级③ attach/detach → list_changed | Task 15(+ D8 手动验证) |
| 降级④ 永不阻塞(分层超时 / fail-pending) | Task 9(三档超时)+ Task 10(RemoteMcpClosed→fail_pending)+ Task 13(detach 快失败) |
| 隔离:进程级配置 / 沙箱 HOME / 会话级端点 | Task 11(strict、0600、绝不全局)/ 既有 SandboxExec(不动)/ Task 9-11(token→session 路由) |
| 「请用户接管」工具、登录/人工交接 | → 计划②(Task 9 的 `LONG_CALL_TOOLS` 与 -32003 码位已预留) |
| 发布:三端随版、跨版本宽容 | D4/D5/D6 + Task 1/2;发布顺序 hub 先于 agent |

