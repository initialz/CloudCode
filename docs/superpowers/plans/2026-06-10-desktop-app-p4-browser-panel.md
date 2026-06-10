# Desktop App P4 — 浏览器面板 + 分屏 实现计划

> **For agentic workers:** REQUIRED SUB-SKILL: superpowers:subagent-driven-development.

**Goal:** app 的 Session 屏变成分屏:左终端(claude)+ 右浏览器映射(CDP 投屏面板),可调分隔、可全屏切换、focus 路由。浏览器面板渲染 agent 投来的 JPEG 帧,捕获鼠标/键盘/IME(中文)回传注入。并修 P2 安全项:per-session 页映射。

**Architecture:** app backend 为浏览器面板**开第二条 ws** 到 hub 的 `/v1/viewer/ws?session=<id>`(复用 P2 全部 hub 中继不改),收二进制 JPEG 帧、发 JSON ViewerInputEvent。app 解码 JPEG→egui 纹理铺面板;egui 鼠标/键/IME 事件→ViewerInputEvent JSON 上行。agent 侧 ScreencastSession 改为按 session 选页(关闭 P2 多账户跨页泄漏)。

**验收(spec P4):** 单窗口内左 claude 干活、右实时看它操作浏览器、随时上手点击/中文输入接管;分隔可拖、可全屏切换。

**基线复用:** P2 hub viewer 中继 + agent screencast/ViewerManager 全在;P3 app eframe + 终端面板可嵌分屏。P4 = app 加 viewer 客户端 + 浏览器面板 + 分屏 + agent 选页修正。

---

## Task 1: agent per-session 页映射(P2 安全项)

**Files:** `crates/agent/src/browser/viewer.rs`、`mcp_endpoint.rs`(可能)、`screencast.rs`(pick 增强)

P2 的 `pick_page_target` 选"活动非 blank 页",多账户共享 agent 下跨页泄漏。ViewerAttach 已带 `session_id`,据此选该 session 的 playwright-mcp 所驱动的页。

- [ ] 调研 + 实现关联(用 context7 查 CDP Target.getTargets 的 browserContextId / Target.attachToTarget):每个 session 的 playwright-mcp 经 --cdp-endpoint 连 Chrome 时创建自己的 browser context。ViewerManager.attach(viewer, session_id) 选页时,优先选属于 session_id 的 playwright-mcp 所用 context 的页。
  - **关联策略(择优实现,验证可行性)**:① 该 session 的 SessionBrowser(mcp_endpoint per-session 子进程)启动时/首个 navigate 后,agent 快照 Chrome targets,记录该 session 新增的 page target id / browserContextId;ViewerAttach 时按此选页。② 退路:若关联不可靠,保留"活动页"但用 `browserContextId` 过滤到该 session 的 context(若能拿到)。③ 最终退路:活动页 + **文档化 solo-use 风险接受**(已在 P2 文档,P4 尽力收紧)。
  - mcp_endpoint 的 SessionBrowser 可暴露"该 session 的 cdp target/context 提示"给 ViewerManager。
- [ ] `pick_page_target` 增强为 `pick_page_for_session(targets_json, context_hint: Option<&str>) -> Option<String>`:有 hint 选匹配 context 的页,无则回退活动页。纯函数表驱动测试(hint 命中、hint 未命中回退、无页 None)。
- [ ] `#[ignore]` 集成:起两个"session"(两个 playwright-mcp 各 navigate 不同页)→ ViewerAttach session A → 断言投的是 A 的页(snapshot 内容含 A 的标识)。本机跑一次。若关联策略证明不可行,文档记录并退到活动页+风险接受。
- [ ] 提交 `fix(agent): per-session page targeting for screencast (closes P2 cross-page leak)`。

## Task 2: app viewer 客户端(第二条 ws)

**Files:** `crates/app/src/viewer/`(新:client、proto)

- [ ] `app/src/viewer/proto.rs`:ViewerInputEvent 的 app 侧定义(与 hub 的 JSON 形 `{"kind":...}` 对齐 —— app 不依赖 agent crate;定义匹配 serde 形即可,加往返测试对齐 hub 的 viewer_session.rs parse)。
- [ ] `app/src/viewer/client.rs`:`ViewerClient::connect(hub_url, token_or_cookie, session_id)` —— 开 ws 到 `/v1/viewer/ws?session=<id>`。**鉴权**:P2 viewer ws 用 cookie(浏览器场景);app 是原生客户端,没 cookie。**需协调**:要么 app 的 viewer ws 复用 PTY ws 的 token 鉴权(改 hub viewer_session.rs 支持 token,像 pty_session 的 CLI Hello 那样),要么 viewer 帧并入 PTY ws。**决定**:改 hub viewer_session 接受 token 鉴权(query param 或首帧 Hello),复用 pty_session 的 token 校验;app 用 config 里的 token。(这是对 P2 的小扩展,记为 Task 2 子项。)
- [ ] client 收二进制 JPEG → frame channel 给 UI;UI 的 ViewerInputEvent → JSON 文本上行。后台任务结构同 PTY backend(tokio 线程,channel 桥 egui)。
- [ ] 测试:proto JSON 往返(对齐 hub 形);连接/帧解析逻辑能抽则抽。真连接靠集成/冒烟。
- [ ] 提交 `feat(app): viewer ws client (screencast frames + input uplink)`。

## Task 3: app 浏览器面板(渲染 + 输入)

**Files:** `crates/app/src/viewer/panel.rs`

- [ ] `BrowserPanel`:持最新帧(解码后纹理)。JPEG 解码用 `zune-jpeg`(轻)或 `image` crate;解码 → egui `ColorImage` → `ctx.load_texture`(每帧更新或脏更新);egui `Image` 铺面板,保持纵横比 letterbox。
- [ ] 输入捕获(面板 focused 时):鼠标 move/down/up/wheel(坐标按 面板显示尺寸:帧 viewport 尺寸换算成 viewport 像素)、键盘(同终端的 key 映射但目标是 CDP:egui Key→ViewerInputEvent::Key{key,code,text,modifiers})、**IME**(compositionend/Commit → ViewerInputEvent::InsertText)。纯函数:坐标换算 `panel_to_viewport(px,py, panel_rect, frame_w,frame_h)`、egui event→ViewerInputEvent 映射,各测试。
- [ ] 帧无 / 未连接时面板显示占位("浏览器空闲 / 未连接")。
- [ ] 提交 `feat(app): browser panel — JPEG render + mouse/key/IME capture`。

## Task 4: 分屏布局 + focus 路由

**Files:** `crates/app/src/main.rs`(Session 屏)

- [ ] Session 屏改为 egui 分屏:左 TerminalPanel + 右 BrowserPanel,中间可拖分隔条(egui `SidePanel` 可调宽 或自绘 splitter)。比例持久(本会话)。
- [ ] 全屏切换:快捷键/按钮在 仅终端 / 仅浏览器 / 分屏 三态切换。
- [ ] focus 路由:点哪个面板哪个获焦,键盘/IME 事件只进获焦面板(egui 单焦点天然保证;确认两面板 id 不冲突)。
- [ ] 浏览器面板**懒连接**:首次显示浏览器面板(或 claude 首次浏览器活动)才开 viewer ws;隐藏/退出 detach(省 screencast 开销,呼应 P2 按需投屏)。
- [ ] 提交 `feat(app): split layout (terminal | browser) + focus routing + fullscreen toggle`。

## Task 5: 集成 + 冒烟 + 收尾

**Files:** smoke 文档

- [ ] 串起:Session 屏双面板生命周期(viewer ws 与 PTY ws 并存、各自重连);BrowserPanel 在 SessionClosed/断线时清理。
- [ ] 冒烟 `2026-06-10-desktop-app-p4-e2e-smoke.md`:`cargo run -p cloudcode-app` → 进 session → 让 claude 开网页 → **右面板实时显示该页** → 鼠标点击页面元素、中文输入框输入中文(IME)→ 页面响应 → 拖分隔条调比例 → 全屏切换三态 → 关浏览器面板停帧(agent 日志)。per-session 页:开两个 workspace 各开不同页,确认各自面板互不串(P2 安全项验证)。
- [ ] `cargo test --workspace` 全绿、`cargo build --workspace` 零警告、push。

## Self-Review 备忘
- viewer ws 鉴权从 cookie 扩到 token(P2 小扩展,Task 2)—— 复用 pty_session token 校验,勿削弱(账户归属门保留)。
- per-session 页映射(Task 1)是真安全修复;若 CDP context 关联不可行,诚实退到活动页+风险接受文档,不假装修好。
- 浏览器面板 IME 与终端面板 IME 同处境(macOS 优先);坐标换算是新纯函数,务必测。
- GUI 不可 headless 测,纯逻辑全测 + 冒烟,与 P3 一致。
- P5 衔接:打包(dmg/AppImage)、自更新、CLI 投屏 URL 退化、字体打包(可选替换 P3 的运行时加载)。
