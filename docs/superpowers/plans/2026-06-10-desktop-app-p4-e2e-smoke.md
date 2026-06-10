# Desktop App P4 — 端到端冒烟验证（人工，在 macOS 上跑）

> 目的：在真实 hub + 真实 agent 上，手动确认 P4 分屏 + 浏览器面板全链路工作：
> session 屏变成**左终端（claude）+ 右浏览器投屏**；让 claude 开网页 →
> **右面板实时渲染该页** → 鼠标点击 / 中文 IME 输入接管页面 → 拖分隔条调比例 →
> 三态全屏切换 → 关浏览器面板停帧 → per-session 页隔离（P2 安全项，**已缓解未根治**，见下）→
> 断线重连回 picker。
>
> 自动化覆盖（已绿，无需你重跑）：`cargo test -p cloudcode-app`（139 个纯逻辑单测）——
> 含 viewer ws URL 构建、ViewerInputEvent JSON 往返（对齐 hub 形）、JPEG 解码、
> letterbox / panel→frame 坐标换算、egui key/IME→ViewerInputEvent 映射、IME 合成态、
> 分屏比例 clamp、**viewer ws 生命周期决策 `reconcile_viewer_action`**
> （连接 rising edge / 已连 idle / 掉线后不重连 / 隐藏面板断连并清除重连闩）。
> agent 侧 `pick_page_for_session` 表驱动测试 + `#[ignore]` 真 Chrome 集成（见 page-mapping notes）。
> 本文档覆盖**自动化无法替代的那半**：真 GUI 渲染、真投屏帧、真鼠标 / 中文 IME 注入到远端 Chrome、
> 真分隔拖动、真断线恢复。（GUI 不能在 headless CI 跑 —— 同 P3 处境，诚实声明。）

---

## 前置条件

1. **agent `browser.enabled=true`**：P4 的右面板靠 agent 的常驻 Chrome 投屏。agent 的
   `config.toml` 里 `[browser] enabled = true`（且本机有 Chrome/Chromium）。
   **若 `enabled=false`**：右面板**不崩**，但永远停在占位
   `browser idle / not connected`（agent 的 screencast 起不来 → hub 关 viewer ws →
   app 收 `Disconnected` → 占位）。见「已知 P4 限制」。
2. **匹配的 hub（本分支 `feature/desktop-app`）**：`SessionOpened` 现在**携带 `session_id`**，
   app 用它开第二条 viewer ws（`/v1/viewer/ws?session=<id>&token=<token>`）。旧 hub 不发
   session_id → 右面板停在占位（app 日志 `viewer: no session_id; browser panel unavailable`）。
3. **config 就位**：和 P3 / CLI client **同一个** `~/.config/cloudcode/config.toml`：
   ```toml
   hub_url = "https://<your-hub>"   # http/https/ws/wss 都行
   token   = "cc_xxxxxxxx"
   ```
   app 的两条 ws（PTY + viewer）共用这个 token 鉴权。
4. **macOS + 中文输入法**：系统里装好拼音输入法，冒烟中文输入时切到它。

---

## 构建 / 启动

```bash
cargo run -p cloudcode-app
```

- **首次构建很慢**（eframe / winit / alacritty_terminal / zune-jpeg 等一大堆依赖），之后增量很快。
- 想看日志：`RUST_LOG=debug cargo run -p cloudcode-app`（默认只 warn；viewer ws 的连接 /
  掉线在 debug/warn 级别有日志）。
- 自定义 config 路径：`cargo run -p cloudcode-app -- --config /path/to/config.toml`。
- 想看 agent 侧投屏起停：跟 agent 的日志（screencast start / ViewerClosed / 帧 pump）。

---

## 步骤清单（逐项打勾）

### 1. 连接 + 进 session（P3 仍工作）
- [ ] `cargo run -p cloudcode-app` → 看到 **connecting** spinner → 进 **Workspaces** picker
      （右上 `account: <账号>`）。
- [ ] 点某在线 workspace 的 **Open** → 进 session 屏，标题 `session: name@agent`，右侧 `cwd: …`。
- [ ] 默认就是 **Split** 视图：**左**是终端面板，claude 的 TUI 正常渲染（颜色 / 光标 /
      box-drawing / 滚动 / Enter / 方向键 —— P3 那套全在）；**右**是浏览器面板。

### 2. 切分屏 + 让 claude 开网页 → 右面板实时渲染
- [ ] 工具栏右侧三态切换 **Browser / Split / Terminal**（当前态高亮）；或按 **Cmd/Ctrl+B**
      循环 Terminal → Split → Browser → 回 Terminal。切到 **Split**（默认）或 **Browser**，右面板出现。
- [ ] claude 还没开页时，右面板显示占位：先 `browser idle / not connected`，
      viewer ws 连上后变 `connecting to browser…`（等首帧）。
- [ ] 让 claude **开一个真实网页**（例如：让它用浏览器工具打开 `https://example.com`）。
- [ ] 右面板**实时渲染该页**（JPEG 帧投屏，letterbox 居中、深灰底边）。标题栏出现绿色 **● live**
      圆点（仅当右面板可见 **且** 真有帧到达时点亮）。

### 3. 鼠标 / 键盘 / 中文 IME 接管页面
- [ ] **点一下右面板抓焦点**（egui 单焦点：点哪个面板哪个获焦，键盘 / IME 只进获焦面板）。
- [ ] 鼠标**移到一个链接上**（移动事件按面板显示尺寸换算回 viewport 像素回传）→ **点击** →
      页面**导航**（输入经 viewer ws → hub → agent 的 Chrome CDP `Input.*` 注入）。
- [ ] 在页面的**文本框**里点一下、用英文打几个字 → 出现在页面输入框（`Event::Text` → `InsertText`）。
- [ ] 切到**拼音输入法**输入中文：合成态由 OS IME 处理，**提交**的中文串作为 `InsertText`
      整串注入 → 出现在页面字段（例如在搜索框打「你好」并能看到）。

### 4. 拖分隔条 + 三态切换 + 关面板停帧
- [ ] **拖动中间分隔条** → 左右比例变化，**有界 20%–80%**（任一面板都拖不没）。
- [ ] 切到 **Terminal**（仅终端）→ 浏览器面板隐藏 → **viewer ws 断开**
      （agent 日志：screencast 停 / `ViewerClosed: screencast ended`；● live 熄灭）。
- [ ] 切回 **Split / Browser** → viewer ws **重连**（lazy connect），帧恢复，● live 重新点亮。
      （这是「隐藏 → 显示」这条**有意的重连手势**；见下方限制里的「无自动重连」。）

### 5. per-session 页隔离（P2/P4 安全项）—— **已缓解，未根治**
- [ ] 开**两个 workspace**（开两个 app 实例，或退出重开），各让 claude 停在**不同的页**。
- [ ] 各自的浏览器面板**应当**显示**本 session 的页**。
- [ ] **诚实的限制**（务必理解）：agent 当前默认 playwright-mcp 配置下，**多个 session 共享同一个
      browser context**，CDP 上没有可据以区分「A 的页」与「B 的页」的 `browserContextId` / `targetId`。
      因此 per-session 选页是**已缓解（plumbing 就位）而非完全隔离**：app 的 viewer ws 带了
      `session_id`，agent 的 `pick_page_for_session(hint)` 选页路径已接好，但默认配置下 `page_hint_for`
      恒为 `None` → **回退到「活动页」**。跨页只在「多账户共享同一 agent」这种**违背 solo-use**
      的部署下才可能发生；单账户自用无此问题。真正根治需 agent 改用 playwright-mcp `--isolated`
      （每 session 独立 context）+ 填充 hint —— 一个后续 config 变更。
      **详见** `2026-06-10-p4-page-mapping-notes.md`（含真机 CDP 实验证据与 `--isolated` 下集成测试通过的结论）。

### 6. 断线重连
- [ ] 在 session 中**短暂 kill 掉 hub**（或断网）→ **两个面板**都进入重连态：
      终端置灰 + 橙色 **reconnecting…** 状态行；浏览器面板回占位（viewer ws 也随之断开）。
- [ ] **恢复 hub** → app 自动重连（PTY ws 退避重试），**回到 workspace picker**（不是卡死 Error 页）。
      重连成功后重开 workspace 即可重新进 session（tmux 若存活则重挂）；进 session 后再切到
      Split / Browser 会用**新 session 的 session_id** 重新建 viewer ws。

---

## 双面板生命周期说明（给排查用）

- **两条独立 ws**：PTY ws（终端，`backend.rs` 拥有，**会自动退避重连**）与 viewer ws
  （浏览器投屏，`viewer/client.rs` 拥有，**不自动重连**）彼此独立。
- **viewer ws 懒连接**：只在浏览器面板**可见**时开（`reconcile_viewer` 每帧对账：
  `Split`/`BrowserOnly` → 该连；`TerminalOnly` → 该断）。隐藏即 detach（→ hub `ViewerDetach`
  → agent 停 screencast），省开销、呼应 P2 按需投屏。
- **掉线后不重连闩**（P4 Task 5 加固）：viewer ws 掉线（如 `browser.enabled=false`
  screencast 永远起不来，或网络抖动）后，**不**在面板仍可见时每帧重连 —— 否则会把 hub/agent
  打成 busy-loop。重连是**有意手势**：把浏览器面板**切走再切回**（隐藏会清除重连闩）。
  纯逻辑由 `reconcile_viewer_action(want, connected, retry_blocked)` 决策，已单测。
- **生命周期清理点**（均会 drop viewer handle → agent 停投屏）：
  - 面板隐藏（切到 Terminal-only）；
  - `SessionClosed` / 离开 session 屏（`drain_events` 里 `next` 非 `Session` → 清终端 + 浏览器 + viewer）；
  - **PTY ws 断线（Disconnected）**：reducer 进 `Connecting{reconnecting}`（非 `Session`）→ 同上清理路径**也 drop viewer**；
  - `on_exit`（关窗）。
- **重进 session 干净重建**：新 `SessionOpened` 重建 `BrowserPanel`、重置视图为默认 Split、
  drop 旧 viewer、清除重连闩；之后 `reconcile_viewer` 用**新 session_id** 懒连接。

---

## 已知 P4 限制

- **viewport 尺寸固定、不随面板宽 reflow**：远端 Chrome 按 agent screencast 的固定 viewport
  渲染，app 把该帧 **letterbox** 进面板（保纵横比，留黑边），页面**不会**按面板宽度重排版 —— P5 / 后续。
- **viewer ws 无自动重连**：掉线后需**切走再切回**浏览器面板手动重连（见上「掉线后不重连闩」）。
  这是有意设计（避免对永不投屏的 agent busy-loop），不是 bug。
- **双击发 clickCount=1**：浏览器面板按下/抬起目前恒 `click_count=1`，**没有双击/三击语义**
  （Task 3 备注），页面里需要 dblclick 的交互暂不支持。
- **per-session 页隔离已缓解未根治**：默认 playwright-mcp 配置多 session 共享 context →
  回退活动页；真正隔离需 `--isolated` + hint（后续 config 变更）。详见 page-mapping notes。
  单账户自用无影响；多账户共享 agent 违背 solo-use 模型。
- **Linux IME（fcitx/ibus）未验证**：浏览器面板 IME 与终端面板同处境，P4 以 macOS 为准。
- **GUI 在 CI 未测**：只有纯逻辑单测覆盖（139 个）；真投屏渲染 / 输入注入 / 分隔拖动 / 重连的
  真实表现靠本文档人工冒烟。
