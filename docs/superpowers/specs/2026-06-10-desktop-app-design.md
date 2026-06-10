# CloudCode 桌面端(terminal app + 浏览器映射)— 设计

> 一个纯 Rust 单二进制桌面应用:窗口内分屏显示 claude 终端会话(PTY 流)与 agent 端浏览器的实时映射(CDP 投屏),nmux 式体验 —— 但 claude 与浏览器都运行在 agent,本地只是显示与交互层。

- **状态**:设计已与用户逐段评审通过,待用户审阅本文档后进入实现计划
- **日期**:2026-06-10
- **分支**:feature/desktop-app(spec 所在;实现按里程碑开子分支)
- **实现方式**:用户将以 /goal 驱动实现,本 spec 为其输入,要求自包含

## 背景与动机

CloudCode 现状:claude 跑在 agent(有 `claude /login` 的主机),用户经 CLI(TUI)或 webterm(浏览器 SPA)接入,hub 中继 PTY 字节流。

此前的 `feature/local-browser` 线(M1-M3,已封存退役,见"既有资产处置")把浏览器放在用户 client 机器上、经 hub 反向隧道 MCP 帧 —— 验证可行,但暴露根本矛盾:headless⇄headed 切换要重启浏览器(丢页内状态)、登录态绑定单一 client 机器、webterm 用户无法支持。

新架构反转:**浏览器单实例常驻 agent**(与 claude 同信任域,登录态统一持久),需要人看/操作时把**画面与输入**经 hub 流到显示端(CDP screencast/Input,非 VNC,纯协议零系统依赖)。桌面 app 是新的主显示端:终端 + 浏览器映射同窗分屏。

## 已定决策(用户拍板记录)

| 决策点 | 选择 | 理由/备注 |
|--------|------|----------|
| 技术栈 | **纯 Rust 原生,单二进制**(否决 Tauri/Electron) | 团队 Rust 哲学;复用 client 的 Rust 协议层(proto/wire);零 webview 依赖 |
| UI 底座 | **egui/eframe(wgpu 后端)** | 壳 UI(分屏/设置)免费;自定义绘制区嵌终端与 viewer;有 egui_term 先例 |
| V1 面板范围 | **终端 + 浏览器两面板** | 聚焦核心价值;file manager/多 tab 不作 V1 验收项 |
| CLI client 去留 | **app 逐步取代 CLI** | CLI 进维护模式;浏览器场景 CLI 退化为打印投屏 URL |
| M1-M3 处置 | **封存退役,新架构全面接替** | feature/local-browser 分支保留不删不合;可复用件(mcp_endpoint、握手回放、超时档)在新架构重用 |
| 投屏方式 | **CDP screencast + Input**(非 VNC/Xvfb) | 纯协议、跨平台、headless 可投、变化驱动帧 |
| 浏览器面板 IME | 合成完整串 `Input.insertText` 注入 | 与终端面板同一 IME 哲学;webterm IME 经验平移 |

## 整体架构

```
┌──────────────── agent 主机 ─────────────────┐
│ claude ──MCP(localhost)──► mcp_endpoint      │
│                              │ 短路接本地子进程(不再经 hub/client)
│                              ▼
│                       playwright-mcp ──--cdp-endpoint──┐
│                                                        ▼
│   投屏模块(新) ◄────CDP screencast/Input────── Chrome(常驻 headless,
│        │                                （单实例,--remote-debugging-port 仅 localhost）
└────────│────────────────────────────────────┘
         │ hub 中继:帧流↓ / 人工输入↑(新二进制通道,类 PTY 流路由)
         ▼
┌─────── cloudcode-app(新 crate,单二进制桌面应用)───────┐
│  eframe/egui 壳:窗口、分屏布局、focus 路由、设置        │
│  ┌─终端面板────────────┬─浏览器面板──────────────┐    │
│  │ alacritty_terminal  │ CDP viewer              │    │
│  │ (VTE 状态机)         │ JPEG 帧→wgpu 纹理        │    │
│  │ + 自绘网格/CJK 字体   │ + 键鼠/IME→输入上行       │    │
│  │ + winit IME→PTY 流   │                         │    │
│  └─────────────────────┴─────────────────────────┘    │
│  连接核心:复用 client 的 proto/wire(经共享 crate)       │
└────────────────────────────────────────────────────────┘
```

### 组件清单

| 组件 | 位置 | 职责 | 复用 |
|------|------|------|------|
| **agent 浏览器底座** | crates/agent | Chrome 常驻实例管理(监督重启);playwright-mcp 以 `--cdp-endpoint` 连同一实例;mcp_endpoint 短路直连本地子进程 | M3 的 mcp_endpoint、握手回放、3 档超时直接搬 |
| **agent 投屏模块** | crates/agent | CDP `Page.startScreencast` 拉帧、`Input.dispatchMouseEvent/KeyEvent/insertText` 注入;按 ViewerAttach/Detach 启停(无人看零开销) | 新写;CDP 客户端可用现成 crate(如 chromiumoxide)或轻量自封 ws+JSON |
| **hub 投屏中继** | crates/hub | 新二进制帧通道,复用 PTY 流的 session 路由模式;viewer 鉴权走现有 account/ACL | 路由骨架照抄 PTY |
| **cloudcode-proto(顺势改进)** | crates/proto(新) | 现有四份手工镜像(hub/pty_proto↔client/proto、hub/tunnel↔agent/tunnel)收成一份 + 投屏新帧;hub/client/agent/app 四方引用 | 根治镜像锁步(M1-M3 两次踩坑) |
| **cloudcode-app** | crates/app(新) | egui 壳 + 双面板 + 连接核心 | client 的 wire/config 经共享化复用 |

### 架构红利(相对 M1-M3)

claude 的浏览器自动化路径完全 agent 本地化:无跨网 MCP 帧、无 client 依赖、无授权门弹窗(同信任域,访问控制由 hub 账号体系在投屏入口把门)、登录态单点持久、webterm 用户未来可加投屏页全功能支持。

## 终端面板设计(最大单体,工作量黑洞,逐项落实)

`alacritty_terminal` 提供 VTE 解析、grid 状态、滚动缓冲、内置 Selection 模型;渲染与输入自做:

| 子问题 | 方案 | 评估 |
|--------|------|------|
| 网格渲染 | egui 自定义 widget;按行扫描合并同 style 的 run 批量绘制;dirty 行驱动重绘 | 中;egui_term 先例可参考结构 |
| CJK | wcwidth 双宽格对齐;egui 字体 fallback 链(等宽英文+中文,如 Sarasa Mono/Noto Sans Mono CJK);**字体打进二进制**(单二进制哲学,不赖系统字体) | 中;V1 必须做对(用户群中文) |
| IME | winit `Ime` 事件:`Preedit` 合成态光标处内联灰显,`Commit` 串直写 PTY 流;webterm 的合成态时序经验平移 | 中高;macOS 最稳,Linux fcitx/ibus P3 实测 |
| 选择/复制 | alacritty_terminal `Selection`(块选/语义选词)+ 鼠标事件映射;复制走 arboard | 低 |
| 滚动 | grid scrollback(沿用 50k 行约定)+ 滚轮/触控板映射 | 低 |
| resize | 面板像素 ÷ 字格尺寸 → cols/rows → 现有 Resize 协议帧 | 低 |
| 性能结构 | PTY 字节→VTE 状态机跑专用线程(alacritty 同款);egui 仅 dirty repaint;验收:打字回显无感、cat 大文件不卡 UI | 结构性,V1 即按此搭 |

**V1 明确不做**:链接点击、搜索、主题系统(深色默认)、字体设置 UI(配置文件指定)。

## 浏览器面板与投屏协议

**面板侧**:JPEG 帧 → zune-jpeg 解码 → wgpu 纹理 → egui Image;鼠标(坐标按 面板:viewport 比例换算)/滚轮/键盘事件 + IME Commit 串 → 输入消息上行。光标形状跟随留 V1.5。

**投屏协议**(hub 中继,显示端无关 —— app 与未来 webterm 投屏页共用):

```
帧下行:  ScreencastFrame { session, seq, format(=jpeg,留扩展), jpeg_data, viewport_w, viewport_h }
输入上行: ViewerInput { session, event }
          event = Mouse{kind,x,y,buttons,modifiers} | Key{code,modifiers,down}
                | InsertText{text}(IME/粘贴) | Wheel{dx,dy,x,y}
控制:    ViewerAttach/Detach { session }   // agent 据此启停 screencast
         ViewerResize { w,h }              // V1 可固定 1280x800,字段先留
```

要点:按需投屏(Attach 才 startScreencast,断线即停);帧格式带 `format` 字段留 H.264 升级位;同 session 多 viewer 允许(hub 扇出,输入不抢锁,信任域内 V1 简单化);`InsertText` 是 IME 正确性的关键(整串注入,不模拟逐键)。

## app 会话流程与连接核心

```
启动 → 读配置(沿用 client 的 config.toml 格式/位置)
  → ws 连 hub(复用 wire 层)
  → workspace 选择页(egui 列表,语义对齐现 menu.rs:跨 agent 列表/新建/删除/takeover)
  → OpenSession → 终端面板激活
  → 浏览器面板初始占位;用户点开或 claude 开始浏览器活动 → ViewerAttach → 帧流
```

- **能力协商退役**:app 不上报 `browser_capable`(字段保留 serde 兼容,语义废弃);agent 配置 `browser = on/off` 决定是否给 claude 注入 MCP 配置。
- 断线重连:沿用 client 重连语义;UI 用 egui toast + 面板置灰;投屏重连后重新 Attach。
- 终端与投屏共用一条 hub ws(新消息类型,不开第二连接)。

## 错误处理

| 故障 | 处置 |
|------|------|
| agent 上 Chrome 崩溃 | agent 监督重启常驻实例;playwright-mcp 对外部 CDP 的重连行为是 P1 首周必验风险;重连不及则重启 playwright-mcp 子进程 + mcp_endpoint 短路层做握手回放(M3 代码直接搬) |
| hub 断线 | 双面板置灰 + 自动重连;投屏 Detach→重连后 Attach |
| 帧解码失败/乱序 | 按 seq 丢旧帧;坏帧跳过下一帧自愈 |
| claude 自动化与人工输入并发 | 信任域内不抢锁,事件自然交错;文档注明人工操作时建议让 claude 暂停 |
| viewer 全部离开 | 停 screencast,自动化照常,零投屏开销 |

## 里程碑(供 /goal 拆分,每阶段独立可验)

| 阶段 | 内容 | 独立验收标准 |
|------|------|-------------|
| **P1 agent 浏览器底座** | Chrome 常驻(localhost CDP)+ playwright-mcp `--cdp-endpoint` 同实例 + mcp_endpoint 短路本地(搬 M3 握手回放/超时档);agent 配置 browser=on/off | 现有 CLI/webterm 进 claude 让它开网页:全程不经 client;重开 session 登录态仍在(agent profile 持久) |
| **P2 投屏通道** | cloudcode-proto 共享 crate(四镜像收一 + 投屏帧)+ agent 投屏模块 + hub 帧流中继 + 临时 web 验证页(canvas 渲帧,验协议,亦为未来 webterm 投屏页雏形) | 浏览器开验证页:实时看到 claude 操作的页面,能点击能输入(含中文) |
| **P3 app 骨架+终端面板** | crates/app:egui 壳、连接核心、workspace 选择页、完整终端面板(CJK/IME/选择/滚动/resize) | 用 app 完整跑一个 claude 会话;中文输入/显示无碍;cat 大文件不卡 |
| **P4 app 浏览器面板+分屏** | viewer 面板、分屏布局(可调比例/全屏切换)、focus 路由 | 单窗口内:左边 claude 干活,右边实时看它操作浏览器,随时上手接管 |
| **P5 收尾** | 打包(dmg/AppImage)、对接现有 tag→CI release 机制、CLI 打印投屏 URL 退化路径、文档 | 新装机:下载→打开→配 token→可用 |

## 测试策略

- **proto crate**:帧序列化往返单测(沿 M1 模式)。
- **agent CDP 模块**:CI 可跑真 headless Chrome 的集成测试(启动 Chrome→screencast 收帧→Input 注入→断言页面变化)—— 本架构自动化可测性优于 M1-M3。
- **投屏协议回环**:假 agent↔假 viewer 经 hub 中继的帧/输入往返。
- **终端面板**:alacritty_terminal 自带测试兜底;渲染层 golden 测试可选;CJK/IME 手动清单(P3 验收附录)。
- **e2e 冒烟**:每里程碑一份手册(沿 M1-M3 文档惯例)。

## 风险表

| 风险 | 应对 |
|------|------|
| playwright-mcp `--cdp-endpoint` 连外部 Chrome 的行为细节(launch vs connect 差异) | P1 首周验证;不行则降级:playwright-mcp 自管浏览器、其启动的 Chrome 同样开 remote-debugging 供投屏连 |
| winit IME 在 Linux(fcitx/ibus)成熟度 | P3 实测;macOS 优先交付 |
| egui 大网格渲染性能 | dirty 行优化;不达标再上 glyph cache/instanced 绘制 |
| Chrome 常驻内存占用 | agent 配置可关(browser=off);空闲回收策略留 P5 评估 |
| 单二进制内置 CJK 字体的体积 | 子集化(subset)等宽 CJK 字体;体积预算 ~10-15MB 可接受 |

## 既有资产处置

- `feature/local-browser`(M1-M3,d3a3343)及三个里程碑分支:**保留封存,不合 dev,不再演进**。设计文档与"claude MCP 行为/playwright-mcp 进程树"等工程认知是本 spec 的直接输入。
- 直接复用件:mcp_endpoint(短路改造)、MCP 握手回放、3 档超时、playwright-mcp 启动参数集(--headless/--user-data-dir/--cdp-endpoint)、echo 桩测试法。
- webterm:不在 V1 范围;投屏协议设计成显示端无关,P2 的验证页即其未来投屏面板的雏形。

## 验收总标准(V1 完成定义)

单二进制 cloudcode-app:打开 → 连 hub → 选 workspace → 左侧终端跑 claude(中文输入/显示完好),右侧实时映射 agent 浏览器(claude 操作可见、人工可点击输入、中文可输),分屏可调、全屏可切;agent 重启 Chrome 后投屏与自动化自愈;CLI 用户保留打印投屏 URL 的退化路径。
