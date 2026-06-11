# Desktop App P6 — 端到端冒烟验证（人工，在 macOS 上跑）

> 目的：在真实 hub + 真实 agent 上，手动确认 P6 cmux 式 UI + tabbed multi-target viewer 全链路工作：
> 统一深色主题、持久侧边栏 IA、workspace 切换零损失（tmux 持久化叙事）、
> 带 tab 条的多 target CDP 镜像面板、注意力光环 (attention halo)、重连横幅。
>
> 自动化覆盖（已绿，无需你重跑）：`cargo test --workspace`（372 个纯逻辑单测，6 ignored）——
> 含主题 token smoke、`apply_event` reducer 全路径（连接/切换/重连/错误）、tab 标题截断
> (`tab_label` 中英文)、`auto_select` 表驱动（10 cases，含 [blank, real] 初选）、agent 侧
> `preferred_target` 表 + forwarder 断流/静默 detach 两路（Fix 1）、`frame_is_live` 边界、
> `switch_decision` 表、sidebar `row_badge` 表、`TargetInfo` JSON 往返、`targets_wire_json`、
> hub uplink `parse_viewer_uplink`（含 select_target）、`apply_target_event` 表驱动、
> JPEG decode 2×2 fixture、letterbox/panel→frame 坐标换算、IME 合成态、全部 P4 viewer 逻辑。
> 本文档覆盖**自动化无法替代的那半**：真 GUI 渲染、真投屏帧、真侧边栏切换、真 tab 条、
> 真 bell/光环注入、真断线恢复横幅。（GUI 不能在 headless CI 跑 —— 同 P3/P4 处境，诚实声明。）

---

## 前置条件

1. **agent `[browser] enabled=true`**：P6 的 tab viewer 靠 agent 的常驻 Chrome 投屏。
   agent 的 `config.toml` 里 `[browser] enabled = true`（且本机有 Chrome/Chromium）。
   若 `enabled=false`：浏览器面板不崩，但 tab 条永远空 + 帧永远不到（agent 侧 TargetWatcher
   起不来 → 不推 ViewerTargets → app tab 条保持空/占位）。
2. **本分支 hub（`feature/desktop-app`）**：P6 协议版本 14；hub 须发 `ViewerTargets` 文本帧
   下行 + 接受 `{"kind":"select_target"}` 上行。旧版 hub tab 条空（而不崩）。
   同时本分支 hub 的 `SessionOpened` 携带 `session_id` — viewer ws 依赖此字段。
3. **config 就位**：`~/.config/cloudcode/config.toml`：
   ```toml
   hub_url = "https://<your-hub>"
   token   = "cc_xxxxxxxx"
   ```
4. **至少两个 workspace**：冒烟步骤 2（切换英雄流程）需要。可在侧边栏用 "+ new workspace" 创建。
5. **macOS**：中文 IME 步骤以 macOS 拼音输入法为准；dock bounce 最佳效果。

---

## 构建 / 启动

```bash
source $HOME/.cargo/env
cargo run -p cloudcode-app
# 或查看详细日志：
RUST_LOG=debug cargo run -p cloudcode-app
```

- 首次构建较慢；之后增量很快。
- agent 侧日志（TargetWatcher 事件、screencast start/stop）可观察 CDP 生命周期。

---

## 步骤清单（逐项打勾）

### 1. 主题与 IA：统一深色主题 + 持久侧边栏

- [ ] `cargo run -p cloudcode-app` → 初始连接：**全屏居中 spinner**（`connecting to <hub>…`）——
      此时无侧边栏（第一次 Welcome 到来前没有列表，诚实不展示空壳）。
- [ ] 连接成功后（hub Welcome）→ 进入**双列布局**：
      左侧**持久侧边栏**（宽约 220 px，可拖动）+ 右侧内容区。
      **没有单独的 picker 全屏**——侧边栏就是 picker，永在。
- [ ] 侧边栏色调：`#181825`（BG1）深色；workspace 行清晰可读（白色主名 + 灰色 `@agent`）。
- [ ] 每个 workspace 行左侧有状态点：
      - `●` 绿 = agent 在线 + tmux 活跃；
      - `○` 暗灰 = agent 在线但无 tmux session；
      - `◌` 深 = agent 离线（行不可点）。
- [ ] 侧边栏底部：hub 状态（`● connected <账号>` 绿色）。
- [ ] 内容区为 `#11111b`（BG0）深底；居中提示 `select a workspace to attach`。
- [ ] 整体：sidebar/顶栏/banner/viewer tab 条/terminal/browser 面板色调统一，
      无 "两个 app 缝在一起" 的割裂感（这是吸取 cmux 割裂教训的关键检查点）。

---

### 2. 英雄流程 —— 切换与保活（tmux 持久化叙事）

> 核心宣言：**切 workspace ≠ 关进程**。旧 workspace 的 claude 在 agent 的 tmux 里继续跑；
> 切回来即重 attach，零损失。这是 CloudCode 的差异化叙事，这一步要亲眼确认。

- [ ] 在侧边栏**点击 workspace A** → 短暂 "reconnecting…" 旋转器（连接循环初始化 session）→
      进入 session 屏：顶栏 `ws-A@agent`；左终端显示 claude TUI；右可选 browser 面板。
- [ ] 在 claude 里**开始一个耗时任务**（例如：让 claude 写一段代码/分析文件，确认它在持续工作）。
- [ ] **点击侧边栏 workspace B**（不同 workspace）→
      - 顶栏变为橙色横幅 `reconnecting to <hub>…` + 旋转器（几百毫秒级别）；
      - 面板**置灰但不清空**（terminal grid 保留，旧内容可见）；
      - 横幅消失，进入 workspace B 的 session：顶栏 `ws-B@agent`，B 的 claude 就绪。
- [ ] **点回 workspace A** → 同样短暂重连横幅 → 进入 A 的 session →
      **claude 的任务还在继续**（tmux 在 agent 上一直活着，切走期间未中断）。
- [ ] **验证「切换零损失」**：输出/进度与切走前连续，没有被中断或回到初始状态。

> **实现注意（"switch via reconnect" 机制）**：
> 切换实际走「关连接 + 重连 + 自动 reopen 目标 workspace」路径，非会话内指令。
> 橙色横幅是这条路径的诚实展示，不是 bug。见下「已知限制」。

> **冷启动 relaunch**：`last_active` 保存在进程内存中，**不跨重启持久化**。
> 退出 app 再重启后不会自动 attach 上次的 workspace——侧边栏列出所有 workspace，
> 用户点击所需的行即可（tmux 还在跑，重 attach 仍零损失）。

---

### 3. Tabbed Viewer —— 多 target CDP 镜像

> claude 每打开一个新页面，tab 条就多一个 tab；关页面 tab 消失。

- [ ] 确保处于 **Split** 或 **Browser** 视图（顶栏右侧三态按钮，或 Cmd+B 循环）。
- [ ] claude 尚未打开页面时，右侧面板显示 **`agent browser idle — pages claude opens appear here`**
      （viewer ws 已连但 agent Chrome 无页面）。
- [ ] **让 claude 打开第一个网页**（例如：`open https://example.com`）：
      - tab 条出现 **第一个 tab**（标题 `Example Domain`，截断至 24 字符）；
      - tab 为高亮状（ACCENT 蓝色文字 + 底部 2px 蓝色下划线）；
      - 右面板开始渲染该页 JPEG 投屏（letterboxed，深灰背景条边）；
      - 右下角出现 `LIVE · 1280×800`（或 agent 配置的 viewport 尺寸）半透明角标。
- [ ] **让 claude 打开第二个网页**（例如：`open https://baidu.com`）：
      - tab 条新增 **第二个 tab**；
      - 流自动切到新打开的 tab（新 target 出现即自动切换，V1 简化逻辑）；
      - 新 tab 高亮，第一个 tab 变为非激活（TEXT_MUTED 灰色）。
- [ ] **点击第一个 tab** → 流切换到第一个页面：
      - agent 侧 Chrome 的**前台 tab 随之切换**（`Page.bringToFront` 副作用 ——
        screencast 只投可见 tab，所以点 tab = 把那页提到 Chrome 前台，agent Chrome 里可见变化）；
      - 右面板渲染第一个页面内容；
      - `LIVE` 角标在切换后约 0-2s 内重新亮起。
- [ ] **让 claude 关闭第二个页面**（例如：`close the baidu tab` / `page.close()`）：
      - 第二个 tab 消失；
      - 流自动 fall back 到剩余的第一个 tab（`auto_select` 逻辑）；
      - 若第二个 tab 是当前活动 tab，切换无需用户干预。
- [ ] **让 claude 关闭所有页面** → tab 条消失，面板回到空闲占位
      `agent browser idle — pages claude opens appear here`。
- [ ] **`[about:blank, 真实页面]` 初选探针**：让 agent Chrome 同时存在一个 `about:blank`
      tab 和一个真实页面（如 example.com），然后 attach（切到 Browser/Split 视图触发
      viewer ws 重建）→ tab 条**高亮真实页面的 tab**，且面板渲染的正是该页内容
      （高亮与实际流一致）。两侧共享规则：「首个非 (about:blank|chrome://) 页面；
      否则首个页面；否则无」——agent `preferred_target` / `pick_page_entry_for_session`
      与 app `auto_select` 必须 lockstep。

---

### 4. figma.com WebGL 检查

- [ ] 让 claude 打开 `https://www.figma.com`（登录可选；未登录可看到主页）。
- [ ] viewer 面板渲染出 figma.com 的页面内容（**WebGL 合成输出被 CDP screencast 捕获**）。
- [ ] 画布区域 / 编辑器 WebGL 内容正常出现（不是全黑或全空白）；`LIVE` 角标持续亮。
- [ ] 鼠标移动到面板内 → 坐标正确映射（移动事件注入到 figma 页面）。

---

### 5. LIVE 角标

- [ ] 正常投屏中：右下角显示 `LIVE · <宽>×<高>` 半透明小角标（BG2 背景 pill + TEXT_MUTED 字）。
- [ ] **停帧验证**：将视图切到 **Terminal only**（顶栏按钮或 Cmd+B）→ viewer ws 断开 →
      再切回 **Split/Browser** → viewer ws 重连，等待首帧到达 → `LIVE` 重新出现。
- [ ] **2s 超时验证**（可选，但能做就做）：若 agent Chrome 帧停止（短暂关 Chrome 或者
      claude 没在操作页面），等待约 2 秒 → `LIVE` 角标自动消失（`frame_is_live` 2s 窗口）；
      帧恢复 → 角标重新出现。

---

### 6. 注意力光环 (Attention Halo)

- [ ] 在 claude 的 session 中，终端面板拥有焦点（点击一下终端区域）。
- [ ] 触发 terminal bell：在终端里输入 `printf '\a'`（BEL 字符）并回车，或等待 claude
      自然发出铃声（如任务完成提示）。
- [ ] **光环触发**：
      - 终端面板出现 **2px ACCENT 蓝色描边**（圆角，inset 1px，不被 reconnect overlay 遮挡）；
      - 侧边栏该 workspace 行**右侧出现 ACCENT 蓝点**（未 hover 时可见；hover 时被 delete/reset 图标遮住）。
- [ ] **清除**：在终端里**按任意键**（如空格或 Enter）→ 光环消失，蓝点消失。
- [ ] **再次 ring bell** → 光环重现；再次按键 → 再次清除。
- [ ] **app 未聚焦时的 dock bounce**（最佳效果）：将 app 窗口切到后台（Cmd+Tab 切到其他 app），
      在终端里触发 bell（ssh 到 agent 或让 claude 跑一个产生 bell 的命令）→ macOS Dock 里
      cloudcode 图标**短暂弹跳**（`RequestUserAttention(Informational)`）。

---

### 7. 重连横幅（Reconnect Banner）

- [ ] 在 session 中**短暂 kill 掉 hub**（停止 hub 进程或断网）。
- [ ] 立即：
      - 内容区**顶部出现橙色横幅**（`reconnecting to <hub>…` + 旋转器 +
        `· your session keeps running on the agent` 灰色小字）；
      - 侧边栏底部状态变为橙色 `● reconnecting…`；
      - **terminal 面板置灰但不清空**（terminal grid 保留，用户能看清是哪个 session）；
        **browser 面板回到空闲/断连占位**（viewer ws 随连接掉线，texture 被清掉——
        **不是**保留最后一帧；只有终端 grid 是置灰保留的）；
      - **输入被 disable**（terminal/browser 不接收键盘/鼠标，UI 已 disabled_ui 包住）。
- [ ] 恢复 hub → app 自动重连（backend 的退避重试）：
      - 橙色横幅消失；
      - 侧边栏底部回到绿色 `● <账号>`；
      - app 自动 reopen 上次的 workspace（`last_active` → `FollowUp::OpenSession`）→
        进入同一 workspace 的 session（tmux 一直在跑，重 attach 无损失）；
      - 面板恢复可用。
- [ ] 验证：没有卡死 Error 全屏；没有清空 terminal 内容（重连期间保留）。

---

### 8. agent Chrome 崩溃 → 干净断流（Fix 1 探针）

> agent 的常驻 Chrome 半路死掉（崩溃 / 被 kill）时，viewer 不应冻在最后一帧：
> agent 侧 forwarder 检测到 browser 级 CDP ws 断开，主动上报
> `ViewerClosed { reason: "browser connection lost" }`，hub 关闭 viewer 路由。

- [ ] 投屏进行中（`LIVE` 角标亮着），**在 agent 机器上 kill 掉 agent 的 Chrome 进程**
      （注意是 agent 拉起的那个 headless/常驻 Chrome，不是你本地浏览器）。
- [ ] app 侧：**干净断流**而非冻帧——browser 面板回到占位文字（viewer ws 被 hub 关闭 →
      Disconnected → texture 清空），`LIVE` 角标消失；终端面板不受影响。
- [ ] agent 日志可见 `target watcher channel closed (browser connection lost)`。
- [ ] **恢复**：等 agent 把 Chrome 拉回（或重启 agent）后，**切到 Terminal-only 再切回
      Browser/Split**（清除 viewer 的 retry_blocked 闩，见下「生命周期说明」）→
      viewer ws 重建 → tab 条与投屏恢复。
- [ ] 对照：正常 detach（切 Terminal-only / 切 workspace）**不**产生上述
      `browser connection lost` 日志或多余的 ViewerClosed（deliberate-detach 静默路径）。

---

## 双面板 + tab bar 生命周期说明（给排查用）

- **viewer ws 懒连接**：只在 Browser / Split 视图时建立；Terminal-only → 断开（agent 停投屏），
  切回 Browser/Split → 重建（新的 session_id 的 viewer ws）。
- **掉线后不重连闩**：viewer ws 掉线（agent `browser.enabled=false` 或网络抖）后，不会
  每帧自动重连（防止 busy-loop）。重连需**切到 Terminal-only 再切回**（清除 retry_blocked 闩）。
- **tab 条仅在 targets 非空时显示**：无目标时面板显示占位文字，tab 条不出现。
- **`Page.bringToFront` 副作用**（V1 已知设计点）：tab 条点击会把 agent Chrome 的对应 tab
  推到前台（CDP screencast 只投可见 tab 故必须如此）。这意味着「切 tab = 改变 agent Chrome
  的前台 tab」，agent Chrome 里可见的页面随之变化——这是预期行为，不是 bug。
- **tab 关闭按钮缺失（V1）**：tab 条没有 close 按钮；关页面通过 claude 的工具（
  `browser.closePage` / MCP 等）操作。
- **切换 workspace 走重连路径**（见步骤 2）：切换 = 关连接 + 重建 + 自动 reopen 目标；
  橙色横幅是这条路径的诚实展示，持续约几百毫秒，视网络而定。

---

## 已知 P6 限制

- **多 viewer tab 争抢**：Chrome 只对前台 tab 出帧（CDP screencast 限制）。两个 viewer
  （例如 webterm 验证页 + desktop app）同时附加且**选了不同 tab** 时，每次选 tab 都把
  自己的页提到 Chrome 前台（`Page.bringToFront`），会互相把对方的流冻住（对方 `LIVE`
  角标消失，帧停在最后一张）。solo-use 单人操作下可接受，V1 不解决。
- **注意力光环仅覆盖当前 attach 的 session**：Bell 只在附加中的 PTY 流里可检测；未 attach
  的 workspace 无法感知 bell（跨会话通知需 agent 侧钩子，P7+ 未来工作）。
- **tab 关闭按钮缺失（V1）**：关页面需通过 claude。
- **切换 workspace 展示短暂重连横幅**（cosmetic）：这是 switch-via-reconnect 机制的副作用，
  非用户错误；几百毫秒自愈。
- **多 tab 滚动走细滚动条**：tab 条 tab 多时水平滚动（`floating: true` 的细滚动条），
  滚动条细（6px），触摸板滑动可用。
- **cold relaunch 不自动 attach**：`last_active` 在进程内存中，重启 app 不自动重开
  上次 workspace（侧边栏点击即可手动 reattach，tmux 仍在运行）。
- **viewer ws 掉线需手动重连**：掉线后切 Terminal-only 再切回 Browser/Split。
- **Linux IME（fcitx/ibus）未验证**：浏览器面板 + 终端面板 IME 均以 macOS 为准。
- **GUI 在 CI 未测**：纯逻辑单测 369 个覆盖所有 reducer/widget 逻辑；真 GUI 渲染/tab 切换
  /真 bell/真断线恢复靠本文档人工冒烟。
