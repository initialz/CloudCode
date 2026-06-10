# Desktop App P1 — 端到端冒烟验证手册 (E2E Smoke)

> **目的 / Purpose:** 让 **用户在自己的 agent 环境里** 亲手验证 P1 验收标准:
> claude 的浏览器自动化全程在 agent 本地闭环 —— Chrome + playwright-mcp 都跑在
> agent 主机上,**不经过任何 cloudcode client / hub 投屏**。
>
> **验收(spec P1):** 现有 CLI/webterm 进 claude 让它开网页,全程不经 client;
> 重开 session 浏览器 profile 仍在。
>
> 自动化层的回归由 `crates/agent/src/browser/` 的单测 + 一个真实全栈 `#[ignore]`
> 集成测试 (`full_stack_real_chrome_and_playwright`) 覆盖;本文档是**人工在目标
> 环境**走一遍的手册。

---

## 0. 架构回顾(验证时心里要有这张图)

```
claude  ──localhost HTTP MCP──▶  mcp_endpoint (resident, 127.0.0.1:<mcp_port>)
  (在 pty 里)                         │  按 token→session 路由
                                       ▼
                          per-session @playwright/mcp 子进程 (stdio)
                                       │  --cdp-endpoint http://127.0.0.1:<cdp_port>
                                       ▼
                          常驻 headless Chrome (agent 主机, ONE 实例)
```

- 一个 agent 主机:**一个**常驻 Chrome,**每个 session 一个**独立 playwright-mcp 子进程。
- claude 连的是 agent 本地的 `127.0.0.1:<mcp_port>`,**不是** client。整条链路都在 agent 主机上。

---

## 1. 前置条件 (Prereqs)

在 **agent 主机** 上(不是你的本地客户端机器,如果它们不是同一台):

1. **Chrome / Chromium**
   - macOS:`/Applications/Google Chrome.app/...` 或 `/Applications/Chromium.app/...`(自动探测)。
   - Linux:`google-chrome` / `chromium` / `chromium-browser` 在 `$PATH`(自动探测),
     或显式配 `[browser].chrome_path`。
2. **node + npx**(playwright-mcp 用 `npx` 拉起)。`node -v && npx -v` 应有输出。
3. **预热 playwright-mcp 缓存**(首次 npx 冷启动会拉包,几十秒;先跑一次让后续秒起):
   ```bash
   npx -y @playwright/mcp@0.0.76 --version    # 期望输出: Version 0.0.76
   ```
4. **(走 example.com 这一跳需要)外网**。无外网也能验:见 §4 的 `data:` URL 降级。

---

## 2. 配置 (agent.toml)

在 agent 的 `agent.toml` 加:

```toml
[browser]
enabled = true        # 总开关,默认 false —— 不开这个,整套浏览器栈不启动
# chrome_path = "/path/to/chrome"   # 可选;不填则自动探测(见 §1)
# cdp_port = 19222                  # Chrome 的 remote-debugging 端口,默认 19222
# mcp_port = 7110                   # 常驻 MCP HTTP endpoint 端口,默认 7110(claude 连这里)
# mcp_command = "..."               # 逃生舱:整条覆盖 playwright-mcp 启动命令,通常不填
```

各 knob 含义:
- `enabled`(默认 `false`):不打开,main.rs 不起 Chrome、不 serve endpoint,pty 也不注入 `--mcp-config`。
- `chrome_path`(默认空 → 自动探测):显式指定 Chrome 二进制。路径不存在会回落到自动探测。
- `cdp_port`(默认 `19222`):Chrome 的 CDP 端口。playwright-mcp `--cdp-endpoint` 连这里。
- `mcp_port`(默认 `7110`):claude 连的本地 MCP HTTP 端口。**与 cdp_port 是两回事**。
- `mcp_command`(默认空):测试/逃生用,整条覆盖。生产留空,用内置启动器。

---

## 3. 启动 agent 并确认底座就绪

启动 agent(用你平时的方式)。在日志里(`RUST_LOG=info` 更清楚)应看到:

- Chrome 就绪:
  ```
  Chrome ready  cdp=http://127.0.0.1:19222  chrome=/Applications/Google Chrome.app/...
  ```
  (以及更早的 `spawned headless Chrome pid=...`)
- MCP endpoint 监听:
  ```
  browser MCP endpoint listening on 127.0.0.1  port=7110
  ```

健康探活(可选):
```bash
curl -s http://127.0.0.1:7110/healthz      # -> ok
curl -s http://127.0.0.1:19222/json/version | head -c 200   # Chrome 的 CDP 版本 JSON
```

---

## 4. 走一遍 claude(核心验收)

从 **现有 CLI 或 webterm** 打开一个 workspace,进入 `claude`:

1. **MCP 工具在不在**
   让 claude 列出可用 MCP 工具(例如直接问它 “list your MCP tools / 你有哪些 MCP 工具”,
   或用 `/mcp`)。期望:看到 **`cc-browser`** 这个 server,带 `browser_navigate` /
   `browser_snapshot` 等工具。
   - 注入来源:pty.rs 在 `browser.enabled` 时给 claude 注入
     `--mcp-config`(指向 `http://127.0.0.1:<mcp_port>/mcp/<token>`)。

2. **开网页 + 快照**
   让 claude:`打开 https://example.com 并对页面做一次 snapshot`。
   期望:
   - `browser_navigate` 成功,回显 `Page Title: Example Domain`。
   - `browser_snapshot` 成功,可读树里含 `heading "Example Domain"`。
   (无外网时改让它开:`data:text/html,<title>CcProbe</title><h1>CcProbe</h1>`,
    快照里应出现 `CcProbe`。)

3. **证明是 agent 本地、与 client 无关**(P1 的灵魂)
   这一步要**亲眼**确认浏览器跑在 agent 主机、是 cloudcode-agent 的子孙进程,
   而 **不依赖任何 cloudcode client 浏览器**:
   ```bash
   # 在 agent 主机上:
   pgrep -fl "remote-debugging-port=19222"        # 常驻 Chrome,带我们的 CDP 端口
   pgrep -fl "@playwright/mcp"                     # per-session playwright-mcp 子进程

   # 进程树:Chrome 与 playwright-mcp 都挂在 cloudcode-agent 之下
   #   (macOS 用 `ps -o pid,ppid,command`,Linux 用 `pstree -p <agent_pid>`)
   ps -o pid,ppid,command | grep -E "cloudcode-agent|playwright/mcp|remote-debugging-port"
   ```
   关键判据:
   - Chrome / playwright-mcp 的祖先进程是 **cloudcode-agent**,不是任何客户端进程。
   - 关掉本地客户端的浏览器(如果有)对自动化**毫无影响** —— 因为根本没用到。
   - 整条链路监听在 **127.0.0.1**(agent loopback),没有对外/对 client 的浏览器连接。

4. **profile 跨 session 持久**(P1 范围内的持久化判据)
   - 看 profile 目录:`<agent state dir>/agent/browser-profile/`。
     让 claude 开一个会写 cookie/localStorage 的页面(或就 example.com),然后:
   - **关闭并重开 workspace session**(退出 claude 再进,或重开 workspace)。
   - 重新进 claude,确认它仍能驱动浏览器,且 `browser-profile/` 目录**原地仍在**
     (同一个 `--user-data-dir`,常驻 Chrome 没换实例,profile 不丢)。
   - 说明:P1 没有“人看着浏览器手动登录”的投屏链路,**完整的登录态持久 / 人工接管
     验证留到 P4**;P1 这里只断言 **profile 目录跨 session 持续存在、常驻 Chrome 实例
     不随 session 起落而重建**,登录态因此天然落在同一 profile 里。

---

## 5. P1 已知范围 / 不在本次验收内 (Known scope)

- **没有投屏 / 截图给用户**:P1 浏览器是 agent 上的 **headless**,用户看不到画面,
  也没有“把浏览器画面镜像给 client”的链路 —— 那是 **P2+**。
- **一个 Chrome,多 session 共享**:整个 agent 一个常驻 Chrome 实例;**每个 session 独立
  一个 playwright-mcp 子进程**(互不串扰,但共享同一浏览器 profile)。
- **人工接管 / 登录投屏看不到**:P1 无 human-viewing。`request_handoff` 之类的人工接管
  工具尚未接线(endpoint 里预留了 600s 超时档位但 P1 不触发)。完整登录态持久化验证见 **P4**。
- **token 自愈**:agent 重启会从已写出的 `mcp-browser.json` 复用同一 token(`is_valid_token`
  32 位 hex 校验),claude 侧 MCP 配置不需要手动改。

---

## 6. 自动化回归对照(给维护者)

人工冒烟之外,代码层有:
- `crates/agent/src/browser/mcp_endpoint.rs` 单测:token 路由、JSON-RPC-error-at-200、
  超时分档、子进程死亡失败、token 校验/复用等(默认 `cargo test --workspace` 跑)。
- 全栈集成测试(默认 `#[ignore]`,需真 Chrome + npx playwright-mcp + 外网):
  ```bash
  cargo test -p cloudcode-agent full_stack_real_chrome -- --ignored --nocapture
  ```
  它把 §3+§4.2 这条链路(真 Chrome + 真 playwright-mcp + 真 axum endpoint,走 HTTP)
  全自动跑一遍:initialize → notifications/initialized → browser_navigate(example.com)
  → browser_snapshot,断言快照含 “Example”。无外网时自动回落 `data:` URL。
