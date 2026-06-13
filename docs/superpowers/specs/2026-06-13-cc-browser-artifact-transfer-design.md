# cc-browser:文件产物回传(计划②补丁) — 设计

> `cc-browser` 后端里 playwright-mcp 跑在 **client**,它写到 client 本地磁盘的文件产物(截图、PDF、下载、trace)只在 MCP 响应里回一个 client 本地路径;而 claude 跑在 **agent**,Read 工具在 agent 上解析路径,读不到。本补丁让 client 把这些产物经**现成 FsWrite 管道**镜像回 agent workspace,并把响应里的路径**重写成 agent 上可读的绝对路径**,使 claude 透明地"看见"产物。

- **日期**:2026-06-13
- **分支**:`feature/cc-browser-preset`(计划② 之上;基线 `feature/cc-browser` 计划①)
- **状态**:设计已与用户敲定,待写实现计划

## 背景与动机

这是 `cc-browser` 真机部署后用户发现的真实缺口。用户在已登录的真实站点上让远程 claude 查限行信息,claude 截图后说"快照文件保存在你本地机器上,这边读不到",转而用 `browser_evaluate` 跑 JS 提取文字才绕过去。

根因(本会话用真 playwright-mcp@0.0.76 + 真 claude CLI 实测确认):

1. **playwright-mcp 在 client 上,产物文件落在 client 磁盘。** `cc-browser` 后端经计划① 隧道由 **client** 端 `McpHost` 拉起 `npx @playwright/mcp`;它的截图/PDF/下载/trace 全写到 **client** 的 `--output-dir`,MCP 响应里只回 client 本地路径(markdown 链接)。
2. **claude 在 agent 上,Read 解析 agent 路径。** claude 的 Read 工具要求**绝对路径**且在 agent 文件系统上解析 → 拿到一个 client 本地路径时,要么文件不存在、要么读到 agent 上的同名错文件。
3. **截图的内联图片不可靠。** 实测 `browser_take_screenshot@0.0.76`:**不带 `filename` 参数**时响应是 `["text","image"]`(内联 base64 PNG ~22KB + 路径);**带 `filename`** 时只回 `["text"]`(纯路径,无内联图)。而真 claude 的自然习惯是**主动带 filename 存盘、再用 Read 去"真正看见"图片** —— 这恰好抑制了内联图、又把它送去读一个 client 本地路径。所以"靠内联图"在 `cc-browser` 下不可靠。
4. **纯文本/数据其实已经通。** `browser_snapshot`(a11y 树)、`browser_evaluate`(JS 提取)返回内联文本,经计划① 透明管道 claude 已能读到——用户案例最后用 JS 提取成功正因如此。**缺口只在「文件型产物」**:截图(claude 习惯存盘)、PDF、下载。

> 对照:`web` 后端 playwright-mcp 与 claude **同在 agent**,产物落在 agent 磁盘、claude 直接 Read 即可——本缺口是 `cc-browser`(跨机)独有。

## 目标 / 非目标

### 目标

| # | 目标 |
|---|------|
| 1 | **文件产物可读**:`cc-browser` 下 playwright-mcp 在 client 产生的文件(截图/PDF/下载/trace),claude 在 agent 上能用 Read 直接读到 |
| 2 | **透明**:claude 照常截图、照常 Read 返回的路径即可,不需要新工具、不需要知道"产物在另一台机器" |
| 3 | **复用现成基础设施**:用已有的 FsWrite 上传管道(`FsWriteInit`/`FsWriteChunk`/`FsWriteResult`),**零协议/帧改动**,agent/client 仍可独立升级(延续计划①②约束) |
| 4 | **顺序正确**:claude 收到 MCP 响应时,产物文件必已落在 agent 上,绝不会 Read 到尚未到达的文件 |
| 5 | **自动镜像全部产物(V1)**:每次 MCP 工具调用后,自动把本次新产生的产物全部回传,无需 claude 或用户显式触发 |

### 非目标(Non-Goals)

- **不碰 `web` 后端**:`web` 下产物与 claude 同在 agent,无需回传;代码路径不改。
- **不加新 MCP 帧/不做内联文件传输**:虽然"在 MCP 响应里内联文件字节让 agent 落盘"更优雅,但需新增帧类型 = 协议 bump,违背计划①②"agent/client 独立可升"约束。采用复用 FsWrite 的保守方案。
- **不做显式 fetch 工具**:不给 claude 加"拉取产物"工具(多一轮交互、破坏透明性)。
- **不做无大小上限的大文件回传**:超上限的产物不传(见下),避免拖垮隧道/relay 循环。
- **不做产物去重/缓存/增量**:每个新文件直传,V1 不优化重复传输。
- **下载(download)落点不在 V1 强行兜底**:见「开放问题 1」。

## 总体架构

### 数据流(以一次截图为例)

```
claude(agent)
  └─(1) tools/call browser_take_screenshot ──► agent mcp_proxy ──► hub ──► client McpHost ──► playwright-mcp
                                                                                                    │
                                                       (2) 写文件到 client <output-dir>/shot.png ◄──┘
                                                            响应含 client 本地路径
  client McpHost 在回送响应前:
       (3a) 响应驱动检测:扫响应里的 markdown 链接,basename 命中 staging 目录文件 = 本次产物
       (3b) [spawn 任务] 每个 ≤ 上限文件:读字节 ──FsWriteInit/FsWriteChunk──► hub ──► agent fs ──► 写入
                                                          agent workspace `.cloudcode/browser-artifacts/shot.png`
            ◄── 等 FsWriteResult 完成,拿回 final_name ──
       (3c) 重写响应:把该链接 target ──► `{{CC_WS}}/.cloudcode/browser-artifacts/<final_name>`
                       (超限/失败 ──► 提示文字,记日志)
  └─(4) [spawn 任务] 重写后的响应 ──► hub ──► agent mcp_proxy
       (5) agent mcp_proxy 按 token 查本会话工作区绝对路径,把 `{{CC_WS}}` ──► 该绝对路径
  └─(6) claude 收到响应,Read `/abs/workspace_root/account/workspace/.cloudcode/browser-artifacts/shot.png` ──► 文件在 agent 上 ──► 看见
```

### 职责切分(为什么是 client 传 + agent 落地)

- **client** 能读到产物文件,但**不知道** agent 的 workspace 绝对路径。
- **agent** 知道 workspace 绝对路径,但**读不到** client 的文件。

因此:**client 负责「上传 + 用相对名命名」,agent 负责「把占位符落地成绝对路径」**,两端之间用一个固定占位符 `{{CC_WS}}` 传递。client 端做不到正确的绝对路径重写,agent 端拿不到文件字节——这个切分是物理约束决定的,不是偏好。

## 组件设计

### 1. client `McpHost`(`crates/client/src/mcp_host.rs`)

**1a. 统一产物落点(CWD == output-dir)。** `cc-browser` 默认后端 **spawn 时的工作目录(CWD)** 与 `--output-dir` **都设成同一个已知 staging 目录**(client state_dir 下 `browser-output/`,0700)。关键原因(实测确认):playwright-mcp 对**显式 `filename`** 参数(真 claude 的习惯)按 **CWD** 相对落盘(`./shot.png`),而非 `--output-dir`;只有不带 filename 时才落 `--output-dir`。两者都钉到同一 staging 目录后,所有截图/PDF 都可预测地落在那一个目录里。`web` 后端不走此路径,不改。

**1b. 响应驱动检测(不做目录 diff)。** 不做调用前后快照 diff(有并发/竞态边界)。改为**从响应文本里找 markdown 链接** `](<target>)`:对每个 target 取 basename,若 `staging/<basename>` 存在,即本次调用产生、且 **claude 即将去 Read** 的产物。这把"传什么"直接绑定到"claude 会看到的路径",天然跳过仅内联返回的冗余产物(如 a11y `.yml` 不在链接里就不传),也无需 seen-set/mtime 跟踪。

**1c. 经 FsWrite 上传。** 对每个 ≤ 上限的产物:读字节,经 client 出站通道发 `FsWriteInit{ path: ".cloudcode/browser-artifacts/<basename>", ... }` + 若干 `FsWriteChunk{ data_b64, eof }`,**await 对应 `FsWriteResult`**,拿回 `final_name`(agent 侧 `dedupe_path` 可能把重名改成 `x (1).png`,故重写必须用 `final_name` 而非原 basename)。复用现成的 `spawn_upload`/`upload_one_file` 机制(`pending_uploads` 路由 `FsWriteResult`),**在 spawn 出去的任务里跑、不在 relay select! 循环里 await**——否则等 `FsWriteResult` 会和路由它的 text 臂死锁。

**1d. 重写响应路径 + 顺序。** 转移任务里把响应中**每个产物的 markdown 链接 target**(检测时已拿到精确原串)替换成 `{{CC_WS}}/.cloudcode/browser-artifacts/<final_name>`;超上限或上传失败者替换成 `[browser artifact not transferred: <basename> (<size>); generated on client only]` 并记 warn(**不静默**)。**重写完成后**才发出 `RemoteMcp` 响应——claude 收到响应时文件必已落在 agent 上。无产物的响应仍按原路内联发出(零额外延迟)。多响应乱序无碍:agent proxy 按 JSON-RPC id 配对。

### 2. agent `mcp_proxy`(`crates/agent/src/mcp_proxy.rs`)

**2a. 占位符落地(按 token 取本会话工作区绝对路径)。** 在 POST handler 把 MCP 响应交给 claude 前,对响应文本字面替换 `{{CC_WS}}` → **本会话工作区绝对路径**。注意:claude 的 cwd / fs `resolve_safe` 的 base 是 `workspace_root/account/workspace`(**每会话不同**),不是全局 `workspace_root`。该绝对路径在 `pty.rs` 铸 token 时已知(就是 claude 的 `cwd`)。因此扩展 `McpProxy::register` 让它在 `token → session_id` 之外也存 `token → workspace_abs_path`;POST handler 手上有 `token`,据此查表替换。`web` 后端的响应不含 `{{CC_WS}}`,替换是 no-op。

**2b. 仅此一处变换。** proxy 其余仍是透明转发;`{{CC_WS}}` 是受控、唯一、不会与真实页面内容碰撞的标记(实现时若担心碰撞,可用更不可能出现的前缀如 `\u{1}CC_WS\u{1}`,在 spec 落实现时定;原则:唯一、可一次性 find/replace)。

### 3. agent `fs`(`crates/agent/src/fs.rs`)

无需改动:`FsWriteInit`/`FsWriteChunk` 经 `resolve_safe` 写入 workspace 相对路径已支持。`.cloudcode/browser-artifacts/` 由现有写入路径自动创建(与 `.cloudcode/uploads/` 同机制)。

### 4. 配置

- **大小上限**:默认 10 MB(常量,实现时定;截图 ~16KB、典型 PDF 数百 KB 均远低于此)。是否暴露成 `[browser]` 配置项 V1 不做(YAGNI),先内置常量。
- **产物目录生命周期**:agent 侧 `.cloudcode/browser-artifacts/` 跟随 workspace,reset/delete 时随 workspace 清理(与 `.cloudcode/uploads/` 一致);V1 不单独做容量回收。

## 错误处理

| 情况 | 处理 |
|------|------|
| 文件超大小上限 | 不传;响应路径改写成提示文字;记 warn。claude 看到提示而非死链。 |
| FsWrite 上传失败(隧道断/agent 拒) | 不阻塞响应;该文件路径改写成提示文字(注明传输失败);记 warn。其余产物正常。 |
| 调用未产生新文件 | 跳过 3b-3d,响应原样转发。 |
| 同名文件覆盖 | FsWrite 写入按 basename,后写覆盖先写(与 uploads 行为一致);V1 接受。 |
| 检测到并发/串扰的多余文件 | 按 mtime + 调用前未见 过滤;残余边界 V1 记日志、接受。 |
| `{{CC_WS}}` 占位符在真实页面内容里意外出现 | 选唯一性足够高的标记规避;视为不可能,不额外防御。 |

## 测试策略

- **client 单元测试**:
  - 响应驱动检测纯函数(给定真实 playwright-mcp 截图响应文本 + staging 文件名集合,返回 `[(链接 target 原串, basename)]`;覆盖 `./shot.png`、`page-<ts>.png`、无链接、链接指向不存在文件)。
  - 响应路径重写纯函数(给定响应文本 + `[(原 target, 新路径)]`,返回重写后文本;覆盖单/多文件、超限提示替换)。
  - 大小上限边界(等于/超过上限的处置)。
- **agent 单元测试**:`{{CC_WS}}` 落地替换纯函数(给定响应 + 工作区绝对路径,返回绝对路径响应;覆盖多次出现、无出现/no-op)。
- **集成测试(node+chromium 门控,延续计划②已有 `host_roundtrips_via_real_playwright_mcp`)**:McpHost 驱动真 playwright-mcp 截图 → 断言产物经 FsWrite(mock/真 agent fs)落到目标目录、响应路径被重写。
- **真机验证(用户清单)**:`cc-browser` 后端真截图 → claude 在 agent 端 Read 成功看见图;真站点下载/PDF → 回传成功。

## 开放问题

1. **下载文件与非 markdown-link 产物**:V1 检测只认响应里的 markdown 链接(截图、`browser_pdf_save` 均是此形)。若某产物(如浏览器下载文件)不以 markdown 链接形式出现在响应里,V1 不转移——作为后续项(spec 标注,不强行兜底)。实现时实测确认 `browser_pdf_save` 的响应形态。
2. **relay 循环不阻塞(已解决)**:转移在 spawn 出去的任务里跑(复用 `spawn_upload` 模式),select! 循环继续处理 text 臂、路由 `FsWriteResult`——不会死锁、不冻结终端。产物响应的发出被推迟到上传完成(正是要的顺序保证),但不占用 relay 循环。
3. **内联图一并保留**:不带 filename 时 playwright-mcp 返回内联图。本方案不剥离 filename、也不依赖内联图;内联图若出现则原样透传(claude 双重可见),不冲突。
4. **`.playwright-mcp/` a11y 快照**:把后端 CWD 设到 staging 目录后,playwright-mcp 可能在此写 a11y `.yml`。因检测是响应驱动(只传被 markdown 链接引用且存在于 staging 的文件),未被链接引用的 `.yml` 不会被传,无噪音。
