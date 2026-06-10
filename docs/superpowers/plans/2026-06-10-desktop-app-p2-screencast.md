# Desktop App P2 — 投屏通道 实现计划

> **For agentic workers:** REQUIRED SUB-SKILL: superpowers:subagent-driven-development.

**Goal:** agent 的常驻 Chrome 经 CDP screencast 把画面流到一个浏览器验证页,验证页捕获鼠标/键盘/IME 经 hub 回传 agent 注入 CDP Input —— 实时看到 claude 操作的页面,且能人工点击/输入(含中文)。

**Architecture:** viewer 网页(hub 托管单 HTML)──/v1/viewer/ws──► hub ──ServerMsg::Viewer*──► agent screencast 模块 ──CDP──► 常驻 Chrome。帧下行走二进制通道(新 tag 0x03,JPEG 原样不解码);输入上行走 JSON。hub 新增 viewer 连接类型 + viewer_sessions 路由(骨架照抄 PTY session)。

**范围调整(写计划时据摸查决定,已与用户确认):**
- **proto 共享 crate 延到 P3**:四份镜像非逐字相同(client 是子集),抽取是非平凡重构;P2 投屏帧不产生新 Rust 镜像消费者(验证页是 JS)。P2 仍照现有"双文件镜像"惯例加 agent↔hub 帧(hub/tunnel.rs ↔ agent/tunnel.rs 锁步),但**严格逐字**,P3 抽 crate 时一并收编。
- **单活动页投屏**:P2 投"最近活动的非 about:blank 页 target";精确 per-session 页映射留 P4。
- **不碰 app**:P2 显示端只有 hub 托管的验证页(也是未来 webterm/app viewer 面板的协议雏形)。

**基线:** P1 已就位(ChromeManager.cdp_http_url、browser.enabled)。本计划新增 agent screencast 模块 + hub viewer 中继 + 验证页。

**验收(spec P2):** browser=on 的 agent + claude 开着某页;浏览器打开 `http(s)://<hub>/viewer?session=<id>`:实时看到该页;在画布上点击/打字(含中文)→ 页面响应。

---

## Task 1: agent CDP screencast 模块(核心,最难)

**Files:** `crates/agent/src/browser/screencast.rs`(新)、`browser/mod.rs`(挂载)

CDP 客户端:连 Chrome 的 target ws,开 screencast 收帧、注入 Input。用 tokio-tungstenite(已有)+ serde_json 手封最小 CDP(不引 chromiumoxide,符合单二进制哲学)。

事实(P1 spike + CDP 文档):
- `GET <cdp_http_url>/json` 列 targets,每个含 `type`(page/...)、`url`、`webSocketDebuggerUrl`。
- 选页:type==page 且 url 非 `about:blank`、取列表中第一个(P2 单页简化);无则回退第一个 page。
- 连该 target 的 ws,发 `{"id":N,"method":"Page.enable"}`、`{"id":N,"method":"Page.startScreencast","params":{"format":"jpeg","quality":60,"maxWidth":1280,"maxHeight":800,"everyNthFrame":1}}`。
- 收 `{"method":"Page.screencastFrame","params":{"data":"<base64 jpeg>","sessionId":K,"metadata":{...}}}` → **必须** ack:`{"id":N,"method":"Page.screencastFrameAck","params":{"sessionId":K}}`(不 ack 则停推)。
- 输入注入(同 ws):`Input.dispatchMouseEvent`{type:mousePressed/mouseReleased/mouseMoved/mouseWheel,x,y,button,clickCount,deltaX/Y,modifiers}、`Input.dispatchKeyEvent`{type:keyDown/keyUp/char,key,code,text,modifiers}、`Input.insertText`{text}(IME/粘贴整串)。

设计:
- `ScreencastSession`:`async fn start(cdp_http_url, frame_tx: mpsc::Sender<Vec<u8>>) -> Result<Self>` —— 选页、连 ws、startScreencast、spawn 读循环(screencastFrame→base64 decode→`frame_tx.send(jpeg_bytes)`+ack;其他 CDP 消息忽略/对应 id 的回执丢弃)。`fn input(&self, ViewerInputEvent)` —— 把输入事件翻成 CDP 命令发 ws(经一个 cmd mpsc 给 ws 写半边)。`async fn stop(self)` —— stopScreencast + 关 ws。
- `ViewerInputEvent` enum(模块内,P3 抽 proto 时上移):`MouseMove{x,y}`、`MouseButton{x,y,button,down,click_count}`、`Wheel{x,y,dx,dy}`、`Key{key,code,text,down,modifiers}`、`InsertText{text}`。坐标已是 viewport 像素(验证页换算)。
- base64 用现有 `base64` dep。

**测试:**
- CDP 命令构造纯函数:`fn cdp_start_screencast_cmd(id)`、`fn cdp_ack_cmd(id,session)`、输入事件→CDP JSON 映射(`mouse_event_to_cdp`、`key_event_to_cdp`、`insert_text_to_cdp`)—— 各断言 JSON 形状。
- 选页逻辑 `pick_page_target(targets_json) -> Option<String>`(纯函数):多 target 选非 about:blank 的 page;全 about:blank 回退第一个 page;无 page 返回 None。表驱动测试。
- `#[ignore]` 真 Chrome 集成:ChromeManager 起 Chrome → navigate(用 playwright-mcp 或直接 CDP Page.navigate 到 data: 页)→ ScreencastSession.start → 断言 frame_tx 在 5s 内收到 ≥1 个非空 JPEG(magic bytes FF D8)→ 注入一次 MouseMove 不报错 → stop。本机跑一次记录。

提交 `feat(agent): CDP screencast module (frame stream + input injection)`。

## Task 2: agent↔hub 投屏帧 + hub viewer 路由骨架

**Files:** `crates/agent/src/tunnel.rs` + `crates/hub/src/tunnel.rs`(逐字锁步)、`crates/hub/src/registry.rs`

- 二进制 tag:两份 tunnel.rs 加 `pub const TAG_SCREENCAST_FRAME: u8 = 0x03;`(与现有 0x01/0x02 并列)。pack/unpack 复用(格式 [tag][16B id][payload];这里 id 用 **viewer_session_id**)。
- `ServerMsg`(hub→agent)加(锁步两份):
  - `ViewerAttach { viewer_session_id: Uuid, session_id: Uuid }` —— hub 通知 agent:某 viewer 要看 session 的浏览器,开 screencast。
  - `ViewerDetach { viewer_session_id: Uuid }` —— 停。
  - `ViewerInput { viewer_session_id: Uuid, event: ViewerInputEvent }` —— 人工输入下发(ViewerInputEvent serde 定义放 tunnel.rs,锁步)。
- `ClientMsg`(agent→hub)加 `ViewerClosed { viewer_session_id: Uuid, reason: Option<String> }`(screencast 失败/页关)。
- `registry.rs`:`AgentConn` 加 `viewer_sessions: DashMap<Uuid, mpsc::Sender<Vec<u8>>>`(viewer_session_id → 帧通道);`handle_binary_frame` 按 tag 分发(0x02→现有 PTY,0x03→viewer_sessions);`classify` 给 `ViewerClosed` 路由(新 Routing::Viewer(uuid) 或复用 oneshot 机制——简单起见新增 viewer 路由)。`register_viewer/unregister_viewer` 方法。
- agent ws.rs:分发 `ServerMsg::ViewerAttach/Detach/ViewerInput` 到一个新的 `ViewerManager`(per viewer_session 持 ScreencastSession;Attach 起、Detach 停、Input 注入);screencast 帧经 `pack_pty_frame(TAG_SCREENCAST_FRAME, viewer_session_id, jpeg)` 发 hub。

**测试:** 帧 serde 往返(新 ServerMsg/ClientMsg 变体 + ViewerInputEvent);tag 分发单测(喂 0x03 帧 → 路由到 viewer_sessions);锁步 diff 两份 tunnel.rs 的新增块逐字相同。

提交 `feat: viewer protocol frames + hub binary tag routing for screencast`。

## Task 3: hub /v1/viewer/ws 路由 + 验证页托管

**Files:** `crates/hub/src/viewer_session.rs`(新)、`crates/hub/src/app/viewer.rs`(新,托管 HTML)、`crates/hub/src/main.rs`(挂路由)

- `/v1/viewer/ws?session=<id>` upgrade handler(`viewer_session.rs`,骨架照抄 `pty_session.rs` 的 ws split + select! 但简化):
  1. 鉴权:复用 pty_session 的鉴权路径(cookie/account);拒绝未授权。
  2. 生成 `viewer_session_id`;向目标 session 的 agent 发 `ServerMsg::ViewerAttach{viewer_session_id, session}`;在 AgentConn 注册 viewer 帧通道。
  3. select!:agent 帧通道收到 JPEG → `ws.send(Binary(jpeg))`;viewer ws 收到 Text(JSON input)→ 解析成 ViewerInputEvent → 发 `ServerMsg::ViewerInput` 给 agent。
  4. 断开:`ViewerDetach` + unregister。
- `app/viewer.rs`:`serve_viewer_html()` 返回单 HTML(canvas + 内联 JS):连 `/v1/viewer/ws?session=`(从 URL query 读 session),`binaryType=arraybuffer`,收帧 `URL.createObjectURL(Blob([data],{type:'image/jpeg'}))`→Image→drawImage(用完 revoke);捕获 mousemove/down/up/wheel/keydown/keyup + **IME compositionend → InsertText**,坐标按 canvas:viewport 比例换算成 viewport 像素,JSON 发上行。
- main.rs:`.route("/viewer", get(app::viewer::serve_viewer_html))` + `.route("/v1/viewer/ws", get(viewer_session::upgrade))`。

**测试:** input JSON↔ViewerInputEvent 解析单测(hub 侧);viewer_session 路由的帧转发逻辑能抽纯函数就抽(类 M1 translate);HTML 由 P5 手动冒烟覆盖。

提交 `feat(hub): viewer ws relay + standalone screencast verify page`。

## Task 4: 端到端集成 + 冒烟

- `#[ignore]` 集成(agent 侧或 hub 侧):假 viewer ws client 连 hub(带鉴权)→ 触发 ViewerAttach → 真 Chrome screencast → 断言收到 JPEG 二进制帧 → 发一条 input JSON → 不报错。(真 Chrome 那半与 Task 1 集成测试共享 setup。)
- 冒烟文档 `2026-06-10-desktop-app-p2-e2e-smoke.md`:agent browser=on + 进 claude 开页;浏览器开 `/viewer?session=<id>`(session id 从哪拿:agent 日志/admin UI/claude 的 workspace);看到实时画面;鼠标点击、中文输入验证;关页停帧验证(agent 日志 screencast stopped)。已知限制:单活动页、无音频、JPEG 画质档、延迟一个往返。
- `cargo test --workspace` 全绿、零警告、push。

## Self-Review 备忘
- proto crate 延 P3 已记录;P2 的锁步加帧严格逐字(M1 吃过镜像漏洞,Task 2 含 diff 校验)。
- 二进制 tag 0x03 复用 pack/unpack:viewer_session_id 占用 session_id 字段位(16B),agent 侧 ViewerManager 按它分发。
- 风险:CDP target 选页在多页时不稳(P2 单页简化已声明);screencast ack 节流(everyNthFrame=1 + quality=60 控带宽,P2 固定档,自适应留后续)。
- 复用:hub viewer_session 路由骨架抄 pty_session;鉴权抄现有;帧通道机制抄 PTY sessions DashMap。
