# cc-browser:浏览器预设(计划②/共二) — 设计

> 在计划①已真机验证的「远程-MCP 透明管道」上叠加**浏览器预设**:远程 claude 默认在**云端无头浏览器**(`web`,与 claude 同在 agent)里浏览——快、用户无感;仅当用户**明确要求**时才落到**用户本地的有头浏览器**(`cc-browser`,走计划①隧道到 client)——看得见、可登录、可亲手操作。两个后端同用 `@playwright/mcp` 引擎,只是位置与参数不同。透明化、零手动配置(但两台机器需 node)、真浏览器。

- **日期**:2026-06-13
- **分支**:基线 `feature/cc-browser`(计划①全量实现 + 真机验证);本计划在 `feature/cc-browser-preset` 上实现
- **状态**:设计已与用户敲定,待写实现计划

## 背景与动机

计划①(spec:`docs/superpowers/specs/2026-06-12-cc-browser-remote-mcp-design.md`;实现计划与决策记录 D1-D17:`docs/superpowers/plans/2026-06-12-cc-browser-remote-mcp-pipeline.md`)交付并验证了一条 backend 无关的管道:agent 上的 claude 经进程级 `--mcp-config` + `--strict-mcp-config` 连到 localhost proxy,帧经 hub 哑中继到 client,由 client 端 `McpHost` 拉起任意 MCP-over-stdio 后端。计划①刻意把"浏览器"留白:默认后端只是一个验证用 echo 桩(提交 `ee92506`),`[browser]` 配置段、托管 Chrome、「请用户接管」工具全部按决策 D17 推到本计划。

本计划填上这块留白,并吸收一个关键的用户洞察:**绝大多数浏览需求(查资料、读公开页面、抓数据)根本不需要用户看见**——在用户本地弹窗反而打扰;真正需要本地有头浏览器的,只有"用户要亲自登录/亲手操作"这一类**用户自己明确知道**的场景。因此默认走云端无头、用户明示才落本地。

这同时统一了历史上的两条线:desktop-app 线的"agent 无头浏览器"(其 CDP 投屏/合成输入已被否决,但"agent 上能跑浏览器"这一点被 `web` 后端继承)与 cc-browser 线的"本地有头浏览器"(计划①管道 + 本计划的持久 profile)。两者不再是竞争方案,而是同一预设下按场景分工的两个后端。

## 目标 / 非目标

### 目标

| # | 目标 |
|---|------|
| 1 | **默认无感**:网页浏览默认在 agent 上的无头浏览器完成,不弹任何本地窗口、不依赖 client 在线 |
| 2 | **明示才本地**:用户明确要求时,claude 用 `cc-browser` 在用户本地打开可见浏览器 |
| 3 | **真浏览器、零手动配置**:两个后端默认命令内置(`@playwright/mcp`),不写配置即可用;前提仅为两台机器装有 node |
| 4 | **持久登录**:本地有头浏览器带持久 profile(`--user-data-dir`),登一次长期有效 |
| 5 | **撤验证脚手架**:client 默认后端从 echo 桩(`ee92506`)换成真 playwright-mcp;`CC_REMOTE_MCP_BACKEND` 覆盖保留 |
| 6 | **管道零改动**:计划①的 proxy/隧道/McpHost/超时/降级全部原样复用 |

### 非目标(Non-Goals)

- **不做「请用户接管」/暂停工具(V1)**:不实现专门的接管工具,连计划①预留的 `-32003` 接管语义也不启用(码位继续保留)。撞登录墙 = claude 在对话里停下告诉用户、用户在 headed 窗口亲手操作、用户说"好了继续"——纯对话协调。
- **无 CDP 投屏 / 像素镜像**:沿计划①。
- **不做登录态跨机迁移**:`web` 与 `cc-browser` 是两台机器上两个独立浏览器、两份独立登录态;httpOnly cookie 无法跨机导出(playwright-mcp 黑盒硬限制,research 已证)——物理不可能,不是没做。
- **不做 headed⇄headless 中途切换**:选后端是任务级的,不能"无头做到一半带着进度和登录态切到本地"。
- **不自写 Rust CDP 后端**:留作后续优化;本计划两个后端都是 playwright-mcp 黑盒。
- **不做 agent 无头浏览器的可视化**:无头 = 不可见,本就无需。
- **不做 desktop-app 式 ChromeManager**:不自己起 Chrome 再让后端 attach;playwright-mcp 经 `--user-data-dir` 自管 Chrome 与 profile。

## 总体架构

```
┌─ agent 主机(云端)──────────────────────────────────────────────
│
│   claude(tmux 内;--mcp-config 含【两个】server + --strict-mcp-config)
│     │ │
│     │ ├──────────────► "web"(type stdio)
│     │ │                claude 直接 spawn:
│     │ │                npx -y @playwright/mcp@0.0.76 --headless
│     │ │                      │
│     │ │                headless chromium(agent 本机,沙箱内)
│     │ │                ——全程不出 agent,不碰隧道、不碰 proxy——
│     │ │
│     │ └──► "cc-browser"(type http,localhost)
│     ▼                  计划①远程-MCP proxy(原样复用)
│   ┌──────────────────────┐
└───┤ agent⇄hub WS 隧道    ├──────────────────────────────────────
    │ (RemoteMcp 帧,①已有)│
┌───┤ hub:哑中继(①已有)  ├──────────────────────────────────────
└───┤ client⇄hub WS(①已有)│
┌───┴──────────────────────┴─ client 主机(用户本地)──────────────
│   cloudcode CLI — McpHost(①已有;默认后端命令换掉)
│     │  MCP over stdio
│     ▼
│   npx -y @playwright/mcp@0.0.76 --user-data-dir=<持久路径>
│     │  (playwright-mcp 默认 headed,自管 Chrome + 持久 profile)
│     ▼
│   可见浏览器窗口:用户看得见、能亲手登录/操作;登录态持久
└────────────────────────────────────────────────────────────────
```

两条线的性质:

- **`web` 完全不经过我们的传输代码**。它只是注入配置里多出的一个 stdio server 条目——claude 自己 spawn、自己说 stdio MCP,浏览器与 claude 同机。我们的工作量 = 配置生成 + 引导 prompt。
- **`cc-browser` 就是计划①那条管道**,本计划唯一的改动是把 client 端默认后端命令从 echo 桩换成 playwright-mcp(`McpHost` 构造已按 D9 与命令来源解耦,接 `(String, Vec<String>)` 即可)。
- `--strict-mcp-config` 下 claude 恰好只看到这两个 server,同机其他 claude 进程零影响(计划① D11 铁律不变)。
- **webterm 边界的缓和**:计划①里 webterm-only 接入完全没有浏览器能力;本计划起,`web` 在 agent 本机执行、不依赖 client,webterm 用户也能获得默认的无头浏览——只有 `cc-browser` 仍要求原生 CLI client 在线(降级文案沿①)。

## 组件拆分

### 1. agent 注入双 server(`mcp_config_json` 扩展)

- **做什么**:把 `crates/agent/src/mcp_proxy.rs::mcp_config_json(port, token)` 生成的注入配置从一个 server 扩成两个:`web`(`"type":"stdio"`,`command`/`args` = web 后端命令)+ `cc-browser`(`"type":"http"`,指向 localhost proxy,与①字节语义一致)。签名相应扩展以接收 web 后端命令(具体形参在实现计划阶段确定)。
- **怎么用**:注入路径不变——`crates/agent/src/pty.rs` 的注入块(`should_inject_mcp` 门控 → 写工作区 `.cloudcode/mcp-remote.json`(0600,原子写)→ `claude_args.extend(claude_mcp_args(...))`)只是写出的 JSON 多一个条目。
- **依赖 / 兼容性**:工作区 token 自愈回采(`extract_token_from_config`)按 `mcpServers.cc-browser.url` 取 token,新增 `web` 键不影响它;旧 `mcp-remote.json`(单 server)在下一次 open 时被覆盖写为双 server,字节稳定性以新格式为新基线。`server` 字段早在帧上预留(D3),`web` 不走帧,无协议改动。

### 2. agent `web` 无头后端(claude 直 spawn)

- **做什么**:无新代码组件——`web` 后端是注入配置里的一个 stdio 条目,claude 直接 spawn,生命周期归 claude 管(随 claude 进程起灭)。默认命令 `npx -y @playwright/mcp@0.0.76 --headless`。
- **怎么用**:claude 调 `web` 的 `browser_*` 工具 → playwright-mcp 在 agent 本机驱动 headless chromium。每会话的 claude 跑在自己的 `SandboxExec --home` 内,chromium 临时文件/缓存落在每会话 HOME,会话间天然隔离。
- **依赖**:agent 机器需有 node(用户已接受);chromium 二进制(playwright 浏览器安装,见组件 6 与开放问题);**沙箱可运行性是本计划首要待验证项**(见「错误处理与降级」与开放问题)。

### 3. client `cc-browser` 默认后端(撤 echo 桩 → playwright-mcp 持久 profile)

- **做什么**:改写 `crates/client/src/mcp_host.rs::backend_command()` 的解析顺序为:env `CC_REMOTE_MCP_BACKEND`(保留,高级用户/测试)> `[browser].backend` 配置 > **内置默认** `npx -y @playwright/mcp@0.0.76 --user-data-dir=<持久路径>`。同时删除 `embedded_echo_backend()` 与 `EMBEDDED_ECHO_MCP`(撤销 `ee92506` 的生产路径;`test-fixtures/echo-mcp.mjs` 本体保留,集成测试继续用)。
- **怎么用**:playwright-mcp 默认 headed——用户本地弹出真实可见窗口;`--user-data-dir` 指向 client 端持久 profile 路径(缺省路径见组件 5),playwright-mcp **自己管理**该 Chrome 与 profile,我们不起 Chrome、不 attach。
- **依赖**:client 机器需有 node;`McpHost` 的惰性拉起/退避/握手缓存重放(①已有)原样复用——playwright-mcp 崩溃即按既有退避重启,持久 profile 使重启后登录态仍在。`Hello.remote_mcp_capable` 仍 = `backend_command().is_some()`;有了内置默认后,`[browser].enabled = true` 时恒为 true。

### 4. 引导 prompt:双后端选择逻辑

- **做什么**:替换 `crates/agent/src/mcp_proxy.rs::GUIDANCE_PROMPT` 为双后端版本(完整文案见「选择机制与引导 prompt」),经既有 `claude_mcp_args()` 的 `--append-system-prompt` 注入,机制不变。
- **核心规则**:默认一律 `web`;仅用户明示才 `cc-browser`;撞登录墙不自切、停下来问;任务级粘住一个后端;两后端状态不互通。

### 5. `[browser]` 配置段(client 侧 + agent 侧)

整段缺省 = 全默认零配置。pin 版本 `@playwright/mcp@0.0.76`(2026-06-10 实证最新)作为内置常量,随默认命令一起生效;字段名为示例,最终命名实现计划阶段确定。

**client 侧(cloudcode CLI 配置)**:

```toml
[browser]
enabled = true   # 默认 true;false ⇒ backend_command() 返回 None ⇒
                 # Hello 能力位 false,本机不提供 cc-browser
# backend 缺省 = 内置默认(npx -y @playwright/mcp@0.0.76 --user-data-dir=<profile_dir>)
# backend = "npx -y @playwright/mcp@0.0.76 --user-data-dir=/path --browser=firefox"
# profile_dir 缺省 = client 数据目录下的专用子目录(确切路径实现计划阶段定,
# 例如 ~/.cloudcode/browser-profile);仅在 backend 缺省时生效(显式 backend 全权自带参数)
# profile_dir = "/custom/profile"
```

| 字段 | 默认 | 语义 |
|------|------|------|
| `enabled` | `true` | 本机是否提供 cc-browser 能力 |
| `backend` | 缺省(内置 playwright-mcp headed 命令) | 整条后端命令覆盖;env `CC_REMOTE_MCP_BACKEND` 优先级更高 |
| `profile_dir` | 缺省(client 数据目录内) | 持久 profile 路径,拼进默认命令的 `--user-data-dir` |

**agent 侧(agent.toml,新段;既有 `[remote_mcp]` 段不动,继续管 proxy 的 enabled/port/tools_manifest)**:

```toml
[browser]
web_enabled = true   # 默认 true;false ⇒ 注入配置不含 web 条目(回到计划①单 server 形态)
# web_backend 缺省 = 内置默认(npx -y @playwright/mcp@0.0.76 --headless)
# web_backend = "npx -y @playwright/mcp@0.0.76 --headless --browser=chromium"
```

| 字段 | 默认 | 语义 |
|------|------|------|
| `web_enabled` | `true` | 是否注入 `web` stdio server 条目 |
| `web_backend` | 缺省(内置 playwright-mcp headless 命令) | 整条 web 后端命令覆盖 |

### 6. node / playwright-mcp 分发

- **要求**:agent 与 client 两台机器都需 node ≥ 18(用户已接受)。安装文档与 `install.sh` 提示补充该要求;运行时缺 node 的表现见「错误处理与降级」。
- **惰性拉取**:默认命令用 `npx -y @playwright/mcp@0.0.76`——首次执行会现场拉包(可能数十秒),其后命中 npx 缓存。pin 死版本保证两台机器、多次拉取行为一致。
- **首次慢的处理**:spec 承认这一点并分层处理——① `cc-browser` 侧:首跑慢由计划①分层超时兜底(`tools/call` 中档 ~120s 通常足够;不足则错误文案引导重试,第二次命中缓存即快);② `web` 侧:claude 自身 ~30s MCP 连接超时可能在冷缓存时打断首次握手——**预热**(agent 启动或会话 open 时后台 `npx -y @playwright/mcp@0.0.76 --version` 暖缓存)列为实现计划的推荐项;③ vendoring(随包内置,彻底消除首拉)留作开放项。
- **chromium 二进制**:playwright-mcp 需要可用的浏览器内核(playwright 下载的 chromium 或本机已装 Chrome)。首跑缺内核时的体验(自动下载 vs 文档化预装命令)见开放问题。

## 数据流

### 一次 `web` 调用(默认路径,全程在 agent)

1. claude 发 `tools/call`(如 `browser_navigate`)给 `web` server——claude 已直接 spawn 了 `npx -y @playwright/mcp@0.0.76 --headless` 子进程,stdio 直连。
2. playwright-mcp 在 agent 本机驱动 headless chromium 执行,结果原路 stdio 返回。
3. 全程零隧道、零 proxy、零 hub、零 client——不产生任何 `RemoteMcp` 帧。client 离线、甚至 webterm-only 接入,都不影响这条路径。

### 一次 `cc-browser` 调用(用户明示路径,走计划①管道)

与计划① spec「数据流」一节逐字一致(claude → proxy 登记 id/选超时档 → 隧道帧 → hub 哑转发 → client `McpHost` → 后端 stdio → 原路返回),唯一区别是第 5 步的后端从 echo 桩换成 `playwright-mcp --user-data-dir=<持久路径>`:它驱动**用户屏幕上的可见窗口**真实地导航过去。透传不变量(零解析零改写、`id` 配对、无 `id` 通知单向透传)全部沿①。

## 选择机制与引导 prompt

选择机制刻意"退化":claude 不按任务性质猜,只看**用户有没有明说**——默认恒 `web`,明示才 `cc-browser`,撞墙不自切。判断面小到几乎不会错,且把"何时在我屏幕上弹窗"的控制权完整留给用户。

替换 `GUIDANCE_PROMPT` 的完整文案(英文,注入 claude;经 `--append-system-prompt`):

```text
Two browser MCP servers are available:

- `web`: a HEADLESS browser running here on this host. Fast, invisible
  to the user, no setup needed.
- `cc-browser`: a VISIBLE browser window on the USER'S LOCAL machine,
  connected through the cloudcode CLI. The user can see it, log into
  sites in it, and operate it by hand. Its logins persist across
  sessions.

Rules:

1. For any web browsing — research, reading public pages, fetching
   data — ALWAYS use `web`. This is the default; do not ask.
2. Use `cc-browser` ONLY when the user explicitly asks for their local
   browser / cc-browser, or explicitly wants to log in or operate the
   page themselves.
3. If `web` hits a login wall, captcha, or anti-bot check, do NOT
   switch to `cc-browser` on your own. Stop and tell the user that the
   page needs them to log in or act in their local browser, and ask
   whether you should open it with `cc-browser`. Proceed only after
   they confirm.
4. Pick one server per task and stick with it. Browser state (cookies,
   logins, open pages) is NOT shared between `web` and `cc-browser` —
   they are separate browsers on separate machines, and state cannot
   be migrated mid-task.
5. When the user needs to do something by hand in `cc-browser` (e.g.
   log in or solve a captcha), pause and ask them to tell you when
   they are done, then continue from where you left off.
6. If a `cc-browser` tool call returns a 'not connected' style error,
   relay its instructions to the user (they need to open the cloudcode
   CLI on their local machine), then retry after they confirm.
```

规则 3+5 即「接管 V1 不做」的对话化替代;规则 4 即两条硬约束(状态不互通、任务级)的注入面表达。

## 持久登录与用户接管

- **持久登录**:`cc-browser` 默认命令带 `--user-data-dir=<client 端持久路径>`,playwright-mcp 自管该 headed Chrome 与持久 profile——**不需要单独的 ChromeManager、不需要我们起 Chrome 再 attach**(比 desktop-app 的 attach 方案简单一个数量级)。用户登一次,登录态在本地 profile 持久;窗口可见,用户随时能亲手干预(拖滑块、补登录)。后端崩溃/重启、client 重启都不丢登录态(profile 在盘上)。
- **用户接管(V1 不做工具)**:撞登录墙的完整回路是纯对话:claude 停下转告(引导 prompt 规则 3)→ 用户确认用 `cc-browser` → claude 打开页面 → 用户在可见窗口亲手登录/过验证码 → 用户在对话里说"好了" → claude 继续。计划①为接管预留的 `-32003` 错误码与 `LONG_CALL_TOOLS`(~600s 长档)在本计划**继续保留不启用**,留给将来真要做接管工具时用。
- **两条硬约束**(注入面、文档、测试三处都要体现):
  1. `web`(agent 无头)与 `cc-browser`(client 有头)是两台机器上两个独立浏览器、两份独立登录态,**不互通、不迁移**——httpOnly cookie 无法跨机导出,playwright-mcp 黑盒硬限制。
  2. 选后端是**任务级**:一个任务从头到尾用一个,不能中途带状态切换。本地登录态只在本地持久。

## 错误处理与降级

| 场景 | 处置 | claude / 用户可见结果 |
|------|------|----------------------|
| 调 `cc-browser` 时无 client(含 webterm-only) | 计划① `-32004` 合成错误,原样复用 | "本地浏览器未连接,请打开 cloudcode CLI"(引导 prompt 规则 6 转达) |
| `cc-browser` 后端(playwright-mcp)崩溃 | `McpHost` 既有退避重启;持久 profile 保登录态 | 一次失败,重试通常成功 |
| client 端缺 node / npx 失败 | spawn 失败走 `McpHost` 既有退避/冷却,最终在飞请求收 JSON-RPC 错误 | 错误文案引导检查 node 安装;`web` 不受影响 |
| `web` 后端 spawn 失败(缺 node / 沙箱拦 / 拉包超时) | 该 server 由 claude 直管:claude 按自身 MCP 失败语义报告 server 不可用,`--strict-mcp-config` 下其余工作不受影响 | claude 告知用户 web 浏览不可用;**不得**自行改用 `cc-browser`(规则 3 同理,需用户确认) |
| **agent 沙箱拦 headless chromium**(首要风险) | 本计划**首要复验**:在 `SandboxExec --home` 内实跑 `npx -y @playwright/mcp@0.0.76 --headless` + 一次真实导航(重点:沙箱网络访问、chromium 临时文件/共享内存写)。desktop-app 线 P1 实测过 agent 上能跑 Chrome(`feature/desktop-app`),大概率 OK。**若拦**:降级方案按序 ① 放宽沙箱 profile(为 chromium 所需路径/能力开白名单);② 不可放宽则 `web_enabled` 默认翻 false + 文档化要求(用户在非沙箱 agent 上启用),`cc-browser` 单独照常 | 验证结论与所选降级写回本 spec(见开放问题) |
| 首次 `npx` 拉包慢 | 见组件 6 三层处理(超时兜底 / 预热 / vendoring 开放项) | 最坏情况:首次调用一次超时错误,重试即成 |
| 任意请求超时 / client 中途断开 | 计划①分层超时与 `fail_pending` 原样生效(仅 `cc-browser` 路径涉及) | 不挂死,可执行文案 |

## 隔离与安全

- **沿用计划①全部隔离面**:进程级 `--mcp-config`(0600)+ `--strict-mcp-config`、绝不写全局 `~/.claude.json`、绝不 `claude mcp add`、会话级 token 路由、`SandboxExec --home`。
- **`web` 后端在 agent 沙箱内**:headless chromium 作为 claude 的子进程跑在同一每会话沙箱 HOME 里,落盘(缓存/临时 profile)圈在会话内;它具有 agent 的对外网络访问——与 claude 在 agent 上本就能 `curl` 的能力同级,不引入新的权限级别。
- **`cc-browser` 本地 profile 含登录态**:profile 在**用户自己的机器**上、用户自己的账号目录内,与"用户本机 Chrome 里本来就存着登录态"同级;不上传、不过隧道(隧道里只有 MCP 帧)。profile 目录权限收紧(如 0700)列入实现计划。可见窗口本身即监督,沿①"不设同意门"决策。

## 复用与不复用

| 类别 | 内容 |
|------|------|
| **复用(零改动)** | 计划①整条 `cc-browser` 管道:proxy / hub 哑中继 / 隧道帧 / `McpHost`(spawn/泵/握手重放/退避)/ 分层超时 / `-32004` 降级 / 0600 token / 注入铁律 D11 |
| **复用(小改)** | `mcp_config_json` 扩成双 server(组件 1);`GUIDANCE_PROMPT` 换文案(组件 4);`backend_command()` 换默认(组件 3);配置加 `[browser]` 段(组件 5) |
| **撤销** | `ee92506` 的生产路径:`embedded_echo_backend()` / `EMBEDDED_ECHO_MCP`(echo 桩文件本体留作测试夹具);env `CC_REMOTE_MCP_BACKEND` 覆盖保留 |
| **不带** | CDP 投屏 / 像素镜像(desktop-app 线,已否决);desktop-app 的 ChromeManager-attach(被 playwright-mcp `--user-data-dir` 自管取代);自写 Rust CDP 后端(后续优化);接管/暂停工具(V1 不做,`-32003` / `LONG_CALL_TOOLS` 继续闲置预留) |

## 测试策略

沿计划①支点:管道部分用桩后端在 CI 内测,真浏览器隔离成手动冒烟。

| 层 | 测什么 | 怎么测 |
|----|--------|--------|
| 单元 | `mcp_config_json` 双 server 生成:`web` 条目(stdio,command/args 正确、含 `--headless` 与 pin 版本)+ `cc-browser` 条目(http,与①字节语义一致);`web_enabled=false` 时退回单 server | 纯函数断言 JSON |
| | `extract_token_from_config` 对双 server 配置仍取出 `cc-browser` token(自愈回采兼容) | 喂新旧两种格式 |
| | `backend_command()` 优先级:env > `[browser].backend` > 内置默认;`enabled=false` → None;默认命令含 `--user-data-dir` 与 pin 版本;**不再**回落 echo 桩 | 纯函数 + env 隔离测试 |
| | `[browser]` 段缺省解析 = 全默认(client/agent 两侧) | toml 反序列化断言 |
| | 引导 prompt:含 `web`/`cc-browser` 两名、含"不自切、先问用户"措辞;`claude_mcp_args` 拼装不变 | 字符串断言 |
| 集成 | 管道回归:`CC_REMOTE_MCP_BACKEND` 指向 `test-fixtures/echo-mcp.mjs` 跑①的端到端 loopback——证明换默认后端没动管道 | ①既有测试原样过 |
| 手动冒烟 | 沙箱复验(首要):`SandboxExec` 内 `web` 真实导航成功 | agent 真机 |
| | `web` 默认路径:让 claude 查一个公开页面 → 全程无本地窗口、client 离线也成 | 真机 |
| | `cc-browser` 明示路径:用户说"用本地浏览器打开 X" → 本地弹可见窗口;登录一个站点 → 重启 client/后端 → 登录态仍在 | 真机 |
| | 撞墙对话协调:`web` 撞登录墙 → claude 停下询问 → 确认后 `cc-browser` 打开 → 用户亲手登录 → 说"好了" → claude 续跑 | 真机 |
| | 首次 npx 拉包:冷缓存机器上首调的耗时/超时表现与重试恢复 | 真机 |

## 开放问题(实现计划阶段确定)

1. **agent 沙箱复验结论**:`SandboxExec` 内 playwright-mcp + headless chromium 是否直接可跑;不可跑时落哪一档降级(放宽 profile vs `web_enabled` 默认 false + 文档化)。这是实现计划的第一个任务。
2. **playwright-mcp 分发:npx 惰性拉取(当前默认)vs vendored 进安装包**。零手动配置已达成,首跑延迟是唯一痛点;预热(组件 6)是否足够、vendoring 是否值得包体积代价,在此拍定。
3. **`web` 后端 chromium 首次下载**:playwright-mcp 冷机首跑缺浏览器内核时的行为(自动下载的耗时与可行性 / 是否在安装文档与 `install.sh` 中加 `npx playwright install chromium` 预装步骤 / 复用本机已装 Chrome)。
4. **pin 版本升级策略**:`@playwright/mcp@0.0.76` 升级的节奏与验证清单(两后端共用一个 pin;升级需重跑双后端冒烟),以及 pin 常量在代码中的单点存放。
5. **实现级落点**:`[browser]` 字段最终命名与默认 `profile_dir` 确切路径、`mcp_config_json` 新签名、agent 侧 `[browser]` 与既有 `[remote_mcp]` 的代码组织、预热的挂接点。

## 发布

按项目惯例:`feature/cc-browser-preset` 上实现并经用户验证 → 合 `main` → bump MINOR → 打 tag 推送触发 CI。本计划**无协议/帧改动**(`web` 不走隧道,`cc-browser` 帧面不变),不涉及三端 lockstep 发布顺序;agent 与 client 二进制独立可升,旧 client + 新 agent = `web` 可用、`cc-browser` 走 `-32004` 降级,新 client + 旧 agent = 行为同计划①。
