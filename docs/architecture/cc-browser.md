# cc-browser 架构与不变量

> 远程浏览器(local browser)子系统的"真相源"。碰这块代码之前**先读本文**。
> 维护约定:改了链路/协议/超时/帧格式,**顺手改本文**——它和代码同寿命。
> 末次核对:2026-06-14,对应版本 `1.29.0`。

---

## 0. 一句话

claude 跑在 **agent** 机上,用户的浏览器跑在 **client** 机上。`cc-browser` 让 claude 调用的浏览器 MCP 工具,**穿过 hub 隧道**,落到用户机器上一个**有头 Chromium**。中间没有任何一层理解工具语义,全是"按行分隔的 JSON-RPC over stdio / WS"的透明转发。

---

## 1. 拓扑与双后端

```
┌─ agent 机 ───────────────────────────┐      ┌─ hub(公网中继)─┐      ┌─ client 机(用户)──────────────┐
│ claude ──stdio/HTTP──▶ mcp_proxy      │      │                  │      │ relay ──▶ McpHost              │
│ (axum, 127.0.0.1:7110)                │◀WS──▶│  dumb relay      │◀WS──▶│   └─ McpProcess (stdio 泵)     │
│                                       │      │ (零解析转发)     │      │       └─ npx @playwright/mcp   │
└───────────────────────────────────────┘      └──────────────────┘      │            └─ 有头 Chromium     │
                                                                          │               (持久 profile)    │
                                                                          └─────────────────────────────────┘
```

两个浏览器后端,claude 按工具名二选一(`guidance_prompt`,`mcp_proxy.rs`):

| 后端 | 跑在哪 | 头 | 走隧道? | 默认 | 用途 |
|---|---|---|---|---|---|
| `web` | **agent 本地** | 无头 | ❌ 不走 | ✅ 是 | 快、自动化、agent 自给自足 |
| `cc-browser` | **client 本地** | 有头 | ✅ 走 | 否(显式) | 用户要"看见"、要复用本机登录态 |

- 服务名常量:`CC_BROWSER_SERVER="cc-browser"` / `WEB_SERVER="web"`(`agent/src/mcp_proxy.rs:33,37`)。
- playwright-mcp **pin 死**:`@playwright/mcp@0.0.76`,agent 和 client 两侧各有一份同值常量(`mcp_proxy.rs:42` / `mcp_host.rs:28`)。两台机器、多次 `npx` 拉取行为必须一致。
- 计划① 只有**一个** cc-browser 插槽(one server per session)。未知 server 名 client 立即回错(`relay.rs:258`)。

---

## 2. 完整数据流(一次 `browser_navigate` 的来回)

**正向(claude → Chromium):**
1. claude 把 MCP 请求 POST 到 agent 进程内 `mcp_proxy`(`127.0.0.1:7110/mcp/<token>`)。
2. `handle_post`(`mcp_proxy.rs:532`)按 `(session_id, "cc-browser", id)` 在 `pending` 里挂一个 `oneshot`,把原文塞进 `ClientMsg::RemoteMcp{session_id, server, payload}` 发给 hub,然后**阻塞等响应**(超时见 §4)。
3. hub 零解析转发 → client `relay.rs` 收到 `HubToClient::RemoteMcp`(`relay.rs:257`)→ 喂给 `McpHost::feed`。
4. `McpHost` 把帧 `feed` 进 `McpProcess` 的 stdin(`mcp_host.rs:294`)→ playwright-mcp → Chromium 执行。

**回程(Chromium → claude):**
5. playwright-mcp 从 stdout 吐响应 → `McpProcess::next_frame`(`mcp_host.rs:302`)→ `from_process` 泵(`mcp_host.rs:522`)。
6. 泵把帧经 `host_out_tx` → `relay.rs` → `ClientToHub::RemoteMcp` → hub → agent `ws.rs:190` → `resolve_response(session_id, server, payload)`(`mcp_proxy.rs:305`)按 `(session, server, id)` **配对**唤醒第 2 步的 `oneshot`。
7. `handle_post` 拿到响应,回给 claude。

**配对键贯穿全链路 = `(session_id, server, JSON-RPC id)`。** id 错位 / server 名不符 / 反向请求被当响应,都会在第 6 步变成"孤儿",`pending` 等不到 → 超时。

---

## 3. ⚠️ MCP 是双向的(本子系统最重要的不变量)

JSON-RPC over MCP 有**三种**消息,靠 `id`/`method` 字段区分:

| 类型 | 有 `id`? | 有 `method`? | 谁发 | 隧道必须 |
|---|---|---|---|---|
| **请求 request** | ✅ | ✅ | 双向 | 转发 + 等配对响应 |
| **响应 response** | ✅ | ❌ | 双向 | 按 id 配对唤醒 |
| **通知 notification** | ❌ | ✅ | 双向 | 单向投递,不等回 |

**题眼:server(playwright-mcp)也会向 client 发"请求"(server→client reverse request)。** 最典型的是 `roots/list`——当 claude 的真 `initialize` 声明了 `roots` 能力时,playwright-mcp 收到 `browser_navigate` 后会**反问** client 要工作区根目录。

> **不变量 INV-1:client 侧必须就地应答 server 发起的反向请求,绝不把它丢进回程隧道。**
> 隧道的 `resolve_response` 只认"响应"(按 id 配对 pending)。一个 server 发起的"请求"被塞进回程,在 agent 侧就是个**没人等的孤儿**(`id=0 had no pending waiter`),被丢弃;而 playwright-mcp 等不到回答,会**干等满自己的 60s 超时**才继续。

实现:`server_request_reply`(`mcp_host.rs:412`),在 `from_process` 泵转发前拦截(`mcp_host.rs:538`):
- 有 `method` **且**有非空 `id` → 是反向请求 → 就地回:`roots/list` 回空 `{"roots":[]}`,其余回 `-32601`(不支持)。
- 无 `method`(响应)/ 无 `id`(通知)→ 返回 `None` → 照常转发。

---

## 4. 超时矩阵(agent 侧,`mcp_proxy.rs:324-350`)

| 档 | 值 | 触发 | 选档逻辑 `timeout_for` |
|---|---|---|---|
| `REQUEST_TIMEOUT` | 25s | `initialize` / `tools/list` 等握手类 | 命中即用 |
| `CALL_TIMEOUT` | 120s | 普通 `tools/call`(含 `browser_navigate`) | 默认 |
| `LONG_CALL_TIMEOUT` | 600s | `LONG_CALL_TOOLS` 名单内的工具 | 名单命中 |

- `LONG_CALL_TOOLS` 当前**只有** `["request_handoff"]`(`mcp_proxy.rs:334`)。`browser_navigate` 走 120s。
- client 侧 `start_replayed` 的握手重放有独立 60s 吞响应死线(`mcp_host.rs:496`)——**注意它和 §3 的 playwright-mcp 自有 60s 是两回事**,排查时别混。

---

## 5. 关键不变量清单(踩坑预防)

- **INV-1 反向请求就地应答**(见 §3)。新增任何 server→client 请求类型,要在 `server_request_reply` 加分支,否则后端干等超时。
- **INV-2 配对键三元组** `(session_id, server, id)` 全链路一致。改帧格式先想清楚 id 怎么穿。
- **INV-3 playwright-mcp 版本两侧同 pin**。改 `PLAYWRIGHT_MCP_PKG` 必须**两个文件一起改**(`mcp_proxy.rs:42` + `mcp_host.rs:28`)。
- **INV-4 进程组收尸**。后端按 `process_group(0)` 设成组长,`kill_process_group`(`mcp_host.rs:205`)SIGKILL 整组(npx + node + Chromium)。**playwright-mcp 父死不自退**,只能靠这条;client 被强杀时收尸不跑 → 泄漏(见 §7)。
- **INV-5 持久 profile 单例锁**。spawn 前 `purge_singleton_locks`(`mcp_host.rs:189`)清孤儿残留的 SingletonLock,否则走 ~21s 单例重试(干净 profile 仅 ~0.6s)。
- **INV-6 未知 token/server 回 JSON-RPC error,绝不 404**(`mcp_proxy.rs` 各分支)。claude 的 MCP client 不认 HTTP 404。

---

## 6. 这次 60s 卡顿复盘(为后人留档)

**现象:** cc-browser 首次 `browser_navigate` 稳定多花 ~60s(极端报 705s/10min)。

**根因:** 违反 INV-1。playwright-mcp 收到 navigate 后反向发 `roots/list`(id=0),隧道把它当响应丢进回程 → agent `id=0 had no pending waiter` 丢弃 → playwright-mcp 干等满自己 60s 超时(`notifications/cancelled`)才完成 navigate。

**为何探针测不出:** `scripts/mcp-nav-probe.mjs` 的 `initialize` 用**空 capabilities**,不声明 roots → 不触发 `roots/list`。**探针"复现不出"本身就是线索**:探针与真 claude 的唯一差异 = capability 声明。

**修复:** `server_request_reply`(`mcp_host.rs:412`),纯 client 侧,零协议改动。navigate 从 ~63s 降到 ~8s。

**排查为什么折腾了 15+ 轮(元教训):**
1. **client 当时没有 tracing subscriber**,`mcp_host` 所有日志是空操作——对着哑巴调试。已修:`CLOUDCODE_LOG` 文件日志(见 §8)。
2. **日志 EnvFilter crate 名写错**:client 二进制 crate 名是 `cloudcode`(来自 `[[bin]] name`),不是 `cloudcode_client`,逐帧 debug 一度被全滤掉,白费一轮。
3. **手搓 shell 探针抢写丢消息**:`printf|npx` 会在 server 挂上 stdin 读取器前抢写。测 MCP server 要用 node spawn 显式 `stdin.write`。
4. **假设了错的 60s**:一直盯 `start_replayed` 的 60s(§4),真凶是 playwright-mcp 自有的 60s(§3)。

➡️ **一句话教训:透明代理/桥接组件,出厂就该有"每帧带 `方向+id+method`"的边界日志。** 有它,这个 bug 第一份日志就现形,15 轮压成 1 轮。

---

## 7. 已知遗留问题

**playwright-mcp 进程泄漏(未修)。** 收尸(INV-4)只在 client **优雅退出**时跑;用户卡死后强杀 CLI → 整批 Chromium/node 漏成僵尸,跨天堆积。
**待修方案:** spawn 前按专属 `--user-data-dir` reap 存量进程 + client 装 `SIGINT`/`SIGTERM` handler 兜底收尸。

---

## 8. 可观测性开关

- **client 日志:** `CLOUDCODE_LOG=1` → `<state_dir>/client.log`。看逐帧用 filter `info,cloudcode=debug`(**不是** `cloudcode_client`)。逐帧日志认 `→ playwright-mcp stdin` / `← playwright-mcp stdout`,带 `id` `method`。
- **后端 stderr:** 同开关下落 `<state_dir>/playwright-mcp.log`(= `--user-data-dir` 的父目录)。
- **agent 日志:** `handle_post` 与 `resolve_response` 的 debug 带 `id`/`method`;孤儿响应打 `had no pending waiter`。
- **独立复现:** `scripts/mcp-nav-probe.mjs`(node 驱动 playwright-mcp,逐帧计时)。**注意它默认空 capabilities,复现不出 §3 的反向请求**;要复现需让它声明 roots 能力。

---

## 9. 关键文件索引

| 文件 | 职责 |
|---|---|
| `crates/agent/src/mcp_proxy.rs` | agent 侧 axum MCP 端点(:7110)、超时选档、pending 配对、fallback |
| `crates/agent/src/ws.rs` | agent↔hub,`ServerMsg::RemoteMcp` → `resolve_response` |
| `crates/agent/src/config.rs` | `RemoteMcpConfig`(默认端口 7110) |
| `crates/client/src/mcp_host.rs` | client 侧 spawn/stdio 泵/握手重放/**反向请求应答**/收尸 |
| `crates/client/src/relay.rs` | client↔hub,`HubToClient::RemoteMcp` → `McpHost::feed` |
| `crates/client/src/proto.rs` | `ClientToHub` / `HubToClient` 帧定义 |
| `crates/hub/src/tunnel.rs` | hub 零解析中继 |
| `scripts/mcp-nav-probe.mjs` | 独立复现/计时脚手架 |
