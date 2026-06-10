# Desktop App P3 — 端到端冒烟验证（人工，在 macOS 上跑）

> 目的：在真实 hub + 真实 agent 上，手动确认 P3 纯 Rust egui 桌面应用（`cloudcode-app`）
> 全链路工作：连 hub → 选 workspace → 进 claude 会话 → 终端面板完整渲染 claude TUI →
> **中文 IME 输入** → 选择复制粘贴 → resize 重排 → **断线重连**回到 picker。
>
> 自动化覆盖（已绿，无需你重跑）：纯逻辑单测 `cargo test -p cloudcode-app`
> （92 个）—— VTE 喂入→grid、grid→render run 合并、key/IME→PTY 字节、IME 合成态、
> 像素→cols/rows、resize 节流、状态机 `apply_event`（含 Disconnected→reconnecting→picker）。
> 本文档覆盖**自动化无法替代的那半**：真 GUI 渲染、真 IME 合成、真选择/剪贴板、真 resize、真断线恢复。
> （GUI 不能在 headless CI 跑 —— 同 M3 TUI 的处境，诚实声明。）

---

## 前置条件

1. **config 就位**：app 读的是和 CLI client **同一个** `~/.config/cloudcode/config.toml`
   （或 `$XDG_CONFIG_HOME/cloudcode/config.toml`）。内容：
   ```toml
   hub_url = "https://<your-hub>"   # http/https/ws/wss 都行
   token   = "cc_xxxxxxxx"
   ```
   如果你已经能跑 `cloudcode`（CLI），app 直接复用该配置，无需额外设置。
2. **hub 可达 + 至少一个在线 agent**：picker 只允许 **Open** `agent_online` 的 workspace
   （离线的标 `[offline]` 且 Open 按钮置灰）。
3. **macOS + 中文输入法**：系统里装好拼音输入法（系统设置 → 键盘 → 输入法），冒烟中文输入时切到它。

---

## 构建 / 启动

```bash
cargo run -p cloudcode-app
```

- **首次构建很慢**（要编 eframe / winit / alacritty_terminal / image 等一大堆依赖，几分钟级别），
  之后增量很快。
- 想看日志：`RUST_LOG=debug cargo run -p cloudcode-app`（默认只 warn）。
- 自定义 config 路径：`cargo run -p cloudcode-app -- --config /path/to/config.toml`。

---

## 步骤清单（逐项打勾）

### A. 连接 + workspace picker
- [ ] 启动后先看到 **"connecting to \<hub_url>"** 的 spinner 页。
- [ ] 连上后进入 **Workspaces** picker，标题栏右侧显示 `account: <你的账号>`。
- [ ] 列表列出 workspace，每行形如 `name@agent` + badge：
      `[online]`（agent 在线、无 tmux、无 client）/ `[reattach]`（tmux 活着，重开会重连）/
      `[takeover]`（已有 client 接管）/ `[offline]`（agent 离线，Open 置灰）。
- [ ]（可选）底部 **New** 输入 name + agent → **Create** 能建出新 workspace 并自动刷新列表；
      **Delete** 能删；**Refresh** 重新拉列表。

### B. 进 claude，终端面板渲染
- [ ] 点某个在线 workspace 的 **Open** → 短暂后进入 session 页，标题 `session: name@agent`，
      右侧 `cwd: …`。
- [ ] claude 的 TUI 在终端面板里**正常渲染**：
  - [ ] **颜色**（claude 的高亮、状态行配色）。
  - [ ] **光标**（块/下划线随 claude 状态）。
  - [ ] **box-drawing 框线**（claude 的边框 `│ ─ ╭ ╮` 等不串位）。
  - [ ] **滚动**：鼠标滚轮 / 触控板上滑能进 scrollback 看历史，下滑回到底部。
- [ ] 键盘：敲一条命令，**Enter** 生效（claude 收到并响应）；方向键、退格、Ctrl-C 等正常。

### C. 中文输入（IME）—— P3 的重头戏
- [ ] 点一下终端面板**抓焦点**（边框/光标提示已聚焦）。
- [ ] 切到拼音输入法，开始打字：
  - [ ] **灰色带下划线的 preedit（合成态）内联出现在光标处**（还没提交，未进 PTY）。
  - [ ] **候选词窗口锚定在光标位置**（不是飘到窗口角落）。
  - [ ] 选一个候选 → **中文提交、进入 PTY、claude 收到**（例如让 claude 复述你输入的中文，能看到它回显）。
- [ ] 合成态宽度对齐：长拼音串的灰显不把后面的格子挤乱（wcwidth 双宽对齐）。

### D. 选择 / 复制 / 粘贴
- [ ] 鼠标**拖拽框选**终端里一段文本 → 出现半透明蓝色高亮。
- [ ] **Cmd-C** 复制选区到系统剪贴板（去别处 Cmd-V 能粘出刚选的文本）。
- [ ] **Cmd-V** 把剪贴板内容粘进终端（claude 收到，bracketed-paste 模式下不误触发命令）。

### E. resize 重排
- [ ] 拖拽窗口边/角改变大小 → claude 的 TUI **跟着重排**（行列数变化，框线/换行重新排版）。
- [ ] 拖动过程中不卡顿（resize 向 hub 的通知是节流的，只在尺寸稳定后发最终值）。

### F. 断线重连
- [ ] 在 session 中，**kill 掉 hub**（或短暂断网 / 拔 VPN）。
- [ ] 终端置灰，出现 **"reconnecting…"**（橙色）状态行，附 "the session will return to the workspace picker"。
- [ ] **恢复 hub / 网络**后，app 自动重连（退避重试：500ms 起，翻倍，封顶 30s），
      重连成功后**回到 workspace picker**（不是卡死的 Error 页）。
- [ ] 在 picker 里**重新 Open 同一个 workspace** → 若该 workspace 的 tmux 还活着（badge `[reattach]`），
      会**重新挂回原来那个 live tmux 会话**，claude 接着之前的状态继续。

---

## 重连设计说明（给排查用）

- **退避**：mirror CLI client —— `500ms` 起步，每次失败翻倍，封顶 `30s`；每次重连等 `Welcome` 最多 `10s`。
- **事件序列**：wire 死 → backend `emit(Disconnected)` → reducer 进 `Connecting{reconnecting:true}`（置灰 + 状态行）
  → backend 退避循环 `connect`+`Hello`+等 `Welcome` → 成功后 `emit(Connected)` → reducer 回 `Picker` 并自动 `ListWorkspaces`。
- **落点**：重连**回到 picker**（不自动重开 session）。这是最简单且正确的 UX —— server 端 PTY session
  已随断连消失，但 workspace 的 **tmux 若存活**，用户在 picker 重开该 workspace 即重新 attach 到那个 live tmux。
- **keepalive**：hub 在用户 WS 上每 ~25s 发 `Ping`；backend 在 `handle_hub_frame` 里对 `Ping` 回 `Pong`
  （与 CLI client 一致），否则 hub 会判定连接死掉而主动断开。wire 侧另有 45s 读空闲超时兜底，
  静默掉线时主动让 channel 关闭，触发上面的重连路径。
- **初次连接失败**（坏 URL / 启动时 hub 不可达 / 坏 token）**不进重连循环**，直接显示 Error 页 —— 用户还没进到任何地方，明确报错才是对的 UX。

---

## 已知 P3 限制

- **单终端面板**：浏览器面板 / 分屏是 P4，本期只有一个终端。
- **macOS CJK fallback**：优先 `PingFang.ttc`，但本开发机上实际加载的是
  `/System/Library/Fonts/STHeiti Light.ttc`（PingFang 可能缺失）—— **STHeiti 是 macOS 上的 CJK 兜底字体**。
  字体是运行时从系统加载（非内置），缺失则退化成豆腐块（tofu）而非崩溃。
- **选择**：只有简单拖拽框选，**没有双击选词 / 三击选行**。
- **选区高亮可能跨滚动残留**（滚动后旧高亮未必跟随刷新）。
- **Linux IME（fcitx/ibus）未验证** —— P3 以 macOS 为准，Linux 后补。
- **GUI 在 CI 未测**：只有纯逻辑单测覆盖；渲染 / IME / 选择 / 重连的真实表现靠本文档人工冒烟。
