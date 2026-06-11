# cc-browser:远程 MCP 透明管道 — 设计

> 让跑在 agent 上的远程 claude 操作**用户本地机器上的浏览器**,体验对齐"本地 claude code 打开浏览器":零配置、可见窗口、全程无 CDP。核心是一条**通用、backend 无关的「远程 MCP」传输管道**;"cc-browser" 不是一层代码,而是这条管道上的一条**浏览器预设**(固定 server 名 + 默认后端 + 托管 Chrome)。

- **日期**:2026-06-12
- **分支**:基线 `dev`(当前对浏览器代码干净);实现自 `dev` 拉新分支
- **状态**:设计已与用户敲定,待写实现计划

## 背景与动机

cloudcode 是一个 Rust workspace,三个角色:

| 角色 | 跑在哪 | 职责 |
|------|--------|------|
| hub | 公网主机 | 中继 + webterm SPA + admin |
| agent | 云端机器 | 真正跑 `claude`(Claude Code CLI)的地方,经 tmux+claude;与 hub 之间有一条已认证的 WebSocket 隧道 |
| client | 用户本地机器 | 瘦 CLI(`cloudcode`),串流 PTY 字节;另有 webterm(浏览器里的 SPA)作为另一种接入端 |

claude 在 agent(云端)上跑,但"打开浏览器"这件事应该发生在**用户眼前的本地机器**上——带用户的登录态、看得见、可干预。目标体验:和本地 claude code 打开浏览器**一模一样**,尽可能零配置、最大化自动化。

### 先前两次实现的教训

**M1-M3(`feature/local-browser`)— 思路验证可行;本设计是它的泛化与精炼。**
旧 spec 见 `feature/local-browser:docs/superpowers/specs/2026-06-10-cloud-claude-local-browser-design.md`。它证明了 MCP-over-hub-relay 成立:client 端跑 `@playwright/mcp`,中间层对 MCP JSON-RPC 帧全透明转发(靠 JSON-RPC `id` 自配对,中间不解析、不改写)。但那一版把授权门状态机、`tools/list` 能力过滤、handoff 控制都长在了"浏览器"这个具体场景里,管道与场景耦合。本设计把**管道剥离成通用传输**(backend 无关),把"浏览器"降级为一条可替换的预设。

**desktop-app(`feature/desktop-app`)— CDP 投屏路线,已废弃。**
该路线用 CDP screencast 把无头浏览器像素投屏到 egui viewer 镜像,再以 CDP 合成输入注入(参见其 `crates/agent/src/browser/screencast.rs`)。在"百度滑块/验证码"等场景,CDP 合成输入与页面脚本争抢、导致界面死锁。用户**明确否决 CDP**。本设计**完全不含** CDP 通道、不含像素镜像/viewer、不含桌面 egui app,整条线不迁移;取而代之的是真窗口 + 真人输入(见「登录 / 人工交接」),天然没有合成输入争抢。

## 目标 / 非目标

### 目标

| # | 目标 |
|---|------|
| 1 | **透明化**:agent 上的 claude 操作用户本地浏览器,体验等同本地 claude code 打开浏览器 |
| 2 | **零配置默认**:装好 CLI 即用;默认后端随包内置(dev-browser),无需任何手动 MCP 配置 |
| 3 | **backend 无关的通用管道**:中间层只搬 MCP 帧;换后端只改一行配置 |
| 4 | **登录态持久**:托管 Chrome 持久 profile,登录一次长期有效,换后端不丢 |
| 5 | **隔离**:对同机其他 claude 进程零影响;多会话互不串台 |
| 6 | **永不卡死**:client 不在 / 后端崩溃 / 用户不响应,都收敛为有文案的 JSON-RPC 错误或有界超时 |

### 非目标(Non-Goals)

- **无 CDP**:cloudcode 自身不实现任何 CDP 通道——不做 CDP screencast 投屏、不做 CDP 合成输入注入。(后端黑盒内部用什么协议驱动 Chrome 是后端自己的事,不在本设计接口面内。)
- **无像素镜像 / viewer**:不做画面镜像,不做任何 viewer;监督靠本地真实可见窗口本身。
- **无桌面 app**:不引入 egui / 任何桌面 app crate。
- **无同意门**:不做"允许 cloudcode 操作本地浏览器"的同意弹窗(明确决策,见「隔离与安全」)。
- **不是通用 MCP 市场**:本设计只是"一条传输 + 一条浏览器预设",不做后端发现、安装、市场化管理。

## 总体架构

```
┌─ agent 主机(云端)──────────────────────────────────────────────
│
│   claude(tmux 内;每会话临时 --mcp-config + --strict-mcp-config)
│     │ ▲   MCP JSON-RPC(claude 眼里 server 名固定 = "cc-browser")
│     ▼ │
│   远程-MCP proxy 垫片 —— 通用、会话级、不懂浏览器语义,只搬帧
│     │ ▲
└─────┼─┼────────────────────────────────────────────────────────
      │ │   既有 agent⇄hub WebSocket 隧道(新增「远程 MCP」帧类型)
┌─────┼─┼────────────────────────────────────────────────────────
│  hub(公网)—— 哑中继:按会话路由、负载原样转发、不解析内容
└─────┼─┼────────────────────────────────────────────────────────
      │ │   既有 client⇄hub WebSocket 连接
┌─────┼─┼─ client 主机(用户本地)─────────────────────────────────
│     ▼ │
│   cloudcode CLI —— 通用 MCP 宿主
│   (拉起后端子进程 / stdio⇄隧道桥接 / 崩溃退避重启)
│     │ ▲   MCP over stdio
│     ▼ │
│   后端 MCP server(插槽;默认内置 dev-browser,
│   一行配置可换 @playwright/mcp、chrome-devtools-mcp 等任意 MCP server)
│     │
│     │   attach(怎么连由后端自理;Chrome 拥有权在宿主,与后端无关)
│     ▼
│   托管 Chrome(专用进程、持久 profile、headed 可见窗口,
│   独立于用户日常浏览器)
└────────────────────────────────────────────────────────────────
```

三条管道性质,贯穿全文:

- **透传 = backend 无关**。claude 看到的工具表 = 当前后端暴露的工具,运行时动态发现。proxy 不认识 `browser_navigate`/`browser_click`,只搬运。管道唯一"理解"的层级是 JSON-RPC / MCP 的**通用骨架**:`id` 配对、按 `method` 选超时档、降级时合成应答——这是 MCP 通用语义,不是浏览器语义。
- **"cc-browser" 是预设不是代码层**。claude 眼里 MCP server 名固定叫 `cc-browser`(稳定身份,由我们设置);底下真实后端默认是随包的 dev-browser,工具名沿用后端自己的 `browser_*`。
- **hub 保持哑中继**,只多认一种"远程 MCP"帧类型用于转发,不解析负载。

### webterm 的边界(硬约束)

浏览器要跑在**用户本地机器**上,前提是本地能拉起 Chrome 子进程。**webterm(浏览器标签页里的 SPA)做不到**——它无法 spawn 本地 Chrome。因此浏览器功能只对**原生 CLI client** 可用;仅以 webterm 接入时,浏览器调用落到"无可用 client"的降级错误(见「错误处理与降级」),claude 会把"请打开 cloudcode CLI"转达给用户。

## 组件拆分

### 1. agent 远程-MCP proxy 垫片

- **做什么**:一个**通用、不理解浏览器语义**的 MCP proxy(localhost)。把 claude 发来的 MCP JSON-RPC 消息**原样转发**——`initialize` / `tools/list` / `tools/call` / notifications 全透传——塞进 agent 现有的 hub 隧道;反向把 client 侧回来的帧按 JSON-RPC `id` 配对交还 claude。**会话级**:同一 agent 跑多个用户会话时,各自 claude 各指各的 proxy 端点 → 各自的 client,不串台。
- **怎么用**:claude 经 `--mcp-config` 连接它(server 名 `cc-browser`),进程随会话生命周期。
- **依赖**:agent 现有 agent⇄hub WebSocket 隧道;`id` 配对 + method 感知的分层超时(机制移植自 `feature/local-browser:crates/agent/src/mcp_endpoint.rs`)。
- **传输形态(claude⇄proxy 这一跳)**:**倾向 stdio**——claude 直接 spawn proxy,stdio 帧边界清晰,且天然绕开 M1-M3 踩过的"HTTP 非 2xx 被 claude 当成需要 OAuth"的坑。M1-M3 的 `mcp_endpoint.rs` 用 HTTP 且已填了那个坑(JSON-RPC error at HTTP 200),所以"复用 HTTP vs 改 stdio"列为**实现计划阶段再定的开放项**。

### 2. agent pty spawn 注入

- **做什么**:agent 拉起 claude 时拼 `--mcp-config <每会话临时配置>` + `--strict-mcp-config`,并附一段引导 system prompt(注入方式例如 `--append-system-prompt`),告知 claude:`cc-browser` 的工具在**用户本地的可见浏览器**上执行;撞到登录墙/验证码/滑块时调用「请用户接管」工具;收到"未连接"错误时提示用户打开 cloudcode CLI。
- **怎么用**:**进程级、即用即弃**——临时配置只对这一个 claude 进程生效,**绝不写全局 `~/.claude.json`**。**铁律:永远不用 `claude mcp add`**(那会写全局)。
- **依赖**:agent 现有 pty/tmux spawn 路径;`--mcp-config` 注入机制 M1-M3 已有先例(本设计在其上叠加 `--strict-mcp-config` 与引导 prompt)。

### 3. hub 远程-MCP 帧中继

- **做什么**:在既有 client↔agent 路由上新认一种"远程 MCP"帧类型,按会话绑定原样转发,双向对偶;不解析、不改写负载。
- **怎么用**:对 hub 而言与 PTY 字节同构——只认帧、不懂内容,符合其"中继不理解负载"的既有哲学。
- **依赖**:既有会话路由/绑定;负载零反序列化透传(M1-M3 的 `Box<RawValue>` 手法可沿用)。

### 4. client 通用 MCP 宿主

- **做什么**:通用 MCP 启动器 + **一个插槽**。负责拉起真实后端 MCP server 子进程(默认 dev-browser),把后端的 stdio 与 hub 隧道桥接;后端崩溃时带退避重启;缓存 MCP 握手帧并在后端(重)启动时重放,使在跑的 claude 会话无感续接。
- **怎么用**:零配置默认即用;换后端只改一行 `backend = [命令...]`。
- **依赖**:client 既有 hub 连接;MCP 子进程管理(spawn / stdio 泵 / 握手缓存重放 / 退避重启)移植自 `feature/local-browser:crates/client/src/cc_browser.rs`。

### 5. client 托管 Chrome 设施

- **做什么**:CLI 宿主**自己拥有**一个**持久 profile** 的专用 Chrome,独立于用户日常浏览器进程。登录一次,持久保持。后端 **attach** 到这个托管 Chrome——Chrome 的拥有权与后端无关,因此**换后端也不丢登录态**。
- **怎么用**:无感。**headed 默认开**(可见窗口 = 透明 + 监督);headless 作为配置可选项。首次使用自动创建 profile 目录。
- **依赖**:本机 Chrome/Chromium;宿主把托管 Chrome 的连接信息交给后端的方式(如启动参数或环境变量)在实现计划阶段确定。

### 6. 配置 `[browser]` 段(client 侧)

默认零配置——不写任何配置即得到完整默认行为。字段:

```toml
[browser]
enabled = true   # 默认 true;false 时宿主不拉后端、不参与桥接
headed  = true   # 默认 true(可见窗口);false = headless
# backend 缺省 = 随包内置的 dev-browser;换后端只改这一行,示例:
# backend = ["npx", "-y", "@playwright/mcp"]
```

| 字段 | 默认 | 语义 |
|------|------|------|
| `enabled` | `true` | 本机是否提供浏览器能力 |
| `backend` | 缺省(内置 dev-browser) | 一行换任意 MCP server,如 `@playwright/mcp`、`chrome-devtools-mcp` |
| `headed` | `true` | 托管 Chrome 是否带可见窗口 |

(agent 侧是否需要独立的功能开关,属实现级落点,实现计划阶段确定。)

### 7. 默认后端的声明式工具 manifest

- **做什么**:dev-browser 工具表(工具名 / 入参 schema / 描述)的一份**声明式数据副本**,让管道在 client/后端尚不可达时也能应答 `tools/list`——支撑"始终广告工具"(见降级 ①)。manifest 是数据不是代码,不破坏 proxy 的"不懂浏览器语义"。
- **限制**:自定义后端没有 manifest → 冷启动列不出工具 → client 上线后靠 `notifications/tools/list_changed` 补齐(见降级 ③)。
- **依赖 / 待定**:manifest 的存放与加载点(随 agent 内置 vs client 上线时上报缓存),以及冷启动时 `initialize` 握手由管道哪一端权威应答,均在实现计划阶段确定。

## 数据流:一次 `browser_navigate` 调用的完整轨迹

前提:claude 已 spawn(`--mcp-config` 指向 proxy),client 在线,后端已拉起。

1. **claude(agent)** 发 `tools/call`(`name: "browser_navigate"`, 带 `url`)给 proxy 垫片——在 claude 看来就是调本地 MCP server `cc-browser` 的一个工具。
2. **proxy** 不解析语义:取 JSON-RPC `id` 登记在飞请求,按 `method`(`tools/call`)选中档超时(~120s),把**整帧原文**作为远程-MCP 帧写入 agent⇄hub 隧道。
3. **hub** 按会话绑定查到对应 client 连接,**原样转发**。
4. **client 宿主** 收帧,原文写入后端子进程 stdin(若后端此刻未在跑:先拉起 + 重放缓存的握手帧,再投递)。
5. **dev-browser 后端** 执行 navigate,驱动**托管 Chrome**——用户屏幕上的可见窗口真实地导航过去。
6. 后端把 JSON-RPC result 写 stdout → 宿主回传 hub → hub 回转 agent → **proxy 按 `id` 配对**,把响应交还 claude。claude 拿到结果,继续推理。

不变量:

- 全程除 proxy 读 `id`/`method`(配对与选超时档)外,**零解析、零改写**,原文字节透传。
- 请求/响应靠 MCP 自身的 JSON-RPC `id` 配对;通知帧(无 `id`)单向透传、不登记、不等待。
- 每一跳都在既有认证连接内,无新监听面(proxy 仅 localhost / stdio)。

## 错误处理与降级

阻抗不匹配是常态:claude 的 MCP 配置在 spawn 那一刻固定,而 client(连同后端、浏览器)动态来去。处理为以下四条:

1. **始终广告工具**。只要功能开着,claude 一启动读 `tools/list` 就能看到浏览器工具——哪怕此刻没有任何 client 在线。默认后端 dev-browser 带一份声明式 manifest(组件 7),冷启动也能列出。
2. **调用时没有可用 client/后端 → 返回 JSON-RPC 错误**(不是卡死、不是传输失败),文案可执行:"本地浏览器未连接,请让用户打开 cloudcode CLI"。claude 把这句话转达给用户;webterm-only 接入恒走此路径。
3. **client 连上/断开 → 发 `notifications/tools/list_changed`**,促使 claude 重新拉 `tools/list` 同步真实工具集——补齐自定义后端冷启动列不出的工具、反映就绪状态。依赖 claude 作为 MCP client 认这条通知——**落地时验证**(见开放问题)。
4. **永不无限阻塞**。分层超时沿用 M1-M3 的三档:「请用户接管」长档 ~600s / `tools/call` 中档 ~120s / 其余(握手、元数据)短档 ~25s(短档低于 claude 自身 ~30s 的 MCP 连接超时,保证我们的错误先于其客户端超时到达)。超时报"未就绪 + 怎么办"的可执行文案。

具体场景:

| 场景 | 处置 | claude 可见结果 |
|------|------|----------------|
| 调用时无 client(含 webterm-only) | 降级 ②,proxy/管道合成 JSON-RPC 错误 | "本地浏览器未连接,请让用户打开 cloudcode CLI" |
| 调用中途 client 断开 | 该会话所有在飞请求立刻以 JSON-RPC 错误收尾(M1-M3 `fail_pending` 思路);随后发 `list_changed` | 在飞调用 → 错误;非浏览器工作不受影响 |
| 后端子进程崩溃 | 宿主带退避重启(次数有上限);托管 Chrome 持久 profile 使状态可恢复;崩溃瞬间在飞调用 → 错误 | 一次失败,重试通常成功 |
| client 重连 | 宿主重新拉后端 + 重放握手;发 `list_changed` | 工具表恢复,无需 claude 重启 |
| 用户长时间不响应「请用户接管」 | 长档超时(~600s)到点,返回"用户未在时限内接管"错误 | claude 得知并可改策略/转告用户 |
| 任意请求超时 | 对应档位到点 → JSON-RPC 错误,文案含状态与建议(如"浏览器可能仍在启动,重试通常成功") | 不挂死,可重试 |
| (若传输沿用 HTTP)任何请求级失败 | **必须**以 HTTP 200 + JSON-RPC error 对象返回,绝不裸回非 2xx——claude 把 MCP POST 的任何非 2xx 当成"需要认证",触发 OAuth 探测瀑布并报误导性的 `SDK auth failed: HTTP 404`(M1-M3 实测教训,`mcp_endpoint.rs` 已修) | 真实错误文案直达 |

错误码约定沿用 M1-M3(如超时 / 未注册 / 通道拆除各占一码),最终分配在实现计划阶段确定。

## 隔离与安全

### 隔离:防止污染同机其他 claude

| 层 | 机制 | 效果 |
|----|------|------|
| 进程级配置 | agent 拉起 claude 时拼 `--mcp-config <每会话临时配置>` + `--strict-mcp-config`;即用即弃,**绝不写全局 `~/.claude.json`**。**铁律:永远不用 `claude mcp add`**(会写全局) | 同机器上别的 claude(没带这两个 flag)读自己本来的本地 MCP,**零影响** |
| 沙箱 HOME | 叠加保险:agent 本就在 `SandboxExec --home <每会话>` 里跑 claude | 即便有落盘文件也圈在每会话 HOME 内 |
| 会话级端点 | proxy 端点是**会话级**的:同一 agent 跑多个用户会话时,各自 claude 各指各的 proxy → 各自的 client | 多会话不串台 |

### 安全:可见窗口即监督,不设同意门

**明确决策:不做"允许 cloudcode 操作本地浏览器"的同意弹窗。** 监督由**可见窗口本身**承担——浏览器就开在用户屏幕上,每一步操作看得见、随时可干预——贴近本地 claude code"打开浏览器无需点头"的透明感。M1-M3 的授权门状态机随之不再保留(见「复用与不复用」)。

### 登录 / 人工交接

- **登录 = 用户直接在可见窗口里操作**(窗口就在本机、看得见),不需要 M1-M3 那种抽象的 handoff 同意流。托管 Chrome 持久 profile 使一次登录长期有效。
- 保留一个**轻量「请用户接管」工具**(命名沿用 M1-M3 的 `request_handoff` 与否在实现计划阶段确定):
  1. claude 撞到登录墙 / 验证码 / 滑块 → 调用该工具;
  2. 用户收到一句提示:"请在浏览器里完成 X";
  3. 长档超时(~600s)内等用户在**真实窗口里亲手操作**;
  4. 用户示意完成 → 工具返回 → claude 继续。
- **这正好天然解决了最初的滑块死锁问题**:真窗口、真人输入,没有 CDP 合成输入与页面脚本的争抢。

## 可插拔后端(方案 A:透明管道)

CLI 宿主 = 通用 MCP 启动器 + 一个插槽。默认后端 dev-browser(随包内置、零安装);换后端只改一行 `backend = [命令...]`(可换 `@playwright/mcp`、`chrome-devtools-mcp`,或任意 MCP server)。

- **唯一约束:后端必须会说 MCP。** 非 MCP 工具需要单独包一层 MCP shim 才能入插槽——这是例外,不是常态。
- **不写任何 per-backend 适配器代码。** claude 看到什么工具,完全由后端运行时决定。

| 方案 | 思路 | 结论 | 理由 |
|------|------|------|------|
| **A:透明管道** | 工具表 = 后端的工具表,原样透传 | **采用** | 零适配器代码;后端升级/更换零成本;与 M1-M3 已验证的全透传机制同构 |
| B:固定工具面 + 适配器 | 我们定义一套稳定的 `browser_*` 工具,逐后端写适配器映射 | 否决 | 每加一个后端要写一份适配器;且被迫取各后端能力的最小公集,丢长尾能力 |
| C:混合(常用工具固定 + 其余透传) | A、B 折中 | 否决 | 只有频繁换后端才值得的多余复杂度;当前没有这种频率 |

## 复用与不复用

基线:从 `dev`(当前对浏览器代码干净)出发新建分支构建。

| 类别 | 内容 | 来源 |
|------|------|------|
| **复用(移植)** | hub 隧道反向通道(承载 MCP 帧的对偶帧类型与路由) | `feature/local-browser` |
| | agent pty spawn 的 `--mcp-config` 注入机制(本设计叠加 `--strict-mcp-config` + 引导 prompt) | `feature/local-browser` |
| | JSON-RPC-error-at-200 处理(claude 把非 2xx 当 OAuth 的坑) | `feature/local-browser:crates/agent/src/mcp_endpoint.rs` |
| | 分层超时机制(~600s / ~120s / ~25s 三档,method 感知) | 同上 |
| | MCP 子进程管理(spawn / stdio 泵 / 握手缓存重放 / 退避重启) | `feature/local-browser:crates/client/src/cc_browser.rs` |
| **不带(整条线砍掉)** | CDP screencast(`crates/agent/src/browser/screencast.rs`)、egui viewer / app crate、任何 CDP 合成输入 | `feature/desktop-app` |
| **不随迁(被新决策取代)** | M1-M3 的授权门状态机(被"不设同意门"取代);`tools/list` 能力过滤(被"始终广告 + 调用时错误 + `list_changed`"的降级模型取代) | `feature/local-browser` |

## 测试策略

延续 M1-M3 的支点:管道不关心对端是不是真浏览器(都是不透明 MCP 帧),用桩 MCP 后端即可在 CI 内测全管道,把真浏览器隔离成手动冒烟(`feature/local-browser` 的 `test-fixtures/echo-mcp.mjs` 是现成先例)。

| 层 | 测什么 | 怎么测 |
|----|--------|--------|
| 单元 | 远程-MCP 帧编解码(serde 往返)、`id`/`method` 提取与超时档选择 | 纯函数,表驱动 |
| | manifest 服务:无 client 时 `tools/list` 由 manifest 应答 | 喂请求断言合成响应 |
| | strict 注入:spawn 参数拼装含 `--mcp-config` + `--strict-mcp-config`、临时配置内容正确、绝不触碰全局路径 | 参数拼装做成纯函数断言 |
| 集成 | **端到端 `tools/call`**:loopback hub → 假 client → 桩 MCP 后端,全链路原样透传、`id` 配对回到调用方 | 进程内 loopback,不依赖真浏览器 |
| | client attach → 收到 `notifications/tools/list_changed`;detach 同理 | 模拟 client 上下线 |
| | 无 client 时调用 → JSON-RPC 错误(文案含"打开 cloudcode CLI"),且非传输失败 | 断言错误对象与文案 |
| 手动冒烟 | 真实 dev-browser + 真实托管 Chrome:导航/截图等基本操作;headed 窗口可见 | 本地跑,不进 CI |
| | 登录持久化:登录一次 → 重启 client/后端/Chrome → 登录态仍在;换后端登录态仍在 | 本地跑 |
| | 滑块/验证码走「请用户接管」:claude 调用 → 提示 → 人工完成 → 续跑 | 本地跑 |

## 开放问题(实现计划阶段确定)

1. **claude⇄proxy 传输:stdio(倾向)vs 复用 M1-M3 的 HTTP。** stdio 帧边界清晰、天然绕开 OAuth 误判坑;HTTP 一侧已有填好坑的现成实现(`mcp_endpoint.rs`)。
2. **dev-browser 的分发方式:npx 拉取 vs vendored 进安装包。**"零安装体验"是定死的,达成机制开放。
3. **manifest 来源**:随 agent 内置 vs client 上线时上报缓存;连带冷启动时 `initialize` 握手由哪一端权威应答、握手缓存归属端。
4. **`notifications/tools/list_changed` 兼容性验证**:claude 作为 MCP client 是否消费该通知并重拉 `tools/list`——落地时第一时间验证,不成立则降级策略需在实现计划中给出替代(如文案引导重试)。
5. **其余实现级落点**:proxy 端点的具体形态与生命周期挂接、「请用户接管」工具的提供方(宿主注入 vs dev-browser 自带)与命名、托管 Chrome 连接信息的交付方式、agent 侧功能开关、错误码最终分配。

## 发布

按项目惯例:`dev` 上实现并经用户验证 → 合 `main` → bump MINOR → 打 tag 推送触发 CI。新增帧类型横跨 hub/agent/client 三端,需随同版本发布;跨版本宽容策略(旧端遇未知帧的行为)在实现计划阶段确定。
