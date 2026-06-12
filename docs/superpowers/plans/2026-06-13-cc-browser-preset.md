# cc-browser 浏览器预设(计划②/共二) 实现计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 在计划①已验证的远程-MCP 管道上叠加浏览器预设:claude 默认用 agent 本机无头浏览器(`web`,stdio 直 spawn),用户明示才用 client 本地有头浏览器(`cc-browser`,走①隧道 + 持久 profile);同时撤掉 client 默认后端的 echo 验证桩。

**Architecture:** agent 侧把注入配置从单 server 扩成双 server——新增 `web`(`type:"stdio"`,claude 直接 spawn `npx -y @playwright/mcp@0.0.76 --headless`,全程不经隧道)+ 原有 `cc-browser`(`type:"http"`,字节语义与①一致);client 侧 `backend_command` 的默认后端从内置 echo 桩换成 `npx -y @playwright/mcp@0.0.76 --user-data-dir=<持久 profile>`(headed,登录态持久);引导 prompt 换成双后端选择文案(默认 `web`、明示才 `cc-browser`、撞墙不自切)。计划①的 proxy/隧道/McpHost/超时/降级零改动,无协议/帧改动。

**Tech Stack:** Rust / @playwright/mcp(node)/ serde(toml 配置)/ 计划① 远程-MCP 管道

---

## 决策记录

| # | 决策 | 依据 / 说明 |
|---|------|-------------|
| P1 | **client `backend_command` 改签名**:`pub fn backend_command(cfg: &BrowserConfig) -> Option<(String, Vec<String>)>`;内部委托纯函数 `backend_command_from(env_backend: Option<&str>, cfg: &BrowserConfig)` 便于无 env 污染的单测。优先级:env `CC_REMOTE_MCP_BACKEND`(操作员显式覆盖,优先于一切、含 `enabled=false`)> `[browser].backend` > 内置默认。调用点全部更新:`relay.rs::run/relay_loop`(新形参 `browser`)、`wire.rs::connect`(新形参 `browser`,`Hello.remote_mcp_capable = backend_command(browser).is_some()`)、`main.rs`(两处 `wire::connect`、一处 `relay::run` 都从 `cfg: &ClientConfig` 取 `&cfg.browser`)。 | spec 组件 3;`ClientConfig` 在 `main.rs` 的 `run_chat`/`session_loop`/`reconnect_wire` 全程在手,传参链路最短。 |
| P2 | **`mcp_config_json` 改签名**:`pub fn mcp_config_json(port: u16, token: &str, web: Option<(&str, &[String])>) -> String`。`web = Some((program, args))` 时多一个 stdio `web` 条目;`None`(agent `web_enabled=false` 或 web 命令解析失败)退回单 cc-browser(=计划①形态)。实现从 `format!` 换成 `serde_json::json!` 构造(args 含任意路径/参数需 JSON 转义;serde_json 输出确定性,同输入同字节,D12 的"字节稳定"以新格式为新基线)。 | spec 组件 1。 |
| P3 | **`PtyManager` 加 `browser: crate::config::BrowserConfig` 字段** + `PtyManager::new` 第 9 形参(已有 `#[allow(clippy::too_many_arguments)]`);`main.rs` 接线 `config.browser.clone()`(平行于 `remote_mcp`)。注入块用纯函数 `mcp_proxy::web_backend_command(&self.browser)` 算出 `Option<(String, Vec<String>)>` 再借用成 `Option<(&str, &[String])>` 传给 `mcp_config_json`。 | spec 组件 1/2/5。 |
| P4 | **client `BrowserConfig` 放 `crates/client/src/mcp_host.rs`**(消费者所在模块);`main.rs::ClientConfig` 加 `#[serde(default)] browser: BrowserConfig` 字段。字段:`enabled: bool`(默认 true)、`backend: Option<String>`(整条命令覆盖)、`profile_dir: Option<PathBuf>`(默认 = `state_dir()/browser-profile`,即通常 `~/.local/state/cloudcode/browser-profile`;`main.rs::state_dir` 改 `pub(crate)` 复用,DRY)。默认路径不可得(无 home)→ `backend_command` 返回 None。profile 目录由 `backend_command` 创建并收紧 0700(best-effort)。 | spec 组件 5;复用既有 `state_dir()`(尊重 `CLOUDCODE_STATE_DIR`/`XDG_STATE_HOME`)。 |
| P5 | **agent `BrowserConfig` 放 `crates/agent/src/config.rs`**,新 `[browser]` 段(与既有 `[remote_mcp]` 平行、互不相干):`web_enabled: bool`(默认 true)、`web_backend: Option<String>`(整条命令覆盖)。`Config` 加 `#[serde(default)] pub browser: BrowserConfig`。 | spec 组件 5;`[remote_mcp]` 继续只管 proxy 的 enabled/port/tools_manifest。 |
| P6 | **pin 版本常量**:agent/client 是两个互不依赖的 bin crate(workspace 无共享 lib crate),按仓内 `CC_BROWSER_SERVER` 先例**各放一枚、手工 lockstep 注释互指**:`crates/agent/src/mcp_proxy.rs::PLAYWRIGHT_MCP_PKG` 与 `crates/client/src/mcp_host.rs::PLAYWRIGHT_MCP_PKG`,值均为 `"@playwright/mcp@0.0.76"`。各自只在默认命令构造处引用,绝不在别处散落字面量。 | spec 开放问题 4 的"单点存放"在仓库现实(无共享 crate)下的最近解。 |
| P7 | **撤 echo 桩**:删除 `crates/client/src/mcp_host.rs::EMBEDDED_ECHO_MCP`(`include_str!`)与 `embedded_echo_backend()`(撤销 `ee92506` 的生产路径);**保留** `test-fixtures/echo-mcp.mjs` 文件本体(集成测试夹具)。仓内实情:计划①的端到端/集成测试(`mcp_proxy.rs` loopback、`mcp_host.rs` 各测试)都**直接以 `node <fixture 路径>` 构造后端**,不经过 `backend_command()` 的 echo 回落,故无测试需要迁移;Task 6 以 grep + 全仓测试证实。 | spec 组件 3 / 复用与不复用表。 |
| P8 | **新 server 名常量**:`crates/agent/src/mcp_proxy.rs::WEB_SERVER = "web"`。`web` 不走帧、不进 client,无需 client 侧 lockstep。`extract_token_from_config` 不动(按 `mcpServers.cc-browser.url` 取 token,新增 `web` 键天然无影响,Task 3 加回归测试)。 | spec 组件 1 兼容性。 |
| P9 | **引导 prompt**:`GUIDANCE_PROMPT` 整体替换为 spec「选择机制与引导 prompt」一节的完整英文文案(默认恒 `web` / 明示才 `cc-browser` / 撞墙不自切先问 / 任务级粘住一个 / 状态不互通 / not-connected 转达)。注入机制(`claude_mcp_args` 的 `--append-system-prompt`)不变。 | spec 组件 4。 |
| P10 | **接管工具 V1 不做**:`-32003` 码位与 `LONG_CALL_TOOLS = ["request_handoff"]` 原样闲置保留,本计划零触碰。撞登录墙 = 纯对话协调(prompt 规则 3+5)。 | spec 非目标。 |
| P11 | **无协议/帧改动**:`web` 不走隧道,`cc-browser` 帧面与①逐字节一致;`PROTOCOL_VERSION`/`PTY_PROTOCOL_VERSION` 都不 bump。旧 client + 新 agent = `web` 可用、`cc-browser` 走 `-32004` 降级;新 client + 旧 agent = 行为同计划①。 | spec「发布」。 |
| P12 | **npx 预热与 vendoring 本计划不实现**:首跑慢由计划①分层超时兜底(`tools/call` 120s 档);Task 7 真机量化首拉耗时后把结论回填 spec 开放问题 2/3,再决定是否追加预热任务。 | spec 组件 6 把预热列为"推荐项"而非必做;避免在未量化前镀金。 |
| P13 | **两条硬约束**(两机两 profile 不迁移、后端选择任务级)不落新代码,落在引导 prompt 文案(规则 4,P9 已含)+ 本文档 + Task 4 测试断言三处。 | spec「持久登录与用户接管」。 |

## File Structure

```
crates/client/src/mcp_host.rs        修改  +PLAYWRIGHT_MCP_PKG 常量、+BrowserConfig(enabled/backend/profile_dir)、
                                           +default_profile_dir/ensure_profile_dir、backend_command 改签名
                                           (env > 配置 > 内置 playwright-mcp 默认)、删 EMBEDDED_ECHO_MCP +
                                           embedded_echo_backend;+单测(优先级/默认命令/enabled=false/不再回落 echo/toml 缺省)
crates/client/src/main.rs            修改  ClientConfig +browser 字段(serde default)、resolve_config 透传、
                                           state_dir 改 pub(crate)、--init 模板加 [browser] 注释段、
                                           wire::connect ×2 与 relay::run ×1 调用点加 &cfg.browser;+ClientConfig 解析单测
crates/client/src/wire.rs            修改  connect 加 browser 形参;Hello.remote_mcp_capable 用 backend_command(browser)
crates/client/src/relay.rs           修改  run/relay_loop 加 browser 形参;backend_command(browser);
                                           防御性错误文案提及 [browser] 配置
crates/agent/src/config.rs           修改  +BrowserConfig(web_enabled/web_backend)+ Config.browser 字段;+缺省/覆盖单测
crates/agent/src/mcp_proxy.rs        修改  +WEB_SERVER/+PLAYWRIGHT_MCP_PKG 常量、mcp_config_json 改签名(双 server)、
                                           +web_backend_command + parse_command、GUIDANCE_PROMPT 换双后端文案;
                                           更新/新增单测(双 server JSON、token 回采兼容、prompt 措辞)
crates/agent/src/pty.rs              修改  PtyManager +browser 字段、new +形参、注入块算 web 传 mcp_config_json
crates/agent/src/main.rs             修改  PtyManager::new 传 config.browser.clone();--init 模板加 [browser] 注释段
README.md                            修改  Quick start 补 node >= 18 要求一句(浏览器预设依赖)
test-fixtures/echo-mcp.mjs           保留  集成测试夹具(不删)
docs/superpowers/plans/2026-06-13-cc-browser-preset.md   创建  本计划
```

### Task 1: client `[browser]` 配置 + backend_command 换真默认 + 撤 echo 桩

**Files:**
- Modify: `crates/client/src/mcp_host.rs`(L24-36 删 `EMBEDDED_ECHO_MCP`/`embedded_echo_backend`;L38-49 改 `backend_command`;文件头新增常量与 `BrowserConfig`)
- Modify: `crates/client/src/main.rs`(L91-95 `ClientConfig`;L128-145 `resolve_config`;L147-156 `state_dir` 改 `pub(crate)`;L292-299 `--init` 模板;L320 与 L502 `wire::connect` 调用点;L393-401 `session_loop` 签名透传;L433 `relay::run` 调用点)
- Modify: `crates/client/src/wire.rs`(L37 `connect` 签名;L44-49 `Hello` 构造)
- Modify: `crates/client/src/relay.rs`(L110-117 `run`;L119-124 `relay_loop` 签名;L158-160 `backend_command` 调用;L264-274 防御文案)
- Test: `crates/client/src/mcp_host.rs` 模块内 `#[cfg(test)]`(D15:bin crate 无 lib target,测试只能放模块内)+ `crates/client/src/main.rs` 模块内 `#[cfg(test)]`

**步骤(TDD):**

- [ ] **1.1 写失败测试**。在 `crates/client/src/mcp_host.rs` 末尾 `mod tests` 内(`parse_backend_splits_program_and_args` 测试之后)追加:

```rust
    #[test]
    fn browser_config_defaults_from_empty_toml() {
        // 整段缺省 = 全默认零配置(spec 组件 5)。
        let c: BrowserConfig = toml::from_str("").unwrap();
        assert!(c.enabled);
        assert_eq!(c.backend, None);
        assert_eq!(c.profile_dir, None);
        let d = BrowserConfig::default();
        assert!(d.enabled);
        assert_eq!(d.backend, None);
        assert_eq!(d.profile_dir, None);
    }

    #[test]
    fn browser_config_explicit_overrides_parse() {
        let c: BrowserConfig = toml::from_str(
            "enabled = false\nbackend = \"node /tmp/x.mjs\"\nprofile_dir = \"/custom/profile\"",
        )
        .unwrap();
        assert!(!c.enabled);
        assert_eq!(c.backend.as_deref(), Some("node /tmp/x.mjs"));
        assert_eq!(c.profile_dir, Some(std::path::PathBuf::from("/custom/profile")));
    }

    #[test]
    fn backend_env_var_wins_over_everything() {
        // env 是操作员显式覆盖:优先于 [browser].backend,甚至优先于
        // enabled=false(测试/排障时用 env 强行指后端)。纯函数测试,
        // 不碰真实进程环境(并行测试安全)。
        let cfg = BrowserConfig {
            enabled: false,
            backend: Some("node /tmp/other.mjs".to_string()),
            profile_dir: None,
        };
        assert_eq!(
            backend_command_from(Some("node /tmp/echo.mjs"), &cfg),
            Some(("node".to_string(), vec!["/tmp/echo.mjs".to_string()]))
        );
        // env 是空白串 → parse_backend 给 None(显式配置成"没有后端")。
        assert_eq!(backend_command_from(Some("   "), &cfg), None);
    }

    #[test]
    fn backend_config_wins_over_builtin_default() {
        let cfg = BrowserConfig {
            enabled: true,
            backend: Some("npx -y @playwright/mcp@0.0.76 --user-data-dir=/p --browser=firefox".to_string()),
            profile_dir: None,
        };
        let (prog, args) = backend_command_from(None, &cfg).expect("explicit backend");
        assert_eq!(prog, "npx");
        assert!(args.contains(&"--browser=firefox".to_string()));
    }

    #[test]
    fn backend_default_is_pinned_playwright_with_user_data_dir() {
        // 内置默认:npx -y @playwright/mcp@<pin> --user-data-dir=<profile>。
        // 用显式 profile_dir 保持纯函数(default_profile_dir 依赖 HOME)。
        let dir = tempfile::tempdir().unwrap();
        let cfg = BrowserConfig {
            enabled: true,
            backend: None,
            profile_dir: Some(dir.path().join("prof")),
        };
        let (prog, args) = backend_command_from(None, &cfg).expect("builtin default");
        assert_eq!(prog, "npx");
        assert_eq!(args[0], "-y");
        assert_eq!(args[1], PLAYWRIGHT_MCP_PKG);
        assert_eq!(PLAYWRIGHT_MCP_PKG, "@playwright/mcp@0.0.76", "pin 版本单点");
        let uddir = format!("--user-data-dir={}", dir.path().join("prof").display());
        assert_eq!(args[2], uddir);
        assert_eq!(args.len(), 3, "默认命令不得夹带其他参数");
        // 不再回落 echo 桩(撤销 ee92506 生产路径)。
        assert!(!args.iter().any(|a| a.contains("echo")), "echo stub must be gone: {args:?}");
        // profile 目录被创建且(unix)收紧 0700。
        assert!(dir.path().join("prof").is_dir(), "profile dir must be created");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(dir.path().join("prof")).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o700, "profile dir must be 0700");
        }
    }

    #[test]
    fn backend_disabled_yields_none() {
        // enabled=false ⇒ None ⇒ Hello 能力位 false,本机不提供 cc-browser。
        let cfg = BrowserConfig {
            enabled: false,
            backend: None,
            profile_dir: Some(std::path::PathBuf::from("/unused")),
        };
        assert_eq!(backend_command_from(None, &cfg), None);
    }
```

- [ ] **1.2 确认失败**:

```bash
cd /Users/vtech/cloudcode-agent/workspaces/petez/cloudcode_dev/cloudcode
cargo test -p cloudcode-client backend 2>&1 | tail -20
```

预期:**编译失败**,`error[E0422]: cannot find struct, variant or union type `BrowserConfig``、`error[E0425]: cannot find function `backend_command_from``、`cannot find value `PLAYWRIGHT_MCP_PKG``。

- [ ] **1.3 最小实现(mcp_host.rs)**。把 `crates/client/src/mcp_host.rs` L24-49(`EMBEDDED_ECHO_MCP` 常量、`embedded_echo_backend()`、旧 `backend_command()` 三块,即从 `/// 验证脚手架(计划①)…` 注释起到旧 `backend_command` 的收尾 `}` 止)整体替换为:

```rust
/// 两后端共用的 playwright-mcp pin 版本(spec 组件 6:pin 死保证两台
/// 机器、多次 npx 拉取行为一致)。与 agent 侧
/// `crates/agent/src/mcp_proxy.rs::PLAYWRIGHT_MCP_PKG` 手工 lockstep;
/// 升级须两处同改并重跑双后端冒烟。
pub const PLAYWRIGHT_MCP_PKG: &str = "@playwright/mcp@0.0.76";

/// client 侧 `[browser]` 配置段(计划②,spec 组件 5)。整段缺省 =
/// 全默认零配置:enabled + 内置 playwright-mcp headed 命令 + 持久
/// profile 在 state 目录下。
#[derive(Debug, Clone, serde::Deserialize)]
pub struct BrowserConfig {
    /// 本机是否提供 cc-browser 能力。false ⇒ backend_command() 返回
    /// None ⇒ Hello 能力位 false(env 覆盖除外)。
    #[serde(default = "default_browser_enabled")]
    pub enabled: bool,
    /// 整条后端命令覆盖(空白分隔)。缺省 = 内置默认
    /// `npx -y @playwright/mcp@<pin> --user-data-dir=<profile_dir>`。
    #[serde(default)]
    pub backend: Option<String>,
    /// 持久 profile 路径(拼进默认命令的 --user-data-dir)。缺省 =
    /// state 目录下 browser-profile 子目录。仅 backend 缺省时生效
    /// (显式 backend 全权自带参数)。
    #[serde(default)]
    pub profile_dir: Option<std::path::PathBuf>,
}

fn default_browser_enabled() -> bool {
    true
}

impl Default for BrowserConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            backend: None,
            profile_dir: None,
        }
    }
}

/// 缺省持久 profile 路径:state 目录(尊重 CLOUDCODE_STATE_DIR /
/// XDG_STATE_HOME,通常 ~/.local/state/cloudcode)下的 browser-profile。
/// 定不出 state 目录(无 home)→ None,调用方按「无后端」处理。
fn default_profile_dir() -> Option<std::path::PathBuf> {
    crate::state_dir().ok().map(|d| d.join("browser-profile"))
}

/// 创建 profile 目录并(unix)收紧 0700:登录态(cookie/会话)落在
/// 这里,不给同机其他用户可读窗口。best-effort —— 失败不阻断命令
/// 构造,playwright-mcp 自己也会建目录。
fn ensure_profile_dir(dir: &std::path::Path) {
    let _ = std::fs::create_dir_all(dir);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700));
    }
}

/// 解析后端命令(计划②,决策 P1):env `CC_REMOTE_MCP_BACKEND`
/// (操作员显式覆盖,优先于一切、含 enabled=false)> `[browser].backend`
/// > 内置默认(pin 的 playwright-mcp,headed,持久 profile)。
/// None = 本机不提供远程-MCP 能力(Hello 能力位 false)。
pub fn backend_command(cfg: &BrowserConfig) -> Option<(String, Vec<String>)> {
    backend_command_from(
        std::env::var("CC_REMOTE_MCP_BACKEND").ok().as_deref(),
        cfg,
    )
}

/// `backend_command` 的纯函数内核(env 注入为形参,单测不碰进程环境)。
fn backend_command_from(
    env_backend: Option<&str>,
    cfg: &BrowserConfig,
) -> Option<(String, Vec<String>)> {
    if let Some(cmd) = env_backend {
        return parse_backend(cmd);
    }
    if !cfg.enabled {
        return None;
    }
    if let Some(cmd) = &cfg.backend {
        return parse_backend(cmd);
    }
    let profile = cfg.profile_dir.clone().or_else(default_profile_dir)?;
    ensure_profile_dir(&profile);
    Some((
        "npx".to_string(),
        vec![
            "-y".to_string(),
            PLAYWRIGHT_MCP_PKG.to_string(),
            format!("--user-data-dir={}", profile.display()),
        ],
    ))
}
```

并在 `crates/client/Cargo.toml` 末尾新增(client 现无 `[dev-dependencies]` 段;agent 已有同款先例):

```toml
[dev-dependencies]
tempfile = "3"
```

`[dependencies]` 已有 `toml`/`serde`,无需动。

- [ ] **1.4 编译推进到调用点**。跑 `cargo test -p cloudcode-client backend 2>&1 | tail -20`,预期新错误转移到调用点:`wire.rs:48` 与 `relay.rs:159` 的 `backend_command()` 报 `error[E0061]: this function takes 1 argument but 0 arguments were supplied`,以及 `state_dir` 私有可见性错误 `error[E0603]`。逐一修:

  ① `crates/client/src/main.rs` L147,`state_dir` 改 crate 可见:

```rust
pub(crate) fn state_dir() -> Result<PathBuf> {
```

  ② `crates/client/src/main.rs` L91-95,`ClientConfig` 加字段:

```rust
#[derive(serde::Deserialize, Debug)]
struct ClientConfig {
    hub_url: String,
    token: String,
    /// `[browser]` 段:本地 cc-browser 后端(计划②)。缺省 = 全默认。
    #[serde(default)]
    browser: crate::mcp_host::BrowserConfig,
}
```

  ③ `crates/client/src/main.rs` L128-145,`resolve_config` 末尾改为(`hub_url`/`token` 两段不动,只改返回):

```rust
    let browser = file.map(|c| c.browser).unwrap_or_default();
    Ok(ClientConfig {
        hub_url,
        token,
        browser,
    })
```

  ④ `crates/client/src/wire.rs` L37 与 L44-49,`connect` 加形参并用之:

```rust
pub async fn connect(
    hub_url: &str,
    token: &str,
    browser: &crate::mcp_host::BrowserConfig,
) -> Result<Wire> {
```

```rust
    let hello = ClientToHub::Hello {
        token: token.to_string(),
        version: PTY_PROTOCOL_VERSION.into(),
        // 解析得出后端命令 = 本机能承载远程-MCP 后端(决策 D9→P1:
        // 内置默认存在后,[browser].enabled=true 时恒为 true)。
        remote_mcp_capable: crate::mcp_host::backend_command(browser).is_some(),
    };
```

  ⑤ `crates/client/src/relay.rs` L110-124,`run`/`relay_loop` 加形参:

```rust
pub async fn run(
    wire: &mut Wire,
    bytes: &mut ByteRx,
    agent: &str,
    workspace: &str,
    browser: &crate::mcp_host::BrowserConfig,
) -> Result<RelayOutcome> {
    relay_loop(wire, bytes, agent, workspace, browser).await
}

async fn relay_loop(
    wire: &mut Wire,
    bytes: &mut ByteRx,
    agent: &str,
    workspace: &str,
    browser: &crate::mcp_host::BrowserConfig,
) -> Result<RelayOutcome> {
```

  ⑥ `crates/client/src/relay.rs` L153-160,宿主构造注释与调用更新:

```rust
    // 远程-MCP 宿主(Phase C)。后端命令:env CC_REMOTE_MCP_BACKEND >
    // [browser].backend > 内置 playwright-mcp 默认(决策 P1);None →
    // Hello 能力位为 false,hub/agent 不会给我们发 RemoteMcp 帧 ——
    // 万一异常发来,走下方防御性快速失败臂。
    // 注意:host_out_tx 在本作用域常驻(host 内只持 clone),保证
    // host_out_rx.recv() 永不返回 None 而空转。
    let (host_out_tx, mut host_out_rx) = tokio::sync::mpsc::channel::<String>(64);
    let mut mcp_host: Option<crate::mcp_host::McpHost> = crate::mcp_host::backend_command(browser)
        .map(|b| crate::mcp_host::McpHost::new(b, host_out_tx.clone()));
```

  ⑦ `crates/client/src/relay.rs` L264-274,防御文案不再只点名 env:

```rust
                        } else {
                            // 能力位为 false 仍收到帧:防御性快速失败。
                            let _ = wire
                                .out_tx
                                .send(OutFrame::Text(ClientToHub::RemoteMcpClosed {
                                    server,
                                    reason: Some(
                                        "no MCP backend configured (check [browser] in \
                                         config.toml or CC_REMOTE_MCP_BACKEND)"
                                            .to_string(),
                                    ),
                                }))
                                .await;
                        }
```

  ⑧ `crates/client/src/main.rs` 三个调用点:L320 改 `let mut wire = wire::connect(&cfg.hub_url, &cfg.token, &cfg.browser).await?;`;L502(`reconnect_wire` 内)改 `let new_wire = match wire::connect(&cfg.hub_url, &cfg.token, &cfg.browser).await {`;L433 改 `match relay::run(wire, bytes, agent, workspace, &cfg.browser).await? {`。

- [ ] **1.5 `--init` 模板补 `[browser]`**。`crates/client/src/main.rs` L292-299 的 `template` 改为:

```rust
    let template = r#"# Cloudcode client config.
# - hub_url: where the hub is reachable (http(s)://…).
# - token:   account token printed once by `cloudcode-hub gen-token <name>`
#            on the admin's side; ask them for it.

hub_url = "http://localhost:7100"
token   = "cc_PASTE_TOKEN_HERE"

# [browser] controls the local cc-browser backend: a VISIBLE browser
# window on this machine that the remote claude can drive when you
# explicitly ask for it. Defaults: enabled, pinned playwright-mcp via
# npx (requires node >= 18 here), persistent login profile under the
# cloudcode state dir. Uncomment to override.
# [browser]
# enabled     = true
# backend     = "npx -y @playwright/mcp@0.0.76 --user-data-dir=/custom/profile --browser=firefox"
# profile_dir = "/custom/profile"
"#;
```

- [ ] **1.6 加 `ClientConfig` 解析单测**。`crates/client/src/main.rs` 文件末尾追加:

```rust
#[cfg(test)]
mod client_config_tests {
    use super::*;

    #[test]
    fn client_config_browser_section_defaults_and_overrides() {
        // 无 [browser] 段:serde default 给全默认。
        let c: ClientConfig =
            toml::from_str("hub_url = \"http://h\"\ntoken = \"t\"").unwrap();
        assert!(c.browser.enabled);
        assert_eq!(c.browser.backend, None);
        assert_eq!(c.browser.profile_dir, None);

        // 显式 [browser] 段。
        let c: ClientConfig = toml::from_str(
            "hub_url = \"http://h\"\ntoken = \"t\"\n\n[browser]\nenabled = false\nprofile_dir = \"/p\"",
        )
        .unwrap();
        assert!(!c.browser.enabled);
        assert_eq!(c.browser.profile_dir, Some(PathBuf::from("/p")));
    }
}
```

- [ ] **1.7 确认通过**:

```bash
cargo test -p cloudcode-client 2>&1 | tail -5
```

预期:`test result: ok.`(0 failed;无 node 的环境 echo 夹具类测试自 skip)。再快速确认全删干净:

```bash
grep -rn "EMBEDDED_ECHO_MCP\|embedded_echo_backend" crates/ ; echo "exit=$?"
```

预期:无任何匹配,`exit=1`。

- [ ] **1.8 commit**:

```bash
cd /Users/vtech/cloudcode-agent/workspaces/petez/cloudcode_dev/cloudcode
git add crates/client/src/mcp_host.rs crates/client/src/main.rs crates/client/src/wire.rs crates/client/src/relay.rs crates/client/Cargo.toml Cargo.lock
git commit -m "client: [browser] config + pinned playwright-mcp default backend, drop echo stub

backend_command now takes &BrowserConfig: env CC_REMOTE_MCP_BACKEND >
[browser].backend > builtin npx -y @playwright/mcp@0.0.76
--user-data-dir=<state>/browser-profile (0700). Removes the plan-①
embedded echo scaffold (ee92506); test-fixtures/echo-mcp.mjs stays as a
test fixture.

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

### Task 2: agent `[browser]` 配置段

**Files:**
- Modify: `crates/agent/src/config.rs`(L27-31 `Config` 加字段;L146-171 `RemoteMcpConfig` 之后加 `BrowserConfig`;L245-270 测试模块旁加新测试)
- Modify: `crates/agent/src/main.rs`(L359-364 `--init` 模板 `[remote_mcp]` 注释段之后补 `[browser]` 注释段)
- Test: `crates/agent/src/config.rs` 模块内 `#[cfg(test)]`

**步骤(TDD):**

- [ ] **2.1 写失败测试**。在 `crates/agent/src/config.rs` 的 `mod remote_mcp_config_tests` 模块之后追加:

```rust
#[cfg(test)]
mod browser_config_tests {
    use super::*;

    #[test]
    fn browser_defaults() {
        // 段缺省整体:Default 实现。
        let d = BrowserConfig::default();
        assert!(d.web_enabled);
        assert_eq!(d.web_backend, None);

        // 段存在但字段缺省:serde 字段默认。
        let c: BrowserConfig = toml::from_str("").unwrap();
        assert!(c.web_enabled);
        assert_eq!(c.web_backend, None);

        // 显式覆盖。
        let c: BrowserConfig = toml::from_str(
            "web_enabled = false\nweb_backend = \"npx -y @playwright/mcp@0.0.76 --headless --browser=chromium\"",
        )
        .unwrap();
        assert!(!c.web_enabled);
        assert_eq!(
            c.web_backend.as_deref(),
            Some("npx -y @playwright/mcp@0.0.76 --headless --browser=chromium")
        );
    }

    #[test]
    fn config_without_browser_section_gets_defaults() {
        // 整段缺省 = 全默认零配置;既有 agent.toml(无 [browser])解析
        // 不变且拿到默认 browser。
        let cfg: Config = toml::from_str(
            "[hub]\nurl = \"wss://h\"\n\n[auth]\nregistration_token = \"t\"\n",
        )
        .unwrap();
        assert!(cfg.browser.web_enabled);
        assert_eq!(cfg.browser.web_backend, None);
    }
}
```

- [ ] **2.2 确认失败**:

```bash
cargo test -p cloudcode-agent browser_config 2>&1 | tail -10
```

预期:**编译失败**,`error[E0433]`/`error[E0422]: cannot find ... `BrowserConfig``、`Config` 无 `browser` 字段(`error[E0609]: no field `browser``)。

- [ ] **2.3 最小实现**。① `crates/agent/src/config.rs` 在 `impl Default for RemoteMcpConfig`(L163-171)之后插入:

```rust
fn browser_default_web_enabled() -> bool {
    true
}

/// `[browser]` 段(计划②):agent 本机 `web` 无头浏览器后端。与
/// `[remote_mcp]` 平行 —— 那边管 cc-browser proxy,这边只管注入配置
/// 里的 web stdio 条目。整段缺省 = 全默认零配置。
#[derive(Debug, Clone, Deserialize)]
pub struct BrowserConfig {
    /// 是否注入 `web` stdio server 条目。false ⇒ 注入配置只含
    /// cc-browser(回到计划①单 server 形态)。默认 true。
    #[serde(default = "browser_default_web_enabled")]
    pub web_enabled: bool,
    /// 整条 web 后端命令覆盖(空白分隔)。缺省 = 内置默认
    /// `npx -y @playwright/mcp@<pin> --headless`
    /// (pin 常量在 mcp_proxy.rs::PLAYWRIGHT_MCP_PKG)。
    #[serde(default)]
    pub web_backend: Option<String>,
}

impl Default for BrowserConfig {
    fn default() -> Self {
        Self {
            web_enabled: true,
            web_backend: None,
        }
    }
}
```

② `Config` 结构体(L5-31)在 `remote_mcp` 字段之后加:

```rust
    /// `[browser]` 段:agent 本机 web 无头后端(计划②)。整段缺省 =
    /// 全默认(零配置即用)。
    #[serde(default)]
    pub browser: BrowserConfig,
```

- [ ] **2.4 `--init` 模板补 `[browser]`**。`crates/agent/src/main.rs` L359-364,在模板字符串的 `# [remote_mcp]` 注释块(`# port    = 7110` 行)之后、收尾 `"#` 之前追加:

```rust
# [browser] controls the agent-local headless `web` browser entry that
# gets injected into claude's per-session MCP config (plan ②). Default:
# enabled, pinned playwright-mcp via npx (requires node >= 18 on this
# host). Uncomment to override.
# [browser]
# web_enabled = true
# web_backend = "npx -y @playwright/mcp@0.0.76 --headless --browser=chromium"
```

(它在 `format!` 原始字符串里,无需转义;模板中无 `{}` 冲突。)

- [ ] **2.5 确认通过**:

```bash
cargo test -p cloudcode-agent config 2>&1 | tail -5
```

预期:`test result: ok.`,含 `browser_config_tests::browser_defaults ... ok` 与 `browser_config_tests::config_without_browser_section_gets_defaults ... ok`。注意此刻 `cfg.browser` 尚无人消费,`cargo build` 可能报 `dead_code`?不会 —— pub struct 的 pub 字段在 bin crate 中 serde 反序列化路径已算使用;若 build 仍告警,以 Task 5 接线消除为准,本 Task 只需测试绿。

- [ ] **2.6 commit**:

```bash
git add crates/agent/src/config.rs crates/agent/src/main.rs
git commit -m "agent: [browser] config section (web_enabled / web_backend)

Parallel to [remote_mcp]; whole-section absence = all defaults, so
existing agent.toml files parse unchanged.

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

### Task 3: `mcp_config_json` 双 server 生成

**Files:**
- Modify: `crates/agent/src/mcp_proxy.rs`(L31-33 `CC_BROWSER_SERVER` 旁加 `WEB_SERVER`;L328-334 `mcp_config_json` 改签名重写;测试 L719-725 `config_has_http_url_with_token_under_cc_browser`、L746-762 `extract_token_roundtrip_and_garbage` 更新调用)
- Modify: `crates/agent/src/pty.rs`(L632 调用点补第三参 `None`,本 Task 行为保持 = 计划①)
- Test: `crates/agent/src/mcp_proxy.rs` 模块内 `#[cfg(test)]`

**步骤(TDD):**

- [ ] **3.1 写失败测试**。在 `crates/agent/src/mcp_proxy.rs` `mod tests` 内、既有 `config_has_http_url_with_token_under_cc_browser` 之后追加:

```rust
    #[test]
    fn config_with_web_backend_has_two_servers() {
        let args = vec![
            "-y".to_string(),
            "@playwright/mcp@0.0.76".to_string(),
            "--headless".to_string(),
        ];
        let s = mcp_config_json(7110, "abc123", Some(("npx", &args)));
        let v: serde_json::Value = serde_json::from_str(&s).unwrap();
        // web:stdio,claude 直 spawn,不走隧道。
        assert_eq!(v["mcpServers"][WEB_SERVER]["type"], "stdio");
        assert_eq!(v["mcpServers"][WEB_SERVER]["command"], "npx");
        assert_eq!(
            v["mcpServers"][WEB_SERVER]["args"],
            serde_json::json!(["-y", "@playwright/mcp@0.0.76", "--headless"])
        );
        assert!(v["mcpServers"][WEB_SERVER].get("url").is_none());
        // cc-browser:http,与计划①字节语义一致。
        assert_eq!(v["mcpServers"]["cc-browser"]["type"], "http");
        assert_eq!(
            v["mcpServers"]["cc-browser"]["url"],
            "http://127.0.0.1:7110/mcp/abc123"
        );
        assert!(v["mcpServers"]["cc-browser"].get("command").is_none());
    }

    #[test]
    fn config_without_web_is_single_server_plan1_shape() {
        // web=None(web_enabled=false / 命令解析失败)⇒ 退回计划①单
        // server 形态。
        let s = mcp_config_json(7110, "abc123", None);
        let v: serde_json::Value = serde_json::from_str(&s).unwrap();
        assert!(v["mcpServers"].get(WEB_SERVER).is_none());
        assert_eq!(
            v["mcpServers"]["cc-browser"]["url"],
            "http://127.0.0.1:7110/mcp/abc123"
        );
    }

    #[test]
    fn config_json_escapes_hostile_web_args() {
        // 换 serde_json 构造的动机:args 含引号/空格/反斜杠也必须产出
        // 合法 JSON(format! 拼接做不到)。
        let args = vec![r#"--user-data-dir=/tmp/has "quotes" and \slash"#.to_string()];
        let s = mcp_config_json(7110, "t0", Some(("npx", &args)));
        let v: serde_json::Value = serde_json::from_str(&s).expect("valid JSON");
        assert_eq!(
            v["mcpServers"][WEB_SERVER]["args"][0],
            r#"--user-data-dir=/tmp/has "quotes" and \slash"#
        );
    }

    #[test]
    fn extract_token_reads_cc_browser_from_dual_and_legacy_configs() {
        // 自愈回采兼容(spec 组件 1):token 永远从 cc-browser.url 取,
        // 双 server 新格式与计划①单 server 旧格式都要工作。
        let args = vec!["--headless".to_string()];
        let dual = mcp_config_json(7110, "deadbeef", Some(("npx", &args)));
        assert_eq!(extract_token_from_config(&dual), Some("deadbeef".to_string()));
        let single = mcp_config_json(7110, "deadbeef", None);
        assert_eq!(extract_token_from_config(&single), Some("deadbeef".to_string()));
        // 计划①真实写过盘的旧格式字面量(防 mcp_config_json 回归性巧合)。
        let legacy = r#"{"mcpServers":{"cc-browser":{"type":"http","url":"http://127.0.0.1:7110/mcp/cafebabe"}}}"#;
        assert_eq!(extract_token_from_config(legacy), Some("cafebabe".to_string()));
    }
```

- [ ] **3.2 确认失败**:

```bash
cargo test -p cloudcode-agent mcp_proxy 2>&1 | tail -15
```

预期:**编译失败**,`error[E0061]: this function takes 2 arguments but 3 arguments were supplied`(新测试按新签名调)、`error[E0425]: cannot find value `WEB_SERVER``。

- [ ] **3.3 最小实现**。① `crates/agent/src/mcp_proxy.rs` L33(`CC_BROWSER_SERVER` 定义)之后加:

```rust
/// claude 眼里 agent 本机无头浏览器的 MCP server 名(计划②)。stdio
/// 条目由 claude 直接 spawn,不走帧、不进 client,无需跨端 lockstep。
pub const WEB_SERVER: &str = "web";
```

② L328-334 的 `mcp_config_json` 整体替换为:

```rust
/// 生成 claude 要加载的 `--mcp-config` JSON。`web = Some((program,
/// args))` 时含两个 server:`web`(stdio,claude 直 spawn 的本机无头
/// 后端)+ `cc-browser`(http 指向本 proxy);`None`(`[browser]`
/// web_enabled=false 或 web 命令解析失败)退回计划①单 cc-browser。
/// 用 serde_json 构造:args 可含任意用户配置的路径/引号,必须正确
/// 转义;serde_json 输出确定(同输入同字节),D12 的字节稳定以本
/// 格式为新基线。
pub fn mcp_config_json(port: u16, token: &str, web: Option<(&str, &[String])>) -> String {
    let mut servers = serde_json::Map::new();
    if let Some((program, args)) = web {
        servers.insert(
            WEB_SERVER.to_string(),
            serde_json::json!({ "type": "stdio", "command": program, "args": args }),
        );
    }
    servers.insert(
        CC_BROWSER_SERVER.to_string(),
        serde_json::json!({
            "type": "http",
            "url": format!("http://127.0.0.1:{port}/mcp/{token}")
        }),
    );
    serde_json::json!({ "mcpServers": servers }).to_string()
}
```

③ 既有测试更新两处调用:L719-725 `config_has_http_url_with_token_under_cc_browser` 内 `let s = mcp_config_json(7110, "abc123");` 改为 `let s = mcp_config_json(7110, "abc123", None);`;L746-762 `extract_token_roundtrip_and_garbage` 内 `let json = mcp_config_json(7110, "abc123");` 改为 `let json = mcp_config_json(7110, "abc123", None);`。

④ `crates/agent/src/pty.rs` L632 调用点补第三参(本 Task 先 `None` 保持计划①行为,Task 5 接 `[browser]`):

```rust
            let mcp_cfg = crate::mcp_proxy::mcp_config_json(self.remote_mcp.port, &token, None);
```

- [ ] **3.4 确认通过**:

```bash
cargo test -p cloudcode-agent 2>&1 | tail -5
```

预期:`test result: ok.`(0 failed),含四个新测试与两处更新后的旧测试全过。

- [ ] **3.5 commit**:

```bash
git add crates/agent/src/mcp_proxy.rs crates/agent/src/pty.rs
git commit -m "agent: mcp_config_json grows an optional stdio web entry

New signature mcp_config_json(port, token, web: Option<(&str, &[String])>);
None keeps the plan-① single cc-browser shape (pty.rs passes None for
now). serde_json construction so user-configured args are JSON-escaped.
extract_token_from_config regression-tested against dual + legacy shapes.

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

### Task 4: `GUIDANCE_PROMPT` 换双后端选择文案

**Files:**
- Modify: `crates/agent/src/mcp_proxy.rs`(L35-43 `GUIDANCE_PROMPT` 注释 + 常量整体替换;测试 L728-743 `claude_args_carry_strict_flag_and_guidance` 补断言)
- Test: `crates/agent/src/mcp_proxy.rs` 模块内 `#[cfg(test)]`

**步骤(TDD):**

- [ ] **4.1 写失败测试**。在 `mod tests` 内、既有 `claude_args_carry_strict_flag_and_guidance` 之后追加:

```rust
    #[test]
    fn guidance_prompt_covers_dual_backend_selection_rules() {
        // 双后端文案(spec「选择机制与引导 prompt」):两 server 点名 +
        // 五条规则的关键措辞。仍不写死任何工具名(D11 通用化不变)。
        assert!(GUIDANCE_PROMPT.contains("`web`"));
        assert!(GUIDANCE_PROMPT.contains("`cc-browser`"));
        // 规则 1:默认恒 web,不问。
        assert!(GUIDANCE_PROMPT.contains("ALWAYS use `web`"));
        // 规则 2:明示才本地。
        assert!(GUIDANCE_PROMPT.contains("ONLY when the user explicitly"));
        // 规则 3:撞墙不自切、先问用户。
        assert!(GUIDANCE_PROMPT.contains("do NOT\n   switch to `cc-browser` on your own"));
        // 规则 4:任务级粘住一个、状态不互通(两条硬约束的注入面)。
        assert!(GUIDANCE_PROMPT.contains("Pick one server per task"));
        assert!(GUIDANCE_PROMPT.contains("NOT shared between `web` and `cc-browser`"));
        // 规则 6:not-connected 转达(①的降级文案沿用)。
        assert!(GUIDANCE_PROMPT.contains("not connected"));
        assert!(GUIDANCE_PROMPT.contains("cloudcode\n   CLI"));
    }
```

并在既有 `claude_args_carry_strict_flag_and_guidance`(L728-743)的尾部两行断言保持不变的前提下确认其仍含:

```rust
        // 引导文案通用化:点名 server,不写死任何工具名(决策 D11)。
        assert!(GUIDANCE_PROMPT.contains("cc-browser"));
        assert!(!GUIDANCE_PROMPT.contains("browser_navigate"));
```

(这两条对新文案依然成立,不改;`claude_mcp_args` 拼装本身零改动。)

- [ ] **4.2 确认失败**:

```bash
cargo test -p cloudcode-agent guidance_prompt_covers 2>&1 | tail -10
```

预期:编译通过、断言失败 —— `assertion failed: GUIDANCE_PROMPT.contains("\`web\`")`(旧文案是单后端)。

- [ ] **4.3 最小实现**。`crates/agent/src/mcp_proxy.rs` L35-43(`/// 注入给 claude 的通用引导…` 注释 + `GUIDANCE_PROMPT` 常量)整体替换为(文案逐字 = spec「选择机制与引导 prompt」,含换行缩进):

```rust
/// 注入给 claude 的双后端选择引导(计划②,经 --append-system-prompt):
/// 默认恒 `web`(agent 本机无头)、用户明示才 `cc-browser`(client 本地
/// 有头)、撞登录墙不自切先问、任务级粘住一个后端、两后端状态不互通。
/// 仍不写死任何工具名 —— 工具表由后端运行时决定(D11 通用化不变)。
pub const GUIDANCE_PROMPT: &str = r#"Two browser MCP servers are available:

- `web`: a HEADLESS browser running here on this host. Fast, invisible
  to the user, no setup needed.
- `cc-browser`: a VISIBLE browser window on the USER'S LOCAL machine,
  connected through the cloudcode CLI. The user can see it, log into
  sites in it, and operate it by hand. Its logins persist across
  sessions.

Rules:

1. For any web browsing — research, reading public pages, fetching
   data — ALWAYS use `web`. This is the default; do not ask.
2. Use `cc-browser` ONLY when the user explicitly asks for their local
   browser / cc-browser, or explicitly wants to log in or operate the
   page themselves.
3. If `web` hits a login wall, captcha, or anti-bot check, do NOT
   switch to `cc-browser` on your own. Stop and tell the user that the
   page needs them to log in or act in their local browser, and ask
   whether you should open it with `cc-browser`. Proceed only after
   they confirm.
4. Pick one server per task and stick with it. Browser state (cookies,
   logins, open pages) is NOT shared between `web` and `cc-browser` —
   they are separate browsers on separate machines, and state cannot
   be migrated mid-task.
5. When the user needs to do something by hand in `cc-browser` (e.g.
   log in or solve a captcha), pause and ask them to tell you when
   they are done, then continue from where you left off.
6. If a `cc-browser` tool call returns a 'not connected' style error,
   relay its instructions to the user (they need to open the cloudcode
   CLI on their local machine), then retry after they confirm."#;
```

- [ ] **4.4 确认通过**:

```bash
cargo test -p cloudcode-agent mcp_proxy 2>&1 | tail -5
```

预期:`test result: ok.`,含 `guidance_prompt_covers_dual_backend_selection_rules ... ok` 与既有 `claude_args_carry_strict_flag_and_guidance ... ok`(参数拼装顺序、`--strict-mcp-config` 不变)。

- [ ] **4.5 commit**:

```bash
git add crates/agent/src/mcp_proxy.rs
git commit -m "agent: dual-backend guidance prompt (web default, cc-browser on request)

Verbatim wording from the plan-② spec: always web for browsing, local
cc-browser only on explicit user ask, never self-switch on login walls,
one server per task, state not shared across machines. Injection
mechanism (claude_mcp_args / --append-system-prompt) unchanged.

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

### Task 5: pty 注入接 web 后端 + main 接线

**Files:**
- Modify: `crates/agent/src/mcp_proxy.rs`(`WEB_SERVER` 常量之后加 `PLAYWRIGHT_MCP_PKG`、`web_backend_command`、`parse_command`)
- Modify: `crates/agent/src/pty.rs`(L48-49 `remote_mcp` 字段后加 `browser` 字段;L69-86 `new` 加形参;L177-192 构造体;L632 注入块)
- Modify: `crates/agent/src/main.rs`(L220-229 `PtyManager::new` 调用加实参)
- Test: `crates/agent/src/mcp_proxy.rs` 模块内 `#[cfg(test)]`

**步骤(TDD):**

- [ ] **5.1 写失败测试**。在 `crates/agent/src/mcp_proxy.rs` `mod tests` 内追加(`self.browser → web: Option` 的映射即 `web_backend_command`,抽成纯函数在此单测;pty 注入块只是「调它再传给 `mcp_config_json`」的两行胶水):

```rust
    #[test]
    fn web_backend_default_is_pinned_headless_playwright() {
        // [browser] 全缺省 ⇒ 注入双 server,web = npx -y <pin> --headless。
        let cfg = crate::config::BrowserConfig::default();
        let (prog, args) = web_backend_command(&cfg).expect("default web backend");
        assert_eq!(prog, "npx");
        assert_eq!(
            args,
            vec![
                "-y".to_string(),
                PLAYWRIGHT_MCP_PKG.to_string(),
                "--headless".to_string()
            ]
        );
        assert_eq!(PLAYWRIGHT_MCP_PKG, "@playwright/mcp@0.0.76", "pin 版本单点");
    }

    #[test]
    fn web_backend_disabled_or_blank_override_yields_none() {
        // web_enabled=false ⇒ None ⇒ mcp_config_json 退单 server(=①)。
        let cfg = crate::config::BrowserConfig {
            web_enabled: false,
            web_backend: None,
        };
        assert_eq!(web_backend_command(&cfg), None);
        // 显式覆盖为空白串 = 解析失败 ⇒ 同样退单 server,不注入坏条目。
        let cfg = crate::config::BrowserConfig {
            web_enabled: true,
            web_backend: Some("   ".to_string()),
        };
        assert_eq!(web_backend_command(&cfg), None);
    }

    #[test]
    fn web_backend_explicit_override_is_parsed() {
        let cfg = crate::config::BrowserConfig {
            web_enabled: true,
            web_backend: Some("npx -y @playwright/mcp@0.0.76 --headless --browser=chromium".to_string()),
        };
        let (prog, args) = web_backend_command(&cfg).expect("override parsed");
        assert_eq!(prog, "npx");
        assert!(args.contains(&"--browser=chromium".to_string()));
    }

    #[test]
    fn injection_glue_default_config_yields_dual_server_json() {
        // 端到端纯函数串联:默认 [browser] → web_backend_command →
        // mcp_config_json = 双 server;web_enabled=false → 单 server。
        // 与 pty.rs 注入块逐字同形(那边只是 self.browser 换 cfg)。
        let cfg = crate::config::BrowserConfig::default();
        let web = web_backend_command(&cfg);
        let web_ref = web.as_ref().map(|(p, a)| (p.as_str(), a.as_slice()));
        let s = mcp_config_json(7110, "abc123", web_ref);
        let v: serde_json::Value = serde_json::from_str(&s).unwrap();
        assert_eq!(v["mcpServers"][WEB_SERVER]["command"], "npx");
        assert_eq!(v["mcpServers"]["cc-browser"]["type"], "http");

        let off = crate::config::BrowserConfig {
            web_enabled: false,
            web_backend: None,
        };
        let web = web_backend_command(&off);
        let web_ref = web.as_ref().map(|(p, a)| (p.as_str(), a.as_slice()));
        let s = mcp_config_json(7110, "abc123", web_ref);
        let v: serde_json::Value = serde_json::from_str(&s).unwrap();
        assert!(v["mcpServers"].get(WEB_SERVER).is_none());
    }
```

- [ ] **5.2 确认失败**:

```bash
cargo test -p cloudcode-agent web_backend 2>&1 | tail -10
```

预期:**编译失败**,`error[E0425]: cannot find function `web_backend_command``、`cannot find value `PLAYWRIGHT_MCP_PKG``。

- [ ] **5.3 最小实现(mcp_proxy.rs)**。在 `WEB_SERVER` 常量(Task 3 所加)之后插入:

```rust
/// 两后端共用的 playwright-mcp pin 版本(spec 组件 6)。与 client 侧
/// `crates/client/src/mcp_host.rs::PLAYWRIGHT_MCP_PKG` 手工 lockstep;
/// 升级须两处同改并重跑双后端冒烟。
pub const PLAYWRIGHT_MCP_PKG: &str = "@playwright/mcp@0.0.76";

/// 把空白分隔的命令串拆成 (程序, argv)。空串/全空白 → None。镜像
/// client 侧 mcp_host.rs::parse_backend(两 bin crate 无共享 lib,
/// 按 CC_BROWSER_SERVER 先例就地小复制)。
fn parse_command(cmd: &str) -> Option<(String, Vec<String>)> {
    let mut parts = cmd.split_whitespace().map(|s| s.to_string());
    let prog = parts.next()?;
    Some((prog, parts.collect()))
}

/// `[browser]` 段 → 注入配置里 web stdio 条目的命令(决策 P3)。
/// web_enabled=false 或显式覆盖解析失败 → None(注入退回计划①单
/// cc-browser);缺省 = 内置默认 `npx -y <pin> --headless`。
pub fn web_backend_command(
    cfg: &crate::config::BrowserConfig,
) -> Option<(String, Vec<String>)> {
    if !cfg.web_enabled {
        return None;
    }
    match &cfg.web_backend {
        Some(cmd) => parse_command(cmd),
        None => Some((
            "npx".to_string(),
            vec![
                "-y".to_string(),
                PLAYWRIGHT_MCP_PKG.to_string(),
                "--headless".to_string(),
            ],
        )),
    }
}
```

- [ ] **5.4 接线 pty.rs + main.rs**。① `crates/agent/src/pty.rs` L48-49,`remote_mcp` 字段之后加:

```rust
    /// `[browser]` 配置快照(web_enabled / web_backend):注入配置里
    /// web stdio 条目的来源(计划②)。
    browser: crate::config::BrowserConfig,
```

② 同文件 L69-86,`PtyManager::new` 的 `remote_mcp: crate::config::RemoteMcpConfig,` 形参之后加(注释 L70 的「8 个」改「9 个」):

```rust
        browser: crate::config::BrowserConfig,
```

③ 同文件 L177-192 构造体字面量,`remote_mcp,` 之后加一行 `browser,`。

④ 同文件 L632(Task 3 改过的那行)替换为:

```rust
            // [browser] → web stdio 条目:web_enabled=false / 解析失败
            // ⇒ None ⇒ 单 server(=计划①)。纯函数已单测,此处只是借用
            // 成 (&str, &[String]) 喂给 mcp_config_json。
            let web = crate::mcp_proxy::web_backend_command(&self.browser);
            let web_ref = web.as_ref().map(|(p, a)| (p.as_str(), a.as_slice()));
            let mcp_cfg =
                crate::mcp_proxy::mcp_config_json(self.remote_mcp.port, &token, web_ref);
```

⑤ `crates/agent/src/main.rs` L220-229,`PtyManager::new` 调用的 `config.remote_mcp.clone(),` 之后加一行:

```rust
        config.browser.clone(),
```

- [ ] **5.5 确认通过**:

```bash
cargo test -p cloudcode-agent 2>&1 | tail -5
cargo build -p cloudcode-agent 2>&1 | grep -c "^warning" || true
```

预期:`test result: ok.`(0 failed,含 5.1 四个新测试);build 告警计数 `0`(Task 2 里 `Config.browser` 的潜在 dead_code 此刻已被消费)。

- [ ] **5.6 commit**:

```bash
git add crates/agent/src/mcp_proxy.rs crates/agent/src/pty.rs crates/agent/src/main.rs
git commit -m "agent: inject dual-server MCP config wired to [browser]

PtyManager carries the [browser] snapshot; the per-session injection now
derives the web stdio entry via web_backend_command (default: npx -y
@playwright/mcp@0.0.76 --headless, pinned constant) and falls back to
the plan-① single-server shape when web_enabled=false or the override
fails to parse.

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

### Task 6: 集成回归 + 全仓绿 + clippy + node 要求文档

**Files:**
- Modify: `README.md`(Quick start 段补 node 要求一句)
- Test: 全仓(`cargo test --workspace`)+ 既有计划①集成测试原样回归

**说明(仓内实情):** 计划①的端到端/集成测试 —— `crates/agent/src/mcp_proxy.rs::loopback_tools_call_roundtrips_through_pipe_and_echo_backend` 与 `crates/client/src/mcp_host.rs` 的全部夹具测试 —— 都**直接以 `node <fixture 绝对路径>` 构造后端**(经 `concat!(env!("CARGO_MANIFEST_DIR"), "/../../test-fixtures/echo-mcp.mjs")`),从不经过 `backend_command()` 的 echo 回落,因此撤桩**不需要迁移任何测试**;本 Task 用 grep + 全量跑证实这一点。`CC_REMOTE_MCP_BACKEND=node .../echo-mcp.mjs` 作为**运行时**显式覆盖继续可用(Task 1 priority 测试已锁住),供手动管道冒烟(下 Task 7 用)。

**步骤:**

- [ ] **6.1 证实无残留 echo 生产路径、无测试依赖回落**:

```bash
cd /Users/vtech/cloudcode-agent/workspaces/petez/cloudcode_dev/cloudcode
grep -rn "EMBEDDED_ECHO_MCP\|embedded_echo_backend" crates/; echo "grep1 exit=$?"
ls test-fixtures/echo-mcp.mjs
grep -rn "echo-mcp.mjs" crates/ | grep -v "CARGO_MANIFEST_DIR"; echo "grep2 exit=$?"
```

预期:第一条 grep 无匹配(`grep1 exit=1`);夹具文件健在;第二条 grep 无匹配(`grep2 exit=1`,即所有引用都是夹具绝对路径形态)。

- [ ] **6.2 README 补 node 要求**。`README.md` 的 Quick start 代码块之后、`Open the admin UI at ...` 段落之前插入一行:

```markdown
> **Browser preset:** the default web-browsing backends on both the agent (headless) and the client (visible window) run `@playwright/mcp` via `npx` — install **node >= 18** on both machines to use them. Without node, everything else works; browser tool calls return an actionable error instead.
```

- [ ] **6.3 全仓测试**:

```bash
cargo test --workspace 2>&1 | tail -15
```

预期:每个 crate `test result: ok. ... 0 failed`。重点确认计划①回归:`loopback_tools_call_roundtrips_through_pipe_and_echo_backend ... ok`(管道没动)、`real_http_post_roundtrips_via_endpoint ... ok`、`mcp_host` 全部夹具测试 ok(无 node 环境则 skip,视为通过)。

- [ ] **6.4 构建零警告**:

```bash
cargo build --workspace 2>&1 | tee /tmp/cc-build.log | grep -E "^warning" ; echo "warnings-exit=$?"
```

预期:无输出、`warnings-exit=1`(0 条警告)。

- [ ] **6.5 clippy(新增/改动代码零告警)**:

```bash
cargo clippy -p cloudcode-agent -p cloudcode-client -- -D warnings 2>&1 | tail -30
```

判读标准:`-D warnings` 把告警升级为 error,所以看**报在哪个文件哪一行**——凡指向本计划触碰的代码(`mcp_proxy.rs` / `mcp_host.rs` / `pty.rs` / `config.rs` / `relay.rs` / `wire.rs` / 两个 `main.rs` 的新增/改动行),必须就地修掉(常见:`web_backend_command` 的 `match` 可换 `map_or`、借用链可简化)后重跑 6.3-6.5;凡指向本计划未触碰的既有 dev 债,**不管**(记录文件:行即可)。理想结局是命令整体通过(`Finished`)。

- [ ] **6.6 commit**:

```bash
git add README.md
git commit -m "docs: note node >= 18 requirement for the browser preset backends

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

### Task 7: 手动验证清单(交用户真机,不进 CI)

**Files:**(无代码;验证结论回填 `docs/superpowers/specs/2026-06-13-cc-browser-preset-design.md` 的「开放问题」1/2/3)

前置:agent 机与 client 机都装有 node ≥ 18(`node --version` 确认);三端跑本分支构建;一个 sandbox=true 的账号(沙箱在 hub 按账号开,admin UI Accounts 页)。

- [ ] **7.1 沙箱复验(首要,spec 开放问题 1)**。在 agent 真机上,模拟每会话沙箱 HOME 直接跑 web 后端:

```bash
# ① 裸机基线(排除非沙箱因素):
npx -y @playwright/mcp@0.0.76 --version
# ② 经 cloudcode 沙箱真实验证:用 sandbox=true 账号 open 一个工作区,
#    在 claude 里输入:
#    "Use the web browser to open https://example.com and tell me the page title."
```

预期:claude 调 `web` 的 browser 工具(对话里可见工具调用),返回标题 `Example Domain`;agent 本地与 client 本地都**不弹任何窗口**。**若失败**(claude 报 web server 不可用 / 工具调用报 chromium 启动错误):收集 agent 端报错(沙箱拦网络 or 拦 chromium 临时文件/共享内存写),按 spec 降级序处置——① 放宽沙箱 profile 为 chromium 开白名单;② 不可放宽则把 `crates/agent/src/config.rs::browser_default_web_enabled` 改为返回 `false` + 文档化(非沙箱 agent 手动开启),`cc-browser` 不受影响。**结论(直接可跑 / 落哪档降级)回填 spec 开放问题 1。**

- [ ] **7.2 `web` 默认路径(含 client 离线)**。把 client 关掉(或用 webterm 接入),在 claude 里问:"Search the web for the current Rust stable version and cite the page you read." 预期:claude 全程用 `web` 完成(不询问、不报 not-connected)、无任何本地窗口;再开着 client 重复一次,确认 claude 仍默认选 `web` 而非 `cc-browser`。

- [ ] **7.3 `cc-browser` 明示路径 + 持久登录**。client 机跑 `cloudcode` 进工作区,对 claude 说:"Open https://github.com/login in my local browser (cc-browser) and wait for me to log in." 预期:client 机弹出**可见** Chrome 窗口并导航到位;亲手登录 GitHub;对 claude 说 "done, now open my GitHub notifications page" → 已登录态直接进。然后 `Ctrl-C` 退出 client 再重开、重复打开 github.com → **仍是已登录**(profile 在 `~/.local/state/cloudcode/browser-profile`,可 `ls -ld` 确认 0700)。

- [ ] **7.4 撞墙对话协调(prompt 规则 3+5)**。对 claude 说:"Read my private GitHub notifications using the web browser."(不提 cc-browser)。预期:`web` 撞登录墙后 claude **不自行切换**,停下来说明该页需要本地登录并**询问**是否用 `cc-browser`;回答 yes → 本地弹窗 → 亲手登录 → 说 "done" → claude 从断点续跑完成任务。若 claude 自行切换或不询问 = 引导 prompt 措辞需要加强,记录实际对话回填。

- [ ] **7.5 首次 npx 拉包耗时/超时恢复(spec 开放问题 2/3)**。在一台**冷缓存**机器(或 `npm cache clean --force && rm -rf ~/.npm/_npx` 后)分别计时:① agent 侧首次 `web` 调用(claude 自身 ~30s MCP 连接超时是否打断首次握手);② client 侧首次 `cc-browser` 调用(①的 120s `tools/call` 档是否兜住;若一次超时,重试是否命中缓存即成)。同时记录 playwright-mcp 首跑缺 chromium 内核时的行为(自动下载耗时 / 是否需要文档化 `npx playwright install chromium` 预装)。**量化结论回填 spec 开放问题 2/3,并据此决定是否追加「预热/vendoring」后续任务。**

- [ ] **7.6 管道兼容冒烟(撤桩后 env 覆盖仍可用)**。client 机:

```bash
CC_REMOTE_MCP_BACKEND="node /path/to/cloudcode/test-fixtures/echo-mcp.mjs" cloudcode
```

进工作区后让 claude 调一次 cc-browser 工具(如 "call the echo tool on cc-browser with text 'pipe'")。预期:返回 `echo: pipe`——证明 env 覆盖优先级与①管道字节语义都没动。

- [ ] **7.7 收尾**:按仓惯例走 `superpowers:finishing-a-development-branch`——用户验证通过 → 合 `main` → bump MINOR → 打 tag 推送触发 CI(本计划无协议/帧改动,agent/client 可独立升级,无 lockstep 顺序要求)。

## 执行顺序与依赖

Task 1(client,独立)→ Task 2(agent config)→ Task 3(mcp_config_json,独立于 1/2)→ Task 4(prompt,独立)→ Task 5(依赖 2+3)→ Task 6(依赖 1-5)→ Task 7(真机,依赖 6)。Task 1 与 2/3/4 之间无代码依赖,可并行;Task 5 必须最后接线。

## spec 覆盖对照

| spec 条目 | 落点 |
|-----------|------|
| 组件 1 agent 注入双 server | Task 3(生成)+ Task 5(接线) |
| 组件 2 agent web 无头后端 | Task 5(默认命令/常量;无新进程代码,claude 直 spawn)+ Task 7.1/7.2 |
| 组件 3 client 默认后端撤桩 | Task 1 + Task 6.1 |
| 组件 4 引导 prompt | Task 4 |
| 组件 5 `[browser]` 配置段 | Task 1(client)+ Task 2(agent) |
| 组件 6 node/playwright-mcp 分发 | 决策 P6/P12 + Task 6.2(文档)+ Task 7.5(量化回填) |
| 选择机制与引导 prompt | Task 4(逐字文案)+ Task 7.2/7.4(行为验证) |
| 持久登录与用户接管 | Task 1(profile_dir/0700)+ Task 7.3;接管工具不做(决策 P10) |
| 错误处理与降级 | 复用①零改动;`-32004` 文案沿用(Task 4 规则 6);web spawn 失败归 claude 自身语义(Task 7.1 若拦走降级序) |
| 测试策略表(单元 5 行) | Task 3(双 server / token 回采)、Task 1(backend_command / toml 缺省)、Task 2(agent toml 缺省)、Task 4(prompt 措辞 / claude_mcp_args) |
| 测试策略表(集成回归) | Task 6.1/6.3 |
| 测试策略表(手动冒烟 5 行) | Task 7.1-7.5 |
| 发布 | Task 7.7(决策 P11) |

