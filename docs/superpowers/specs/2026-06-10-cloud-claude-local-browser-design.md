# 云端 claude 操作本地浏览器 — 设计

> 让运行在 agent 侧的 claude 驱动 **client(用户笔记本)** 上的真实浏览器,而非 agent 主机上的服务端浏览器。复用现有 hub 长连接作为反向通道。

- **状态**:设计已评审通过,待写实现计划
- **日期**:2026-06-10
- **分支**:dev

## 背景与问题

CloudCode 当前拓扑(见 README):

| 角色 | 跑在哪 | 职责 |
|------|--------|------|
| hub | 公网主机 | 中继 + webterm SPA + admin |
| agent | 有 `claude /login` 的主机 | claude 在此跑;Playwright 沙箱也在此 |
| client | 笔记本/终端 | 极薄,只流转 PTY 字节 |

今天的浏览器自动化(Playwright)跑在 **agent** 侧,claude 操作的是"服务端浏览器"。本设计要把浏览器**执行端从 agent 侧搬到 client 侧**,使 claude 能操作用户**本地**浏览器(带其登录态),用于"需要在用户本地浏览器里完成"的需求。

本质不是造新轮子,而是给现有数据流加一条**反向命令通道**:云端发浏览器指令 → 本地执行 → 结果回传。

## 已定决策基线

| 维度 | 决定 | 理由 |
|------|------|------|
| claude 接口层 | 复用 Playwright MCP 语义,工具集不变 | claude 工具原封不动,协议退化成透明转发 MCP 帧 |
| 传输拓扑 | 方案 1:MCP-over-hub-relay | 复用现成鉴权/ACL/中继/重连,无新隧道;延迟优于裸 CDP |
| 本地浏览器 | headless 默认,接管时切 headed | 自动化无感快跑,人工介入时可视 |
| 接管触发 | 显式 `request_handoff` 为主 + client 启发式保底 | 确定可控 + 漏判兜底 |
| 授权模型 | 会话级,绑定 claude 单次任务 | 贴合"我让他做这件事"心智 |
| 放行谓词 | 仅空闲超时 | GRANTED 后不按域/动作中途拦截,逻辑最简(信任面最宽,已接受) |

**被否方案**:方案 2(独立反向隧道)重复造 hub 已有轮子、新增鉴权面;方案 3(只隧道 CDP 到本地 Chrome)CDP 跨公网啰嗦、且 client 要暴露 CDP 端点安全面巨大。

## 架构与组件

```
agent 主机                          hub(公网)              client(笔记本)
┌──────────────────────┐         ┌──────────┐         ┌────────────────────────┐
│ claude (TUI)         │         │ relay     │         │ cc-browser (新)        │
│   │ http/sse MCP     │         │ +路由      │         │   ├ MCP 子进程管理      │
│   ▼ (127.0.0.1+token)│         │  Browser  │         │   │  @playwright/mcp    │
│ cc-mcp 端点(守护进程内)│◄─ws──►│  Rpc 帧   │◄──ws───►│   ├ 授权门状态机        │
│   http/sse⇄BrowserRpc │         └──────────┘         │   └ handoff 控制器       │
└──────────────────────┘                              │      headless⇄headed     │
                                                       │   ▼                      │
                                                       │ 本地 Chrome              │
                                                       └────────────────────────┘
```

| 单元 | 职责 | 接口 | 依赖 |
|------|------|------|------|
| **cc-mcp 端点**(agent 守护进程内,新) | claude 连接的**常驻 localhost HTTP/SSE MCP server**(非 per-session 子进程);双向搬运 JSON-RPC 帧到 hub;在 `tools/list` 层做能力过滤 + 注入 `request_handoff` | HTTP/SSE(对 claude,127.0.0.1 + per-workspace token)↔ `BrowserRpc`(对 hub) | agent 现有 ws 连接 |
| **hub relay**(改) | 在已有 client↔agent 路由上加 `BrowserRpc`/`BrowserClosed` 帧转发 | `pty_proto` + `agent/tunnel` 新变体 | 无新依赖 |
| **cc-browser**(client,新) | 收帧 → 喂本地 `@playwright/mcp` 子进程;托管授权门 + handoff;能力上报 | `BrowserRpc` ↔ MCP 子进程 stdio | Node + Playwright |
| **授权门**(cc-browser 内) | 单次任务级授权状态机;首帧拦截弹确认,任务结束失效 | 拦截 BrowserRpc 流 | 本地确认 UI |
| **handoff 控制器**(cc-browser 内) | 显式 `request_handoff` + 启发式;headless⇄headed 窗口切换 | 注入工具 + 页面事件监听 | Playwright |

### 边界设计要点

- **cc-mcp 端点按 workspace 无状态转发**,纯管道 + tools/list 改写。授权、handoff、浏览器状态全在 client 侧 —— agent 侧改动最小,敏感逻辑离用户最近。
- **单实例多路由**:端点常驻于 agent 守护进程,按 per-workspace token 把每条 MCP 连接路由到对应 client。启动每个 workspace 的 claude 时,agent 注入 workspace 专属 MCP 配置(URL 携带该 token)。仅绑 127.0.0.1;沙箱网络全开(`profile.sb` `allow network*`)故 claude 即便在 Seatbelt 内也能连。
- **`@playwright/mcp` 当黑盒子进程**,不 fork。通过"注入额外工具"和"页面事件监听"在外面包一层。升级 Playwright MCP 不影响我们。
- **hub 只认帧、不解析内容**,browser RPC 对 hub 半透明,符合其"中继不理解 PTY 字节"的设计哲学。

## 数据流与消息集

完整往返(claude 调一次浏览器工具):

```
claude ──http/sse──► cc-mcp 端点 ──ClientMsg::BrowserRpc──► hub
                                                            │ 按 workspace 路由到绑定 client
                                                            ▼
                            cc-browser ◄──HubToClient::BrowserRpc── hub
                               │ ①授权门拦截 ②handoff 判定 ③喂子进程
                               ▼
                          @playwright/mcp ──► 本地 Chrome
                               │ 结果
                               ▼
cc-mcp 端点 ◄──ServerMsg::BrowserRpc── hub ◄──ClientToHub::BrowserRpc── cc-browser
   │
   ▼ http/sse 还给 claude
```

**核心:数据帧全透明。** MCP 自己的 JSON-RPC `id` 负责请求/响应配对,中间层一律不解析、不配对、只搬运。

消息集(两段协议各加一组对偶):

| 协议段 | 新增帧 | 方向 | 载荷 |
|--------|--------|------|------|
| agent↔hub (`agent/tunnel.rs`) | `ClientMsg::BrowserRpc { workspace, payload }` | agent→hub | 透明 MCP 帧(请求+claude 侧通知) |
| | `ServerMsg::BrowserRpc { workspace, payload }` | hub→agent | 透明 MCP 帧(响应+server 通知) |
| | `ServerMsg::BrowserClosed { workspace, reason }` | hub→agent | 通道终止 |
| hub↔client (`hub/pty_proto.rs` + `client/proto.rs`) | `HubToClient::BrowserRpc { payload }` | hub→client | 转发 |
| | `ClientToHub::BrowserRpc { payload }` | client→hub | 回传 |
| | `ClientToHub::BrowserClosed { reason }` | client→hub | client 主动拆通道 |

设计要点:
- **`payload` 用 `Box<RawValue>`**(`serde_json::value::RawValue`)而非 `serde_json::Value` —— 中间层零反序列化,原样透传,省 CPU 且避免 JSON 重排破坏 MCP 帧。
- **路由复用现有 `workspace` 键**。hub 已为 PTY 维护 client↔agent↔workspace 绑定;browser 帧搭同一路由,不引入新会话概念。
- **授权门 / handoff 不进协议**。授权确认是 client 本地 UI;`request_handoff` 是 cc-browser 拦截 `tools/list` 响应时注入的工具,claude 调它时 cc-browser 自截、不转发 playwright-mcp。
- **授权失效映射**:`BrowserClosed` 到 cc-mcp 端点后,被翻译成一个 MCP 错误响应丢给 claude 当前在飞的调用。

## 授权门状态机

cc-browser 内一个独立单元,拦在 `HubToClient::BrowserRpc` 进 playwright-mcp 子进程之前。

```
                  首个 BrowserRpc 到达
        ┌──────────────────────────────────────┐
        │                                       ▼
   ┌─────────┐   approve    ┌──────────┐   hold 住该帧,本地弹确认
   │  IDLE   │◄────────────│ PENDING  │   "云端任务想操作你的浏览器
   └─────────┘             └──────────┘    [域名/动作摘要]  允许?"
        ▲  ▲                 │      │
        │  │          approve │      │ deny
        │  │                  ▼      ▼
        │  │             ┌──────────┐  deny → BrowserClosed{denied} 上传
        │  │             │ GRANTED  │  claude 收到工具错误
        │  └─────────────└──────────┘
        │   任务边界到达 / 手动撤销      │ 后续帧直接放行
        │                              │
        └──── PTY session 关闭(硬顶)──┘
```

四类终止信号(任一触发 GRANTED→IDLE,下一帧重走 PENDING 二次确认):

| 信号 | 性质 | 说明 |
|------|------|------|
| PTY session 关闭/reset | **硬顶** | session 没了,授权必然失效 |
| 空闲超时(可配,默认 ~10min) | 软 | 近似"任务做完、claude 走了" |
| 用户手动撤销 | 软 | 本地 UI 随时一键收回 |
| 重新进 PENDING | 派生 | 失效后下一次操作 = 新任务 = 重新征得同意 |

**工程约束(显式承认)**:claude 不对外暴露任务起止事件,agent 侧只能看到 PTY 字节流。"任务边界"无法精确捕获,只能用上述复合信号**逼近**。这是 LLM agent 系统的固有限制,设计上明确承认而非假装有干净的 task-end 钩子。

**放行谓词 = 仅空闲超时**:一旦 approve,GRANTED 状态下只问"grant 还活着吗(没空闲超时、session 没关、没被撤销)",不按域名/动作中途拦截。逻辑最简,信任面最宽(已权衡接受)。

> 实现注:放行谓词是状态机的核心,做成纯函数 `should_allow(grant_state, now) -> Decision`,TDD 先行。

## handoff 时序(headless⇄headed)

cc-browser 始终启动**有头但移出屏外/最小化**的 Chrome(切换零状态丢失)。

**路径 A — 显式(claude 主动,为主):**
```
claude: tools/list ──► cc-browser 拦截响应,工具数组尾部注入 request_handoff ──► claude
claude(跑到登录页): call request_handoff{reason} ──► cc-browser 截下(不转发 playwright-mcp)
   ├ bringToFront() + 还原窗口 + 通知
   ├ 进入"等待人工"态,启动接管计时器
   └ 用户完成,点"已完成" ──► 窗口移回屏外 ──► 返回 MCP 结果{handed_back:true} ──► claude 继续
```

**路径 B — 启发式(client 兜底):**
```
cc-browser 监听 page 事件(framenavigated / 出现 password 框 / 已知验证码特征)
   └ 命中 且 当前没在等待人工 且 claude 最近没调 request_handoff
       └ 自动 bringToFront + 通知 ──► 同样进入等待人工态
```

两路径**汇入同一"等待人工"态**:
- **该态阻塞 BrowserRpc 处理**:人接管期间 claude 的自动化帧被 hold(或返回"用户操作中"软错误),防人机同时操作同页打架。
- **启发式去抖**:路径 B 命中后若 claude 紧接着自调 `request_handoff`,不重复弹窗。
- **接管超时**:等待人工态也有超时,避免用户走开后通道永久卡住 → `BrowserClosed{handoff_timeout}`。

## 错误处理与边界

核心原则:**任何中断都让授权门回到 IDLE,grant 绝不跨断线自动恢复**(安全底线)。

| 失效场景 | 处置 | claude 可见结果 |
|---------|------|----------------|
| client 断线 | PTY session 靠 tmux 在 agent 存活;browser 通道立即拆,grant 撤销 | 在飞调用→错误;后续→"browser client offline";可继续非浏览器工作 |
| client 重连 | **永不自动恢复 grant**,下次浏览器操作重走 PENDING | 透明,再被 hold 一次 |
| playwright-mcp 子进程崩溃 | cc-browser 带退避重启(上限 N 次);Chrome 持久 profile 故状态可恢复 | 崩溃瞬间在飞调用→错误;重启后正常 |
| 用户手动关 Chrome 窗口 | 检测进程退出,同崩溃路径 | 同上 |
| client 没装 Node/Playwright | **能力协商**:client 连接上报 `browser_capable`;cc-mcp 端点据此决定 tools/list 是否暴露浏览器工具 | 无能力时浏览器工具不出现,claude 不会调注定失败的工具 |
| 同 workspace 被第二 client 接管 | 旧 client 的 browser 通道随连接关;新 client 重协商 + 重授权 | 接管瞬间→`BrowserClosed`;新 client 重走授权 |
| hub 重启 / agent 重连 | 通道随底层 ws 拆,grant 撤销 | 重连后重协商 |
| handoff 等待人工超时 | `BrowserClosed{handoff_timeout}` | "用户未在时限内接管" |

**关键安全性质**:grant 永不跨断线恢复 —— 断线意味着控制链断过,谁重连上来、是否同一人、中间发生过什么都不再可证,把"省一次确认"让位给"每条新控制链重新征得同意"。

**两个边界设计点**:
- **能力协商在 `tools/list` 层**:cc-mcp 端点收到 `tools/list` 时,若无 browser-capable client 绑定,从响应里**摘掉**浏览器工具(而非返回错误工具)。client 上/下线 → 主动推 `notifications/tools/list_changed`,claude 动态感知能力增减。
- **背压与有序**:MCP 靠 `id` 配对、通知单向流;ws 单连接天然保序。子进程慢时不无限缓冲,超水位 → 对最老在飞请求返回超时。

## 测试策略

原则:**把逻辑尽量挤进纯函数单元,让浏览器本身成为唯一不可测的薄边缘。**

| 层 | 测什么 | 怎么测 |
|----|--------|--------|
| 单元(Rust) | ① 授权门状态机 —— 表驱动覆盖每条转移(含**重连不恢复**) | 纯逻辑,喂事件序列断言状态 + 输出帧。TDD 先行 |
| | ② 新 `BrowserRpc`/`BrowserClosed` 变体 serde 往返 | 同现有 proto 测试模式 |
| | ③ cc-mcp 端点透传 + 路由 | 喂 HTTP/SSE 入帧 → 断言 `BrowserRpc` 原样;按 token 路由到对应 workspace;反向同理 |
| | ④ tools/list 能力过滤 | 无 capable client → 工具被摘;上线 → 断言推 list_changed |
| 集成 | ⑤ **端到端帧环路**(关键):agent→hub→client→子进程→回 | 用**假 playwright-mcp**(回显桩)替真 Chrome |
| | ⑥ 断线中途:在飞→错误 + grant 撤销 + 重连不恢复 | 同桩 + 模拟 ws 断开 |
| 手动冒烟(真浏览器) | ⑦ headless⇄headed handoff、bringToFront | 脚本驱已知登录页,本地跑,**不进 CI** |

**假 playwright-mcp 桩是支点**:我们的代码不关心对端是不是真浏览器(都是不透明 MCP 帧),用回显 MCP server 当替身,即可在 CI 里测全"授权门 + 路由 + 透传 + 断线恢复",把真浏览器隔离成手动冒烟点。这是"全透明透传 + playwright-mcp 当黑盒"在可测性上的回报。

**明确取舍**:不为 handoff 视觉切换在 CI 造脆弱断言,留在手动冒烟。

## 实现落点清单(供写计划用)

- `crates/agent/`:在守护进程内**内建常驻 MCP HTTP/SSE 端点**(127.0.0.1 + per-workspace token,非 per-session 子进程);启动 workspace 的 claude 时注入 workspace 专属 MCP 配置;`tunnel.rs` 加 `BrowserRpc`/`BrowserClosed` 变体;ws 路由接入。
- `crates/hub/`:`pty_proto.rs` 加 client↔hub 帧;relay 转发逻辑接 browser 帧;能力标志透传。
- `crates/client/`:`proto.rs` 镜像新帧;新增 cc-browser 模块(MCP 子进程管理 + 授权门 + handoff + 本地确认 UI);能力上报。
- **本地确认 UI**:走 client TUI 原生模态(复用 `menu.rs`/overlay 风格),叠加系统通知 + 终端响铃作唤起;handoff 唤起复用同一套。
- **`@playwright/mcp` 分发**:随 client `install.sh` 预装(连同 Node 依赖)。
- 配置:空闲超时、接管超时、子进程重启上限、MCP 端点端口等进配置。

## 已解决的设计选择(原未决项)

- **cc-mcp 端点形态**:不做 per-session 子命令,改为 agent 守护进程内的常驻 localhost HTTP/SSE MCP 端点 —— 一个实例服务所有 workspace,省去子进程↔守护进程 IPC 跳。
- **本地确认 UI**:client TUI 原生模态 + 系统通知/响铃。
- **`@playwright/mcp` 分发**:随 `install.sh` 预装。
