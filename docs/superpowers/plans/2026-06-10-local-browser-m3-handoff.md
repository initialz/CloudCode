# 云端 claude 操作本地浏览器 — M3 人工接管 + 收尾 实现计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** claude 遇到登录/验证码时能把浏览器交给真人(headless→headed 切换),人工完成后交还;附带 install.sh 预装、agent 重启 token 自愈、若干 M2 审查遗留。

**Architecture:** 黑盒 playwright-mcp 下的 handoff 用**重启切换**实现:client 缓存 claude 的原始 MCP 握手帧(initialize 请求 + initialized 通知),`request_handoff` 到来时停掉 headless 子进程、同 profile 启 headed、**回放握手**、等人工完成(TUI pill 按回车),再切回 headless 同样回放,最后给 claude 回 `request_handoff` 的成功结果(并提示重新导航;登录态经持久 profile 延续)。`request_handoff` 工具靠 client 改写 `tools/list` **响应**注入(需要 client 跟踪在飞请求 id→method),其调用被 client 截下本地处理、不进 playwright-mcp。

**与 spec 的偏离(留痕):** spec 原定"始终有头+移屏外+bringToFront(零状态丢失)",前提是自有 Playwright 驱动;M2 已定 playwright-mcp 黑盒(无窗口管理、跨平台搬窗不可靠),故改为重启切换。代价 = 切换时丢页内状态(表单填写),但 profile 级状态(cookie/登录)保留 —— 而登录正是 handoff 的核心场景,可接受。

**分支:** `feature/local-browser-m3`(基于 m2)。

**M3 不含:** webterm 浏览器通道 —— 经查 webterm 不发 `browser_capable` → serde 默认 false → agent 不注入 → claude 在 webterm 会话里根本没有浏览器工具,行为已是干净降级,无需实现(永久限制,文档化);产物自动上传 workspace(好特性,但留 M4/后续);显式撤销 UI。

---

## 摸查事实(写计划时已核实)

- playwright-mcp 默认 **headed**,`--headless` 是显式 flag → headed 启动 = 同命令去掉 `--headless`。无 `--headed` flag。
- 同一 `--user-data-dir` 单实例锁:重启前必须确保旧进程完全退出(`McpProcess::shutdown` 是 `start_kill`,需等 wait 退出后再启新的,否则 "Browser is already in use" )。
- MCP 协议要求 `initialize` 先行;claude 不会对存活的 MCP 连接重发握手 → 重启后必须由 client 回放缓存的握手。
- client 已有的拦截基建:relay 的 `method_is_passive` 已解析 payload 的 `method`;`BrowserChannel`(cc_browser.rs)是 in_tx + 后台泵;响应经 `browser_out_rx` 回流。
- agent 端点超时:`timeout_for` 已按 method 区分(tools/call 120s)。handoff 含人工登录,120s 不够。
- agent 重启丢 token:`workspace_tokens` 与 endpoint `routes` 均内存态;但 `mcp-browser.json` 文件还在且 claude(tmux 里活着的)用的就是文件里那个 token → agent 重启后 open_session 读回文件即可自愈。

## File Structure

| 文件 | 改动 | 职责 |
|------|------|------|
| `crates/client/src/cc_browser.rs` | 改 | `BrowserChannel` 支持 restart(等旧进程退净)+ 握手缓存/回放;`mcp_command_headed()` |
| `crates/client/src/relay.rs` | 改 | id→method 在飞跟踪;tools/list 响应注入 request_handoff;拦截其调用;handoff 交互流(两段 pill);启发式密码框提醒 |
| `crates/agent/src/mcp_endpoint.rs` | 改 | `timeout_for` 对 `request_handoff` 调用放宽到 600s |
| `crates/agent/src/pty.rs` | 改 | open_session 读回已有 mcp-browser.json 自愈 token;(顺带)spawn 失败时… |
| `install.sh` | 改 | client 分支预装/预热 |
| `docs/superpowers/plans/2026-06-10-local-browser-m3-e2e-smoke.md` | 新建 | M3 冒烟 |

---

## Task 1: BrowserChannel 握手缓存 + 重启回放

**Files:** `crates/client/src/cc_browser.rs`

- [ ] `McpProcess::shutdown` 改为 kill 后 `child.wait().await`(确保 profile 锁释放);加 `pub async fn wait_exit(mut self)` 形式按现有结构调整。
- [ ] `BrowserChannel` 增加:
  - `handshake: Arc<Mutex<Vec<String>>>` —— start 后,前两条**入站**帧若 method 为 `initialize` / `notifications/initialized` 则缓存(由 relay 喂之前调用 `maybe_cache_handshake(&frame)`,或在 `feed` 内部探测;实现取其简)。
  - `pub async fn restart(self, program: &str, args: &[&str], out_tx, on_replay_swallow: …) -> io::Result<BrowserChannel>`:停旧(kill+wait)→ spawn 新 → 依次 feed 缓存的握手帧,**吞掉** initialize 的响应(按 id 匹配,不送 out_tx)→ 返回新 channel(携带同一份握手缓存)。回放吞应答的实现:重启期间先用临时 mpsc 收新进程输出,匹配吞掉 initialize 响应后再把泵切到正式 out_tx;或泵任务内置 `swallow_ids: HashSet<String>`。取实现最简者,加注释。
- [ ] 测试(echo 桩即可验证回放管线):start → feed initialize(被缓存)→ restart(同 echo 桩)→ 断言 restart 后 out_rx 没收到回放的 initialize 响应(被吞)→ feed tools/list → 正常收到响应。
- [ ] `mcp_command_headed()`:与 `mcp_command()` 同源,去掉 `--headless`(env 覆盖时:`CC_BROWSER_MCP_HEADED` 可选覆盖,缺省 = 把默认命令的 --headless 移除;env 自定义命令时 headed 版本 = 原样去掉 "--headless" token,找不到就原样)。
- [ ] `cargo test -p cloudcode-client` 绿;提交 `feat(client): browser channel restart with handshake replay`。

## Task 2: request_handoff 注入与拦截

**Files:** `crates/client/src/relay.rs`

- [ ] **在飞 id→method 跟踪**:relay 喂子进程前,对非 passive 之外也记录:`inflight: HashMap<String /*id key*/, String /*method*/>`(id 提取仿 agent 端 `extract_id_key` 语义,写个本地 helper + 测试)。`browser_out_rx` 收到响应时 remove。
- [ ] **tools/list 响应注入**:出站帧若其 id 对应 method==`tools/list`,解析 JSON,往 `result.tools` 数组 append:

```json
{"name":"request_handoff","description":"Hand the browser to the human user (visible window) for login/CAPTCHA or anything requiring manual action. The browser restarts headed; in-page state is lost but cookies/logins persist. After the human finishes, the browser returns headless and you should re-navigate to continue.","inputSchema":{"type":"object","properties":{"reason":{"type":"string","description":"Why the human is needed (shown to them)"}},"required":["reason"]}}
```

  重序列化回传。解析失败则原样放行(防御)。
- [ ] **拦截调用**:入站 `tools/call` 且 `params.name=="request_handoff"` → 不 feed 子进程,进 Task 3 的 handoff 流;其余照旧。
- [ ] 测试:注入(构造 tools/list 响应 JSON → 断言 append 后合法且含 request_handoff)、拦截判定 helper。
- [ ] 提交 `feat(client): inject request_handoff tool into tools/list, intercept its calls`。

## Task 3: handoff 交互流

**Files:** `crates/client/src/relay.rs`、`crates/agent/src/mcp_endpoint.rs`

- [ ] client 流程(拦截到 request_handoff 调用后):
  1. BEL + pill:`云端请求人工接管浏览器: <reason> — [y]打开窗口 / [n]拒绝`(复用 scan_consent_chunk)。
  2. n → 给 hub 回一条出站 BrowserRpc:`{"jsonrpc":"2.0","id":<原id>,"error":{"code":-32003,"message":"user declined handoff"}}`(client 直接构造响应,本就是这次调用的服务方)。
  3. y → `browser.restart(headed)`(Task 1)→ pill 换文案:`浏览器已切到可见窗口,完成人工操作后按回车交还`→ 等回车(扫 \r/\n;其余吞)→ `browser.restart(headless)` → 回成功响应:`{"jsonrpc":"2.0","id":<原id>,"result":{"content":[{"type":"text","text":"Human finished. Browser is headless again; cookies persisted. Re-navigate to continue."}]}}`。
  4. 全程 gate 不变(handoff 本身就是 tools/call,已过授权门)。
- [ ] agent:`timeout_for` 细化 —— `tools/call` 且 `params.name=="request_handoff"` → 600s;其余 tools/call 维持 120s。测试更新。
- [ ] 测试:client 侧响应构造 helper 的 JSON 合法性;agent timeout_for 三档。
- [ ] 提交 `feat: human handoff flow (headed switch, 600s window)`。

## Task 4: install.sh 预装 + 启发式提醒 + spawn 快速失败

**Files:** `install.sh`、`crates/client/src/relay.rs`

- [ ] install.sh client 分支(`install_bin cloudcode` 后):若 `command -v node`,则 `npx -y @playwright/mcp@0.0.76 --version` 预热 + 提示可选 `npx -y @playwright/mcp@0.0.76 install-browser`(机器无 Chrome 时);无 node 则打印一句"浏览器通道需要 Node.js"继续不报错。
- [ ] 启发式(notify-only,履行 spec"启发式保底"的最小承诺):出站帧含 `"type":"password"` 或 `\"password\"` 特征且当前未在 handoff 中 → BEL + 单行提示(stdout 短 pill,2 秒级或下帧自清):`页面似乎需要登录 — 可让 claude 调 request_handoff`。不自动切换、不拦帧。简单字符串探测即可,加注释说明这是提醒不是判定。
- [ ] spawn 快速失败(M2 审查 LOW-2):allowed 分支里 spawn 失败/mcp_command 为 None 时,立即发 `ClientToHub::BrowserClosed{reason:"browser subprocess unavailable"}`,让 agent fail_pending,claude 不用干等 120s。
- [ ] 提交 `feat: install preflight, login-page hint, fast-fail on spawn error`。

## Task 5: agent 重启 token 自愈

**Files:** `crates/agent/src/pty.rs`

- [ ] open_session 的 browser_capable 分支:mint 前先尝试读 `<cwd>/.cloudcode/mcp-browser.json`,从中提取已有 token(简单字符串/serde 解析 url 尾段),存在则采用它(写回 workspace_tokens + register),不存在才 mint。这样 agent 重启后,tmux 里活着的 claude(手持文件里的旧 token)在下一次 reattach-open 时 token 即被重新注册 → 自愈。
- [ ] 测试:`extract_token_from_config(json)->Option<String>` 纯函数 + 往返(用 `mcp_config_json` 生成再提取)。
- [ ] 提交 `fix(agent): re-adopt persisted token on open so agent restart self-heals`。

## Task 6: M3 冒烟文档 + 收尾

- [ ] `docs/superpowers/plans/2026-06-10-local-browser-m3-e2e-smoke.md`:让 claude 去一个需要登录的站点 → claude 调 request_handoff → 终端响铃弹接管确认 → y → **可见 Chrome 窗口弹出** → 人工登录 → 回车 → 窗口消失(回 headless)→ claude 重新导航,已是登录态。decline 路径、600s 窗口、agent 重启自愈检查、install.sh 新装机预热。已知限制:切换丢页内状态(cookie 保留)、启发式仅提醒。
- [ ] `cargo test --workspace` 全绿、`cargo build --workspace` 零警告、push。

## Self-Review 备忘

- spec 偏离(重启切换 vs 常驻有头)已留痕并给出理由;`request_handoff` 注入/拦截位置与 spec 一致(client 侧、协议无感);600s 对应 spec"接管计时器";启发式降级为 notify-only 是 M3 范围内的诚实承诺,spec 的自动弹窗保底留待自有驱动时代。
- 回放吞响应是 Task 1 最微妙处,测试用 echo 桩覆盖;profile 锁要求 kill+wait 已写明。
- id→method 跟踪同时服务 Task 2 注入与 Task 3 拦截,一处实现。
