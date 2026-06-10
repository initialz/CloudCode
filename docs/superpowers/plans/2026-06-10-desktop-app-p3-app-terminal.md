# Desktop App P3 — app 骨架 + 终端面板 实现计划

> **For agentic workers:** REQUIRED SUB-SKILL: superpowers:subagent-driven-development.

**Goal:** 纯 Rust egui 桌面应用(`crates/app`)能连 hub、选 workspace、进 claude 会话,终端面板完整:VTE 渲染、CJK 显示、IME 中文输入、选择复制、滚动、resize。完成后 = 用 app 跑完整 claude 会话,中文无碍。

**Architecture:** eframe/egui 应用;复用 client 的 wire(transport)+ 抽取的 cloudcode-proto(协议类型);终端面板 = alacritty_terminal(VTE 状态机 + grid + Selection)+ 自绘 egui widget;PTY 字节流喂 VTE,egui 键鼠/IME 事件转 PTY 输入字节。

**关键约束:** GUI 无法在 headless 环境跑 —— 验证靠:**编译干净 + 纯逻辑单测(VTE 喂入、grid→渲染数据、输入→字节、IME 态)+ 用户手动冒烟**(同 M3 TUI 的处境)。每个 GUI 任务的"验收"是编译 + 单测 + 冒烟清单项。

**范围:** 仅终端面板(浏览器面板 = P4)。app 此刻是单面板终端 + workspace 选择。

**基线复用蓝图(摸查已确认):** wire.rs 自包含可搬;ClientToHub/HubToClient + AgentInfo/WorkspaceInfo 抽 proto;config load/open_session/list_workspaces 等 API 脱离 ratatui 可搬;PTY 字节走裸 binary(in_bin_rx);client 是 hub 协议的子集(缺 SplitPane/ChangeLayout)。

---

## Task 1: cloudcode-proto 共享 crate(根治 client↔hub 镜像)

**Files:** `crates/proto/`(新)、`crates/client/src/proto.rs`、`crates/hub/src/pty_proto.rs`、`crates/client/src/`(引用)、`crates/hub/src/`(引用)、根 `Cargo.toml`

仅抽 **client↔hub** 协议(app 作为 client 只需这个);agent↔hub(tunnel.rs)和 viewer 类型不动(P4 再说)。

- [ ] 建 `crates/proto`:`cloudcode-proto`,deps 仅 `serde`/`uuid`(workspace 版本)。`lib.rs` 放 **超集** `ClientToHub`(含 hub 独有的 `SplitPane`/`ChangeLayout` + `SplitDirection`/`PaneLayout`)、`HubToClient`、`AgentInfo`、`WorkspaceInfo`、`PTY_PROTOCOL_VERSION`。逐字取自 `hub/pty_proto.rs`(它是超集)。
- [ ] 加进根 Cargo.toml `[workspace] members`;`[workspace.dependencies]` 加 `cloudcode-proto = { path = "crates/proto" }`。
- [ ] `crates/hub`:删 `pty_proto.rs` 的枚举定义,改 `pub use cloudcode_proto::*;`(或直接在用到处 `use cloudcode_proto::...`);hub 依赖加 proto。**hub 现有代码不改逻辑**,只换类型来源。
- [ ] `crates/client`:同样 `proto.rs` 改为 re-export `cloudcode_proto`;client 依赖加 proto。client 之前缺 SplitPane 等 —— 现在 enum 多了这些变体,client 不构造它们即可(match 处若 exhaustive 会要求加臂 → 加 `_ => {}` 或显式忽略;检查 relay/menu 的 match)。
- [ ] **wire 格式不变验证**:proto crate 加 serde 往返测试(每个变体);跑 `cargo test -p cloudcode-hub -p cloudcode-client` 确认现有测试全过(wire 兼容性 = 序列化字节不变)。
- [ ] `cargo build --workspace` 零警告。提交 `refactor: extract cloudcode-proto crate (client<->hub protocol)`。

## Task 2: app 骨架 — eframe + 连接 + workspace 选择页

**Files:** `crates/app/`(新)

- [ ] `crates/app` crate:bin `cloudcode-app`。deps:cloudcode-proto、tokio(full)、serde、toml、anyhow、bytes、tokio-tungstenite(workspace 版)、dirs、`eframe`(pin 0.28)、`egui`(随 eframe)。
- [ ] 搬 `wire.rs`(transport,自包含)进 `crates/app/src/wire.rs`,改用 `cloudcode_proto` 类型(注释:与 client/wire.rs 是 transport 双份,非协议镜像,若现第三消费者再抽 crate)。搬 config load/resolve(改名 `HubConfig`)。
- [ ] eframe app 结构:`App` 状态机 enum `{ Connecting, Picker{workspaces}, Session{...}, Error }`。tokio runtime 在后台线程跑 wire(eframe 主线程跑 UI),二者用 channel 通信(UI→后台发命令,后台→UI 发 HubToClient/PTY 字节,egui `ctx.request_repaint()` 驱动)。
- [ ] 连接流程:启动读 config → 后台 `wire::connect` → 等 `Welcome` → `list_workspaces`(搬 API)→ Picker 态。
- [ ] Picker UI:egui 列表展示 WorkspaceInfo(name@agent + badge:在线/tmux活/有client takeover 提示),按钮 新建/删除/打开;打开 → `open_session` → 等 `SessionOpened` → Session 态(终端面板,Task 3 填充)。
- [ ] **不在 headless 跑 GUI**:`cargo build -p cloudcode-app` 干净;后台连接逻辑(命令 enum、状态转移纯函数)单测;`cargo run` 留用户冒烟。
- [ ] 提交 `feat(app): eframe skeleton + hub connect + workspace picker`。

## Task 3: 终端面板 — VTE + 渲染

**Files:** `crates/app/src/terminal/`(mod、render)

- [ ] deps 加 `alacritty_terminal`(pin 0.24 或当前稳定,用 context7/docs 核对 API)。
- [ ] `TerminalPanel`:持 `alacritty_terminal::Term`(+ 必要的 EventListener/dimensions)。PTY 字节(in_bin_rx)喂 `term.handle_input`/Processor(按该版本 API);VTE 在后台或 UI 线程推进 —— 简单起见 UI 线程每帧消费 channel 里的字节批量推进(大流量后续优化,先正确)。
- [ ] 自绘 egui widget:遍历 `term.grid()` 可见行,按 cell 的 (fg,bg,flags) 合并相邻同 style run,用 egui `painter` 画背景矩形 + 文本;光标(块/下划线,按 term 状态)。等宽字格尺寸由字体度量定。
- [ ] **纯逻辑单测**:喂一段已知 ANSI 字节序列 → 断言 grid 内容(如 `\x1b[31mhi` → (0,0)='h' fg=red);run 合并函数 `rows_to_runs(grid_row) -> Vec<Run>` 表驱动测试。渲染本身靠冒烟。
- [ ] `cargo build` 干净;提交 `feat(app): terminal panel — alacritty VTE + grid render`。

## Task 4: 终端 — CJK 字体 + IME 中文输入 + 键盘

**Files:** `crates/app/src/terminal/{input,fonts}`、assets

- [ ] **字体内置**:打包一款等宽 + CJK fallback 字体(Sarasa Mono SC 子集 或 Noto Sans Mono CJK SC,体积预算 ~10-15MB,可子集化常用汉字)进二进制(`include_bytes!`);egui FontDefinitions 配 fallback 链(等宽英文 → CJK)。wcwidth 双宽对齐(`unicode-width` crate)。
- [ ] **键盘 → PTY 字节**:egui `Event::Key` / `Event::Text` → 终端控制字节(回车=\r、退格、方向键 CSI、Ctrl 组合、可打印字符 UTF-8)。映射表纯函数 `key_to_bytes(key, modifiers) -> Option<Vec<u8>>` + 测试(回车、Ctrl-C=0x03、方向键、Esc)。
- [ ] **IME**:egui/winit IME 事件 —— `Event::Ime(Ime::Preedit)` 合成态在光标处内联灰显(覆盖渲染,不写 PTY);`Ime::Commit(s)` → s 的 UTF-8 字节写 PTY。开 IME(`ctx.output_mut().ime` 或 eframe viewport ime allowed)。`unicode-width` 校准合成态显示宽度。
- [ ] 纯逻辑单测:key_to_bytes 全表;IME 态机(preedit 设置/清除、commit 产出字节)纯函数测试。中文渲染/IME 真表现靠冒烟(macOS 优先)。
- [ ] 提交 `feat(app): CJK fonts + IME composition + keyboard->PTY input`。

## Task 5: 终端 — 选择复制 + 滚动 + resize

**Files:** `crates/app/src/terminal/`

- [ ] **选择**:alacritty_terminal `Selection`(点选/块选/语义选词);egui 鼠标 press/drag/release → 更新 Selection;复制走 `arboard`(或 egui 剪贴板)把选区文本写系统剪贴板;Cmd/Ctrl-C 复制、右键菜单可选。
- [ ] **滚动**:scrollback(沿 50k 行约定,term 配置);egui 滚轮/触控板事件 → `term.scroll_display`;滚动条可选。
- [ ] **resize**:面板像素 ÷ 字格度量 → cols/rows;变化时 `term.resize` + 发 `ClientToHub::Resize{cols,rows}` 给 hub(节流,避免拖动狂发)。
- [ ] 纯逻辑单测:像素→cols/rows 换算函数;resize 节流逻辑。选择/滚动靠冒烟。
- [ ] 提交 `feat(app): selection+copy, scrollback, resize`。

## Task 6: 集成 + 冒烟 + 收尾

**Files:** smoke 文档

- [ ] 串起 Session 态:终端面板 + 重连(搬 client 的 backoff 语义,UI 用 toast + 置灰);SessionClosed/SessionError/Ping/Hub 断线处理。
- [ ] 冒烟文档 `2026-06-10-desktop-app-p3-e2e-smoke.md`(用户在 macOS 跑):`cargo run -p cloudcode-app` → 连 hub(同 client 的 config.toml)→ 选 workspace → 进 claude → 终端正常渲染 claude TUI(颜色/光标/滚动)→ **中文输入**(IME 合成态正确、提交入 PTY、claude 收到中文)→ 选择复制 → 拖窗口 resize claude 跟随重排。逐项 checklist。
- [ ] `cargo test --workspace` 全绿、`cargo build --workspace`(含 app)零警告、push。

## Self-Review 备忘
- proto crate 仅 client↔hub(YAGNI);wire 在 app/client 双份是 transport 非协议(已注释,第三消费者再抽)。
- GUI 不可 headless 测 —— 纯逻辑尽量抽函数测,渲染/IME/选择靠冒烟,与 M3 TUI 同处境(诚实声明)。
- 风险:egui/eframe/alacritty_terminal 版本 API(用 context7 核对,pin 版本);egui IME 在 Linux 成熟度(macOS 优先,Linux P3 后补);大流量 VTE 性能(先正确,dirty/线程优化留后)。
- P4 衔接:Session 态会再加浏览器面板 + 分屏;终端面板设计成可嵌入分屏布局的 widget(不假设全窗口)。
