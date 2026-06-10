# Desktop App P1 — agent 浏览器底座 实现计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development. Steps use checkbox syntax.

**Goal:** claude 的浏览器自动化完全在 agent 本地闭环:Chrome 常驻实例(localhost CDP)+ playwright-mcp 以 `--cdp-endpoint` 连同一实例 + mcp_endpoint 短路接本地子进程。不涉及 hub/client/投屏。

**Architecture:** claude →(localhost HTTP MCP)→ mcp_endpoint(移植自 feature/local-browser,POST-blocking + id 关联保留)→ 本地 playwright-mcp 子进程(stdio 管道,移植 client 的 cc_browser 机械)→ --cdp-endpoint → 常驻 Chrome(新监督模块)。注入门控从 browser_capable 改为 agent 配置 `browser.enabled`。

**基线注意:** 本分支无任何 M1-M3 代码。"移植"= 从 `feature/local-browser` 分支取(`git show feature/local-browser:<path>`),按 P1 架构改造,不是 cherry-pick。

**验收(spec P1):** 现有 CLI/webterm 进 claude 让它开网页,全程不经 client;重开 session 登录态仍在。

---

## Task 1: 风险验证 spike(spec 首要风险,本机可直接验)

不写产品代码。在本机(有 node、npx 缓存有 @playwright/mcp@0.0.76)验证:
1. 找到/确认 Chrome 二进制(macOS `/Applications/Google Chrome.app/...` 或 `npx -y @playwright/mcp@0.0.76 install-browser` 的 chromium)。
2. 起 `chrome --headless=new --remote-debugging-port=19222 --user-data-dir=/tmp/cc-p1-spike --no-first-run`。
3. 起 `npx -y @playwright/mcp@0.0.76 --cdp-endpoint http://127.0.0.1:19222`(stdio),喂 initialize → tools/call browser_navigate(example.com)→ tools/call browser_snapshot。
4. 断言:navigate/snapshot 成功且 Chrome 是我们起的那个(`curl 127.0.0.1:19222/json` 看到 example.com 的 target)。
5. 附验:杀 playwright-mcp 重起(同 --cdp-endpoint),不重启 Chrome,再 navigate —— 验证"playwright-mcp 重启不丢浏览器"(短路层握手回放后自动化可续)。
6. 把命令与结论写进 `docs/superpowers/plans/2026-06-10-p1-spike-notes.md` 提交。若失败:按 spec 降级方案(playwright-mcp 自管浏览器+其 Chrome 开 remote-debugging)验证替代路径并记录,后续任务按降级路径调整。

## Task 2: 移植测试夹具 + agent 侧子进程管道

- `test-fixtures/echo-mcp.mjs`:原样移植。
- 新建 `crates/agent/src/browser/subprocess.rs`:从 `feature/local-browser:crates/client/src/cc_browser.rs` 移植 `McpProcess`(spawn/feed/next_frame/shutdown,kill+wait,stdin-close watchdog 注释一并搬)。**不移植** BrowserChannel/restart/握手缓存(P1 不需要 headed 切换;claude 与子进程同生命周期,握手由 claude 自己发)。`mod browser { pub mod subprocess; }` 挂入 main.rs。
- 移植对应测试(echo 桩往返,fixture 路径按 agent crate 调整)。
- `cargo test -p cloudcode-agent` 绿,提交。

## Task 3: Chrome 常驻管理

新建 `crates/agent/src/browser/chrome.rs`:
- `ChromeManager`:按配置定位二进制(`browser.chrome_path` 显式 > macOS/Linux 常见路径探测);spawn 参数:`--headless=new --remote-debugging-port=<browser.cdp_port,默认19222,仅随 agent 配置> --user-data-dir=<state_dir>/browser-profile --no-first-run --no-default-browser-check`;监督:子进程退出即重启(指数退避,上限);`cdp_http_url()` 供 playwright-mcp 与(P2)投屏模块使用;就绪探测:轮询 `GET /json/version` 直到 200。
- 配置(config.rs):`[browser] enabled(默认 false)、chrome_path、cdp_port`。
- 测试:参数构造纯函数单测;就绪探测对假 HTTP server 单测;真 Chrome 集成测试 `#[ignore]`(本机手动跑一次验证,CI 不依赖)。
- 提交。

## Task 4: mcp_endpoint 移植 + 短路改造

- 移植 `feature/local-browser:crates/agent/src/mcp_endpoint.rs` → 保留:axum 路由(POST /mcp/:token、GET 405、healthz)、PostOutcome(JSON-RPC error at 200 的教训!)、`timeout_for` 3 档、`jsonrpc_error`、`extract_id_key`、pending(session_id,id)→oneshot 关联、token routes、`mcp_config_json`(type:http)。
- **改造**:删去 to_hub/send_to_hub/resolve_response-from-ws;新增 per-session 的本地子进程绑定:`EndpointState` 持 `sessions: DashMap<Uuid, SessionBrowser>`,其中 SessionBrowser = playwright-mcp 子进程(McpProcess)+ 读泵 task(stdout 帧 → `resolve_response(session_id, frame)`,机制照旧)。`handle_post` 请求路径:确保该 session 的子进程已起(懒启动:spawn `npx -y @playwright/mcp@0.0.76 --cdp-endpoint <chrome.cdp_http_url()>`,可被 `browser.mcp_command` 配置覆盖、测试用 echo 桩)→ feed → 等 oneshot(超时档照旧)。
- per-session 子进程(而非全局单个):每个 workspace 的 claude 各自一个 playwright-mcp(隔离 MCP 会话状态),共享同一个 Chrome(--cdp-endpoint 同地址)—— tab 隔离 P2+ 再说,V1 接受共窗。
- 移植/适配测试:unknown token、notification 202、request-blocks-then-resolves(响应源换成 echo 桩子进程,真往返!)、timeout JSON-RPC error、3 档 timeout、id_key。新增:session 子进程懒启动、子进程死亡 fail_pending(进程 EOF → 该 session pending 全失败,机制移植 fail_pending)。
- main.rs:AppState.mcp + serve() spawn(移植);ChromeManager 实例注入 EndpointState(拿 cdp url)。
- 提交。

## Task 5: claude 注入 + 配置门控

- 移植 pty.rs 注入块(workspace_tokens 稳定 token、mcp-browser.json 0600 原子写、--mcp-config 追加 claude_args、token 自愈读回、reserve/register),门控改 `self.browser_enabled`(从 config 传入 PtyManager,替代 browser_capable —— **不做** Hello/PtyOpen 协议字段,P1 零协议改动)。
- 移植对应测试(config json、token 提取/校验、workspace_tokens 生命周期相关单测按新结构适配;delete/reset unregister 钩子一并移植)。
- 提交。

## Task 6: 集成验证 + 冒烟文档 + 收尾

- 集成测试:echo 桩当 mcp_command 覆盖,真 HTTP POST 到端点 → 懒启动子进程 → 往返(移植 real_http 测试改造)。
- `#[ignore]` 全链路测试:真 Chrome(Task 3 manager)+ 真 playwright-mcp(--cdp-endpoint)+ 端点 POST navigate → 200 含结果。本机跑一次记录输出。
- 冒烟文档 `2026-06-10-desktop-app-p1-e2e-smoke.md`:用户在自己环境验收(agent 配 browser.enabled=true → CLI/webterm 进 claude → 开网页 → 不经 client;关 session 重开 → 登录态在)。
- `cargo test --workspace` 全绿、零警告;push。

## Self-Review 备忘

- P1 零协议改动(hub/client/proto 不动)、零投屏 —— 范围铁律。
- M3 的握手回放在 P1 不需要(子进程与 claude session 同生共死);P4 viewer 或 Chrome 崩溃恢复需要时再移植 —— 已记录于 spec 错误处理表。
- 风险:spike(Task 1)失败 → 降级路径在任务内文档化,Task 4 的 mcp_command 默认值跟着变(去掉 --cdp-endpoint,Chrome 由 playwright-mcp 自管 + ChromeManager 退化为只管投屏用的 CDP 地址发现)。
