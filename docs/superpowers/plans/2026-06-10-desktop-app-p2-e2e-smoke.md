# Desktop App P2 — 端到端冒烟验证（人工，在你的环境跑）

> 目的：在真实 hub + 真实 agent（browser=on）+ 真实浏览器上，手动确认 P2 投屏通道全链路工作：
> 看到 claude 操作的实时页面，人工点击/输入（含中文）能注入回去，关页能停帧，且**跨账号鉴权拒绝**。
>
> 自动化覆盖（已绿，无需你重跑）：
> - agent 端到端（真 Chrome）：`cargo test -p cloudcode-agent viewer_attach_streams -- --ignored --nocapture`
>   → ViewerManager.attach → ScreencastSession → OutFrame 上拿到 `TAG_SCREENCAST_FRAME (0x03)` 二进制帧（按 viewer_session_id 键控、JPEG `FF D8`），注入鼠标 + 中文 InsertText 不报错，detach 干净。
> - 本文档覆盖的是**自动化无法替代的那半**：真浏览器画布渲染、真 IME 合成、跨账号鉴权、关页停帧。

---

## 前置条件

1. **P1 就位**：agent 起得来且 `browser.enabled = true`（常驻 Chrome 能 ready）。验证：agent 日志有 `Chrome ready cdp=http://127.0.0.1:<port>`。
2. **agent 可被浏览器经 hub 触达**：hub 监听一个浏览器能访问的地址（`http(s)://<hub>`）；agent 已注册到该 hub（hub 日志可见 agent 上线）。
3. **一个登录账号**：你能在浏览器里以某账号登录 hub（拿到 `cc_user_session` cookie）。viewer 页**只认 cookie**，没有 CLI/Hello token 旁路。
4. **第二个账号**（用于鉴权负向用例）：另一个能登录 hub 的账号，且**不**拥有被观察的 session。

---

## 步骤 A：起服务 + 进 claude + 开一个真实页

1. 起 hub（确保 viewer 路由挂上：`/viewer` 与 `/v1/viewer/ws`）。
2. 起 agent，配置 `browser.enabled = true`（P1 的 BrowserConfig；确认 Chrome 路径可自动探测或显式给出）。
3. 从 CLI 或 webterm **打开一个 workspace 并进入 claude**（即开一个 PTY session）。
4. 让 claude **打开一个真实页面**，例如让它访问 `https://example.com`（或任意非 `about:blank` 页）。
   P2 是**单活动页**投屏：投「最近活动的非 about:blank page target」。

## 步骤 B：拿到 session_id（P2 没有现成 UI，从日志取）

`session_id` 是 **PTY session 的 id**（OpenSession 时分配，存进 hub 的 `workspaces` 映射 `(agent, account, workspace) -> session_id`）。P2 还没做「app 里列 live sessions」（那是 P3 的 app 面板），所以最简单的来源：

- **首选——agent / hub 日志**：开 session 时日志会带 `session=<uuid>`（PtyOpened / OpenSession 路径）。webterm 打开 workspace 后，对应那条 PTY 连接的 `session_id` 即是。
- 备选：如果你的部署带 admin UI 且它列出了 live PTY 连接（`AppState.pty_live` 之类），从那里读 session_id；P2 不依赖此项。

> 说明：viewer ws 的 `resolve_session_owner` 就是反扫这张 `workspaces` 表，按 `session_id` 命中拿到 `(agent, account)`。所以你填的 session_id 必须是**当前 live** 的那个；已关闭/没开过的会被拒。

## 步骤 C：打开 viewer 看实时画面

1. 在**已登录（拥有该 session 的账号）**的浏览器里打开：
   ```
   http(s)://<hub>/viewer?session=<session_id>
   ```
2. 预期：canvas 上**实时显示** claude 那个页面的画面（JPEG 帧流，画质档 60、最大 1280x800）。
   - hub 日志出现 `viewer attached viewer=<vid> session=<sid> agent=<name>`。

## 步骤 D：人工输入注入（含中文 IME）

在 viewer 画布上操作，预期页面**有反应**（延迟约一个往返）：

- **移动鼠标 / 点击**：在画布上点一个链接/按钮 → 页面响应（CDP `Input.dispatchMouseEvent`，坐标按 canvas↔viewport 比例换算成 viewport 像素）。
- **打字（ASCII）**：聚焦一个输入框，敲键盘 → 字符出现（CDP `Input.dispatchKeyEvent`）。
- **中文（IME）**：用输入法打一段中文，**合成完成**（compositionend）后整串注入 → 中文出现在输入框（CDP `Input.insertText`，整串而非逐键）。

## 步骤 E：关页停帧

- 关闭 viewer 标签页（或断开 ws）。
- 预期：
  - hub 日志 `viewer detached viewer=<vid>`；hub 向 agent 发 `ViewerDetach`，agent 侧 `ViewerManager.detach` 停 screencast、关 CDP ws。
  - 若是**页面那侧**先关（claude 关了那个 page / target 消失），agent 的帧通道关闭 → 上报 `ClientMsg::ViewerClosed { reason: "screencast ended" }`，hub 收到后结束该 viewer 中继。
  - 任一情况：帧停止流动，CDP screencast 被 stop，无残留 ws/任务。

---

## 鉴权负向用例（T3 的账号归属守卫，必须验证）

打开 viewer 时**用另一个账号**（或未登录）访问同一个 `session_id`：

- **未登录 / 无有效 cookie**：`/v1/viewer/ws` upgrade 直接 `401 login required`，socket 不开。
- **登录但非该 session 所有者**（owner_account != cookie account）：ws 打开后立即被 `POLICY` 关闭，reason `not your session`；看不到任何画面。
- 附带：`session_id` 不存在/已关 → `session not found` 关闭；目标 agent 离线 → `agent offline` 关闭。

> 这条守卫的意义：没有它，任何登录账号都能猜 session_id 偷看别人浏览器。

---

## 已知限制（P2 范围，记录在案）

- **单活动页**：只投「最近活动的非 about:blank page target」；精确 per-session 多页映射留 **P4**。
- **画质 / 尺寸固定**：JPEG quality 60、maxWidth 1280 / maxHeight 800、everyNthFrame 1，无自适应码率（留后续）。
- **输入回显延迟**：注入走 viewer → hub → agent → CDP 一个往返，约一个 RTT 的延迟。
- **无音频**：纯画面 + 输入，不含音频通道。
- **proto 未抽共享 crate**：agent↔hub 帧仍是「双文件逐字镜像」（tunnel.rs 锁步），P3 抽 crate 时收编。
</content>
</invoke>
