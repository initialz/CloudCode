# Desktop App P6 — cmux 式 UI + Tabbed Multi-Target Viewer 设计与计划

> **For agentic workers:** REQUIRED SUB-SKILL: superpowers:subagent-driven-development.
> 本文档 = 设计修订(对 2026-06-10-desktop-app-design.md 的 UI/IA 升级)+ 实现计划,经用户确认三个关键点后定稿。

**Goal:** app 的 UI/交互向 cmux 看齐(纵向富信息侧边栏、注意力光环、统一深色主题),viewer 升级为**带 tab 的多 target CDP 镜像面板**(tab = agent 端 Chrome 的页面;未来可扩展到 Electron 等 CDP 端点)。

**用户已确认的三个决定:**
1. **只投可见 tab** —— 后台 tab 仅显示标题,切换 tab 时停旧 screencast 起新的;
2. **V1 target 范围 = 常驻 Chrome 的页面**,其他 CDP 应用(Electron 等)留扩展位(agent 配置注册额外 CDP 端点)不实现;
3. **侧边栏常驻取代选择页** —— workspace 列表永在左侧,点击切换会话。

**调研依据:** "nmux" 实为 **cmux**(Manaflow)。其受好评设计:纵向元数据侧边栏(被公认的杀手特性)、面板注意力光环、浏览器为平级面板、claude 保持原生 PTY、键盘优先。其教训:主题割裂(侧边栏/终端两套)、稳定性差、多 agent 共享浏览器上下文遭诟病。我们的差异化王牌:**会话活在 agent 上 —— 关 app 再开一切还在跑**,切 workspace = 重 attach,零损失;UI 要把这个叙事放在中心。

---

## 设计

### 窗口布局(cmux 式)

```
┌──────────────┬─────────────────────────────────────────────────┐
│ CloudCode    │ 顶栏: ws名@agent · 分支/cwd       [终端|分屏|镜像] │
│──────────────│─────────────────────────┬───────────────────────│
│ ▢ ws1 @mac   │                         │ tab条: [●百度][github]│
│   ●运行 ◉等你 │   终端面板               │┌─────────────────────┐│
│              │   (claude 原生 TUI)      ││                     ││
│ ▢ ws2 @mac   │                         ││  当前 tab CDP 镜像   ││
│   ○已保存     │   claude 等输入时        ││                     ││
│              │   面板亮 accent 描边光环  ││                     ││
│ + 新建        │                         │└─────────[LIVE 1280]┘│
│──────────────│─────────────────────────┴───────────────────────│
│ ◉ hub已连 acct│ (重连时: 顶栏橙色横幅"重连中…",面板置灰不清空)     │
└──────────────┴─────────────────────────────────────────────────┘
```

### 设计 token(统一主题,一套管全部 —— 吸取 cmux 割裂教训)

```rust
// crates/app/src/theme.rs — 唯一来源,所有面板/侧边栏/chrome 共用
pub struct Theme;  // 常量集
bg0: #11111b  // 窗口底
bg1: #181825  // 侧边栏
bg2: #1e1e2e  // 面板/卡片
border: #313244
text:   #cdd6f4   text_muted: #9399b2   text_faint: #6c7086
accent: #89b4fa   // 唯一蓝,只用于「需要你注意」: 光环/未读点/选中
ok: #a6e3a1   warn: #f9e2af   err: #f38ba8
radius: 6.0      spacing: 4/8/12/16     sidebar_width: 220.0 默认(可拖)
```
(Catppuccin Mocha 系,终端 ANSI 16 色保持 VTE 现有映射不动。)

### 侧边栏(IA 核心改动)

- 行 = workspace 卡片:名字、@agent、状态点(`●`绿=tmux 活/`○`灰=已保存/`◌`暗=agent 离线)、**attention 点**(accent 色,claude 等输入时);hover 显示删除/重置操作。
- 点击行 → 切换会话:**关当前 PTY attach、开新的**(协议本来就是单活动会话;tmux 让旧 workspace 在 agent 上继续跑 —— 这就是持久化叙事,切换零损失)。再点回来即重 attach。
- 底部:hub 连接状态 + 账户名。"+ 新建" 行内展开输入框(名字+agent 选择)。
- **V1 诚实范围**:attention 信号只对**当前 attach 的会话**可检测(PTY 流里的 Bell);未 attach 的 workspace 只显示 tmux 活/离线徽标。跨会话通知需要 agent 侧钩子,记 P7+ 未来工作。

### 注意力光环

- 信号源:alacritty_terminal 的 `Event::Bell`(把 NoopListener 升级为捕获 Bell 的 listener;claude 响铃/OSC 即触发)。
- 表现:终端面板 2px accent 描边 + 侧边栏行 accent 点;用户在终端击键即清除。

### Viewer 面板(tabbed multi-target)

- **tab 条**:每个 CDP page target 一个 tab(标题截断 + 活动点);claude 开新页自动出现,页关自动消失;点击切换(=screencast 切 target)。无 target 时显示空闲占位("agent 浏览器空闲")。
- **状态徽标**:右下角 `LIVE · 1280×800` 半透明角标(镜像非本地的诚实标注)。
- **自动焦点**:claude 操作某页时(P4 的 page hint 降级为此用途)tab 自动切过去 —— V1 简化:新 target 出现即自动切换到它。
- 输入/IME/坐标换算沿用 P4 实现不变。

### Multi-target 协议与 agent 机制

```
agent 新增: TargetWatcher —— browser 级 CDP 连接,Target.setDiscoverTargets(true)
  → targetCreated/targetInfoChanged/targetDestroyed → 维护 targets 列表
  → 变化时推 ClientMsg::ViewerTargets{viewer_session_id, targets} (JSON 文本帧)
ViewerManager: 每 viewer 持「当前 target」;ServerMsg::ViewerSelectTarget{viewer_session_id, target_id}
  → 停旧 ScreencastSession、start_on_target(该 target 的 webSocketDebuggerUrl)
ScreencastSession::start_on_target(ws_url, frame_tx) —— 绕过选页逻辑直连指定 target
hub: 上行 JSON 多一种 {"kind":"select_target","target_id":..} → ServerMsg::ViewerSelectTarget;
     下行 ViewerTargets → viewer ws 的 Text 帧(帧仍走二进制 0x03)
app: ViewerEvent 加 Targets(Vec<TargetInfo>);ViewerCommand 加 SelectTarget(String)
targets 条目: { id, title, url, kind:"page", attached:bool }
```

锁步注意:tunnel.rs 双文件逐字;`TargetInfo` 定义进 tunnel.rs(两份)+ app 侧镜像(hub JSON 为源,同 ViewerInputEvent 模式)。

---

## 计划

### Task 1: agent 多 target 底座 + 协议
- 两 tunnel.rs 锁步加:`TargetInfo`、`ClientMsg::ViewerTargets{viewer_session_id, targets: Vec<TargetInfo>}`、`ServerMsg::ViewerSelectTarget{viewer_session_id, target_id: String}`;PROTOCOL_VERSION 13→14;serde 往返测试。
- `screencast.rs`:`TargetWatcher`(browser 级 ws,setDiscoverTargets,事件→列表维护,纯函数 `apply_target_event(list, event)->list` 表驱动测试);`ScreencastSession::start_on_target(ws_url,…)`(现 start 重构为选页+start_on_target)。browser 级 ws url 从 `/json/version` 的 webSocketDebuggerUrl 取。
- `viewer.rs`:attach 时起 TargetWatcher(per viewer 或共享一个 watcher 多 viewer 订阅——取实现简者,注释)+ 推首份列表 + 自动选初始 target(沿用 pick 逻辑);处理 SelectTarget 切流;targetDestroyed 时若是当前 target → 自动切到剩余第一个或空闲。
- hub:relay ViewerTargets→Text 下行;上行 parse 扩展 select_target→ServerMsg;registry classify 给 ViewerTargets 路由(Routing::Viewer 已有,确认文本帧路径)。
- `#[ignore]` 集成:真 Chrome 开两页 → watcher 列表含两 target → select 切换 → 两个 target 的帧都能拿到(分别断言 JPEG)。跑一次。

### Task 2: app 主题系统 + 侧边栏 IA 重构
- `crates/app/src/theme.rs`:token 常量 + `apply(ctx)`(egui Style/Visuals 全量定制:背景层、widget 圆角、选中色、滚动条);main.rs 启动应用。
- App 结构重构:`Screen{Connecting,Picker,Session}` → **常驻 sidebar + 内容区**:`App{ workspaces: Vec<WorkspaceInfo>, active: Option<ActiveSession{...}>, sidebar_state }`;Picker 逻辑并入 sidebar(列表/新建/删除/badges);点击行 = 关旧 attach(发 Close session 语义——查 backend 现有 close/open 命令,可能要加 UiCommand::SwitchWorkspace 复合命令)开新 OpenSession;reducer/状态机测试更新。
- 重连横幅(顶栏橙条)替代整屏 reconnecting;面板置灰不清空(终端 grid 保留,回连后续流)。
- 顶栏:ws名@agent + cwd;右侧三态切换按钮组(沿用 ⌘B)。
- 纯逻辑测试:sidebar 行徽标推导、切换命令序列 reducer 测试。

### Task 3: viewer tab 条 + 切流 UI
- app `viewer/proto.rs` 加 TargetInfo 镜像 + ViewerEvent::Targets/ViewerCommand::SelectTarget;client.rs 处理 Text 帧(targets JSON)。
- `panel.rs`:tab 条渲染(标题截断、活动高亮、close 不做 V1)、点击→SelectTarget、target 列表空→占位、LIVE 角标、新 target 自动切换。
- 纯逻辑测试:tab 标题截断、targets 列表 diff→UI 状态、自动切换决策 `fn auto_select(old_list, new_list, current) -> Option<id>`。

### Task 4: 注意力光环 + 抛光
- terminal:EventListener 捕获 `Event::Bell` → `attention: bool`;面板描边(accent 2px)+ 侧边栏行 accent 点;任意终端输入清除。测试:Bell 事件→flag 置位、键入→清除(listener 单元可测)。
- 全面板主题统一检查(viewer tab 条/占位/侧边栏/横幅用 theme token,无硬编码色)。
- 状态条(底部):分支/cwd 可后续,V1 放连接状态与提示。

### Task 5: 冒烟 + 收尾
- 冒烟文档 `2026-06-11-desktop-app-p6-e2e-smoke.md`:侧边栏切换(切走再切回,claude 还在跑 —— 英雄流程)、claude 开两个网页→tab 条两个 tab→点击切换→各自实时、figma.com tab(WebGL 镜像验证)、claude 响铃→光环、重连横幅。
- `cargo test --workspace` 全绿、零警告、push。

## Self-Review 备忘
- 协议改动集中 Task 1(tunnel.rs 锁步 + hub relay),app 侧 Task 3 镜像 —— 老规矩 diff 校验。
- 切 workspace 的"关旧开新"要查 backend 现有命令语义(Close 是整连接关闭!可能需要新的 CloseSession-only 语义或直接 OpenSession 顶替——hub 的 takeover 机制本来就支持同账户重开,验证后取最简)。实现者必须先读 hub pty_session 的 OpenSession/teardown 语义再动。
- 多 viewer 共享 TargetWatcher 的并发:V1 取简,逐 viewer 一个 watcher 也可接受(连接数=viewer 数,个位数)。
- cmux 的教训贯穿:主题唯一来源、稳定优先(dirty repaint 别引入忙刷)、claude 原生 PTY 不动。

---

## 实现备注（T1–T4 对设计的实际偏差，诚实记录）

1. **切换走"switch via reconnect"而非独立 CloseSession**：Self-Review 备忘预判了这一疑问。实测发现 hub 拒绝同连接上的第二个 `OpenSession`（"session already open"），而 `Close` 会关闭整条连接。最终取最简路径：`UiCommand::SwitchWorkspace` → backend 发送 `Close` 并按"线路丢失"处理（NOT 用户主动退出）→ 正常退避重连 → re-Welcome → reducer 的 `last_active` 触发 `FollowUp::OpenSession` 自动 reopen 目标 workspace。这意味着切换时会出现短暂的橙色重连横幅（几百毫秒），是 cosmetic 副作用而非 bug，已在冒烟文档和已知限制中如实说明。

2. **`Page.bringToFront` 副作用**：`ScreencastSession::start_on_target` 在连接到新 target 前会发送 `Page.bringToFront`，因为 CDP screencast 对后台 tab 不产生帧。这意味着 tab 条点击切换 = 把 agent Chrome 对应的 tab 推到前台。设计文档说"只投可见 tab"，这个副作用是该设计的必然结论，但实现中体现为 agent Chrome 的前台 tab 被 app 的 tab 点击所驱动——用户可以看到 agent Chrome 里的 tab 随 app tab 条的点击切换。已在冒烟文档"双面板生命周期说明"和步骤 3 中明确标注。

3. **cold relaunch 不自动 attach（设计范围收窄）**：设计文档第 2 步提到"quit → relaunch → auto-reattaches the last workspace"。实现中 `last_active` 仅保存在进程内存（`AppModel` 字段），不写入 eframe 持久化存储，因此冷启动后不自动 reopen。这不影响核心叙事（tmux 仍在运行，侧边栏点击即可零损失 reattach），但与原设计描述有偏差。已在冒烟文档步骤 2 中如实记录为"冷启动 relaunch 不自动 attach"限制。

4. **初始连接时无侧边栏（phase 精化）**：设计图示中侧边栏"永在"。实现区分了两种 Connecting 相态：`reconnecting: false`（初始连接）→ 全屏 spinner，无侧边栏（因为列表还未到，展示空壳无意义）；`reconnecting: true`（断线重连）→ 保留完整布局 + 橙色横幅。两种相态下的 UX 行为不同，但均有充分理由，冒烟文档步骤 1 已体现。

5. **TargetWatcher 每 viewer 独立（不共享）**：Self-Review 备忘提到可共享或独立。V1 实现为每个 viewer session 持有独立的 TargetWatcher（per-viewer 一条 browser 级 CDP ws），连接数等于活跃 viewer 数（通常为 1），没有复杂的多订阅共享机制。代码注释中有说明（"V1: per-viewer watcher"）。
