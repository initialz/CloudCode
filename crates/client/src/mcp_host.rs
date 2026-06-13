//! 通用 MCP 宿主(client 侧):拉起配置的 MCP-over-stdio 后端子进程,
//! 把不透明 JSON-RPC 帧(原文行)泵进/泵出。backend 无关:本模块不
//! 认识任何具体工具语义,只做 spawn / stdio 泵 / 握手缓存重放 / 退避
//! 重启。移植自 feature/local-browser:crates/client/src/cc_browser.rs,
//! 通用化并剥离授权门(决策 D2/D3)。

use std::sync::{Arc, Mutex};

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::mpsc;

/// claude 眼里固定的 MCP server 名(计划①唯一插槽)。与 agent 侧
/// `crates/agent/src/mcp_proxy.rs::CC_BROWSER_SERVER` 手工 lockstep。
pub const CC_BROWSER_SERVER: &str = "cc-browser";

/// 把空白分隔的命令串拆成 (程序, argv)。空串/全空白 → None。
fn parse_backend(cmd: &str) -> Option<(String, Vec<String>)> {
    let mut parts = cmd.split_whitespace().map(|s| s.to_string());
    let prog = parts.next()?;
    Some((prog, parts.collect()))
}

/// 两后端共用的 playwright-mcp pin 版本(spec 组件 6:pin 死保证两台
/// 机器、多次 npx 拉取行为一致)。与 agent 侧
/// `crates/agent/src/mcp_proxy.rs::PLAYWRIGHT_MCP_PKG` 手工 lockstep;
/// 升级须两处同改并重跑双后端冒烟。
pub const PLAYWRIGHT_MCP_PKG: &str = "@playwright/mcp@0.0.76";

/// 占位符:产物路径重写成 `{{CC_WS}}/<ARTIFACT_DIR_REL>/<name>`,由 agent
/// mcp_proxy 落地成工作区绝对路径。
/// LOCKSTEP: 与 agent `crates/agent/src/mcp_proxy.rs` 的 `WS_PLACEHOLDER` 必须一致。
pub const WS_PLACEHOLDER: &str = "{{CC_WS}}";

/// 产物在 agent workspace 里的相对目录(FsWrite 目标 + 重写路径用)。
pub const ARTIFACT_DIR_REL: &str = ".cloudcode/browser-artifacts";

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
    /// 路径中不得含空格(parse_backend 按空白切分,无引号解析)。
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
    match crate::state_dir() {
        Ok(d) => Some(d.join("browser-profile")),
        Err(_) => {
            tracing::warn!(
                "无法确定 state 目录,cc-browser 默认后端不可用(设 CLOUDCODE_STATE_DIR 可修)"
            );
            None
        }
    }
}

/// 浏览器产物 staging 目录(client 本地):后端 spawn 的 CWD 与
/// `--output-dir` 都钉到这里,使带/不带 filename 的截图都落到一处。
pub fn artifact_dir() -> Option<std::path::PathBuf> {
    match crate::state_dir() {
        Ok(d) => {
            let dir = d.join("browser-output");
            ensure_profile_dir(&dir); // 复用 0700 create_dir_all
            Some(dir)
        }
        Err(_) => {
            tracing::warn!("无法确定 state 目录,浏览器产物回传不可用");
            None
        }
    }
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

/// 解析后端命令(计划②,决策 P1):优先级 env `CC_REMOTE_MCP_BACKEND`
/// (操作员显式覆盖,优先于一切、含 enabled=false)→ `[browser].backend`
/// → 内置默认(pin 的 playwright-mcp,headed,持久 profile)。
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
    let mut args = vec![
        "-y".to_string(),
        PLAYWRIGHT_MCP_PKG.to_string(),
        format!("--user-data-dir={}", profile.display()),
    ];
    if let Some(out) = artifact_dir() {
        args.push(format!("--output-dir={}", out.display()));
    }
    Some(("npx".to_string(), args))
}

/// `backend_command` 会不会返回 Some —— 纯判断,不碰文件系统(不建
/// profile 目录)。供 wire.rs 设 Hello 能力位用;真正 spawn 走
/// `backend_command`(那条才建目录)。与 backend_command_from 的可用性
/// 判断逐条对齐。
pub fn capable_for_hello(cfg: &BrowserConfig) -> bool {
    capable_from(std::env::var("CC_REMOTE_MCP_BACKEND").ok().as_deref(), cfg)
}

/// 纯函数内核(env 注入为形参,单测不碰进程环境)。
fn capable_from(env_backend: Option<&str>, cfg: &BrowserConfig) -> bool {
    if let Some(cmd) = env_backend {
        return parse_backend(cmd).is_some();
    }
    if !cfg.enabled {
        return false;
    }
    if cfg.backend.is_some() {
        return cfg.backend.as_deref().and_then(parse_backend).is_some();
    }
    cfg.profile_dir.is_some() || default_profile_dir().is_some()
}

/// 已 spawn 的 MCP 子进程,说「按行分隔的 JSON-RPC over stdio」。
pub struct McpProcess {
    child: Child,
    stdin: ChildStdin,
    lines: Lines<BufReader<ChildStdout>>,
}

impl McpProcess {
    pub fn spawn(
        program: &str,
        args: &[String],
        cwd: Option<&std::path::Path>,
    ) -> std::io::Result<Self> {
        let mut cmd = Command::new(program);
        cmd.args(args)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::inherit())
            .kill_on_drop(true);
        if let Some(dir) = cwd {
            cmd.current_dir(dir);
        }
        let mut child = cmd.spawn()?;
        let stdin = child.stdin.take().expect("piped stdin");
        let stdout = child.stdout.take().expect("piped stdout");
        let lines = BufReader::new(stdout).lines();
        Ok(Self { child, stdin, lines })
    }

    /// 写一帧(换行分隔)进子进程 stdin。
    pub async fn feed(&mut self, payload: &str) -> std::io::Result<()> {
        self.stdin.write_all(payload.as_bytes()).await?;
        self.stdin.write_all(b"\n").await?;
        self.stdin.flush().await
    }

    /// 读下一帧;子进程 EOF → None。空行跳过。
    pub async fn next_frame(&mut self) -> Option<String> {
        loop {
            let line = match self.lines.next_line().await {
                Ok(opt) => opt,
                Err(e) => {
                    tracing::warn!("mcp subprocess stdout read error: {e}");
                    return None;
                }
            }?;
            if line.trim().is_empty() {
                continue;
            }
            return Some(line);
        }
    }

    /// 收摊:SIGKILL 直接子进程(npx 之类的包装层)并收尸。真正的后端
    /// 若是孙进程,靠本函数消费 self 掉落 stdin 收口 —— 规范 MCP server
    /// 监听 stdin 关闭后自行优雅退出(异步于本函数返回)。
    pub async fn shutdown(mut self) {
        let _ = self.child.start_kill();
        let _ = self.child.wait().await;
    }
}

/// 从 MCP 响应文本里找 markdown 链接 `](<target>)`,对每个 target 取
/// basename,若 `staging/<basename>` 存在,即本次调用产生、claude 即将
/// 去 Read 的产物。返回 `(链接 target 原串, basename)`。
pub fn detect_artifacts(payload: &str, staging: &std::path::Path) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut rest = payload;
    while let Some(open) = rest.find("](") {
        let after = &rest[open + 2..];
        if let Some(close) = after.find(')') {
            let target = &after[..close];
            // target 不含换行/括号才算合法链接路径
            if !target.is_empty() && !target.contains('\n') && !target.contains('(') {
                let base = target.rsplit('/').next().unwrap_or(target).to_string();
                if !base.is_empty() && staging.join(&base).is_file() {
                    let pair = (target.to_string(), base);
                    if !out.contains(&pair) {
                        out.push(pair);
                    }
                }
            }
            rest = &after[close + 1..];
        } else {
            break;
        }
    }
    out
}

/// 把响应里每个 `](<原 target>)` 替换成 `](<新值>)`。新值可以是
/// `{{CC_WS}}/...` 路径,也可以是超限/失败的提示文字。只替换被 `](` `)`
/// 包裹的精确原串,避免误伤正文。
pub fn rewrite_artifact_links(payload: &str, repl: &[(String, String)]) -> String {
    let mut out = payload.to_string();
    for (orig, new) in repl {
        let from = format!("]({})", orig);
        let to = format!("]({})", new);
        out = out.replace(&from, &to);
    }
    out
}

/// 读 JSON-RPC 帧的 `id`(通知/非 JSON → None)。
fn json_id(frame: &str) -> Option<serde_json::Value> {
    serde_json::from_str::<serde_json::Value>(frame)
        .ok()?
        .get("id")
        .cloned()
}

/// 读 JSON-RPC 帧的 `method`(无 method/非 JSON → None)。
fn json_method(frame: &str) -> Option<String> {
    serde_json::from_str::<serde_json::Value>(frame)
        .ok()?
        .get("method")?
        .as_str()
        .map(|s| s.to_string())
}

/// 宿主自有的合成握手(决策 D16)。id 用字符串 "cc-host-init":
/// start_replayed 的吞响应按 id 匹配,不会与 claude 的 id 冲突。
/// 只在「缓存为空且首帧不是 initialize」的冷启动缝隙调用;一经合成
/// 即入缓存,之后的重生走同一条重放路径。
fn synthesize_handshake(cache: &mut Vec<String>) {
    cache.push(format!(
        r#"{{"jsonrpc":"2.0","id":"cc-host-init","method":"initialize","params":{{"protocolVersion":"2025-06-18","capabilities":{{}},"clientInfo":{{"name":"cloudcode","version":"{}"}}}}}}"#,
        env!("CARGO_PKG_VERSION")
    ));
    cache.push(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#.to_string());
}

/// 一条在跑的后端 MCP 通道:pump 任务独占子进程,把子进程每帧输出
/// 转发到 `out_tx`;`feed` 把入站帧排队写给子进程。
///
/// 通道同时缓存途经 `feed` 的 MCP 握手帧(`initialize` 请求 +
/// `notifications/initialized`):claude 在一条活连接上绝不重发握手,
/// 后端(重)拉起时必须先重放缓存,在跑的 claude 会话才能无感续接。
/// 缓存由 `McpHost` 拥有、仅共享进每条通道(通道收摊不丢缓存)。
///
/// 与 M1-M3 `BrowserChannel` 的差异:去掉 `done_rx`(它只服务于 M3
/// headed/headless 切换时的有界等待,计划①无该路径;②如需再引入)。
pub struct McpChannel {
    in_tx: mpsc::Sender<String>,
    handshake: Arc<Mutex<Vec<String>>>,
}

impl McpChannel {
    /// 冷启动:直接 spawn 并接泵。缓存为空时用这个(真正首启,握手帧
    /// 正在路上,会经 `feed` 自然入缓存)。
    pub fn start(
        program: &str,
        args: &[String],
        cwd: Option<&std::path::Path>,
        out_tx: mpsc::Sender<String>,
        handshake: Arc<Mutex<Vec<String>>>,
    ) -> std::io::Result<Self> {
        tracing::info!(program, ?args, "starting MCP backend subprocess");
        let proc = McpProcess::spawn(program, args, cwd)?;
        Ok(Self::from_process(proc, out_tx, handshake))
    }

    /// 重生:spawn 新子进程,先把缓存握手帧重放进去(重放出的
    /// initialize 响应按 id 吞掉 —— claude 手里已有一份),再接泵。
    /// 重放期间冒出的无关帧(如 server 主动通知)照常转发 out_tx。
    pub async fn start_replayed(
        program: &str,
        args: &[String],
        cwd: Option<&std::path::Path>,
        out_tx: mpsc::Sender<String>,
        handshake: Arc<Mutex<Vec<String>>>,
    ) -> std::io::Result<Self> {
        tracing::info!(program, ?args, "restarting MCP backend subprocess (handshake replay)");
        let mut proc = McpProcess::spawn(program, args, cwd)?;
        let frames: Vec<String> = handshake.lock().expect("handshake mutex").clone();
        for frame in &frames {
            let init_id = if json_method(frame).as_deref() == Some("initialize") {
                json_id(frame)
            } else {
                None // notifications/initialized:无响应可等
            };
            proc.feed(frame).await?;
            if let Some(want) = init_id {
                // drain 到 initialize 响应被按 id 吞掉为止;唯一上限是 60s
                // 超时(防后端挂死)。绝不能用固定计数提前放弃 —— 否则未吞的
                // initialize 响应会被随后的 pump 转发给 claude,污染 id 配对。
                let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(60);
                loop {
                    let Ok(maybe) = tokio::time::timeout_at(deadline, proc.next_frame()).await
                    else {
                        tracing::warn!(
                            "timed out waiting for replayed initialize response; \
                             backend may send a duplicate initialize id to claude"
                        );
                        break;
                    };
                    let Some(resp) = maybe else {
                        tracing::warn!("backend EOF during handshake replay; channel will immediately die");
                        break;
                    };
                    if json_id(&resp).as_ref() == Some(&want) {
                        break; // 吞掉:claude 已有自己的 initialize 响应
                    }
                    if out_tx.send(resp).await.is_err() {
                        break; // 接收端已走,继续重放无意义
                    }
                }
            }
        }
        Ok(Self::from_process(proc, out_tx, handshake))
    }

    fn from_process(
        mut proc: McpProcess,
        out_tx: mpsc::Sender<String>,
        handshake: Arc<Mutex<Vec<String>>>,
    ) -> Self {
        let (in_tx, mut in_rx) = mpsc::channel::<String>(32);
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    inbound = in_rx.recv() => {
                        let Some(frame) = inbound else { break; };
                        if proc.feed(&frame).await.is_err() { break; }
                    }
                    outbound = proc.next_frame() => {
                        match outbound {
                            Some(frame) => { if out_tx.send(frame).await.is_err() { break; } }
                            None => break, // 子进程 EOF
                        }
                    }
                }
            }
            // 收尸 + 掉 stdin 管道;真正的后端按 stdin-close 约定异步退出。
            proc.shutdown().await;
        });
        Self { in_tx, handshake }
    }

    /// 非阻塞投递一帧。Err = 队列满或泵已死 —— 调用方应视为「通道死亡」
    /// 收摊(置 None),下一帧走惰性重生。
    pub fn feed(&self, frame: String) -> Result<(), ()> {
        self.maybe_cache_handshake(&frame);
        self.in_tx.try_send(frame).map_err(|_| ())
    }

    /// 缓存握手帧供重放。两帧齐(len>=2)后零解析开销;按帧等值去重
    /// (重放路径会把缓存帧再喂回 feed,不得增长缓存)。
    fn maybe_cache_handshake(&self, frame: &str) {
        let mut cache = self.handshake.lock().expect("handshake mutex");
        if cache.len() >= 2 || cache.iter().any(|f| f == frame) {
            return;
        }
        match json_method(frame).as_deref() {
            Some("initialize") | Some("notifications/initialized") => {
                cache.push(frame.to_string());
            }
            _ => {}
        }
    }
}

/// 后端连续 spawn 失败的上限;达到后进入冷却,期间一律快速失败
/// (relay 回发 RemoteMcpClosed,agent 立刻把在飞请求转成 JSON-RPC
/// 错误,claude 毫秒级可见,绝不等满超时)。
const MAX_CONSECUTIVE_SPAWN_FAILURES: u32 = 3;
/// 冷却时长;到点后允许再试(计数清零重来)。
const SPAWN_COOLDOWN: std::time::Duration = std::time::Duration::from_secs(60);

/// 宿主投递失败 —— relay 据此回发 `ClientToHub::RemoteMcpClosed`。
#[derive(Debug)]
pub enum McpHostError {
    BackendUnavailable(String),
}

impl std::fmt::Display for McpHostError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            McpHostError::BackendUnavailable(why) => write!(f, "backend unavailable: {why}"),
        }
    }
}

/// 通用 MCP 宿主:一个插槽。惰性拉起后端子进程(首帧到达才 spawn)、
/// 桥 stdio⇄隧道、崩溃带退避重启(连续失败上限 + 冷却)、握手缓存重放。
pub struct McpHost {
    backend: (String, Vec<String>),
    chan: Option<McpChannel>,
    handshake: Arc<Mutex<Vec<String>>>,
    out_tx: mpsc::Sender<String>,
    consecutive_failures: u32,
    cooldown_until: Option<tokio::time::Instant>,
    cwd: Option<std::path::PathBuf>,
}

impl McpHost {
    pub fn new(
        backend: (String, Vec<String>),
        out_tx: mpsc::Sender<String>,
        cwd: Option<std::path::PathBuf>,
    ) -> Self {
        Self {
            backend,
            chan: None,
            handshake: Arc::new(Mutex::new(Vec::new())),
            out_tx,
            consecutive_failures: 0,
            cooldown_until: None,
            cwd,
        }
    }

    /// 投递一帧给后端;后端没在跑就先(按重放语义)拉起。
    /// Err = 后端不可用,调用方应回发 RemoteMcpClosed 快速失败。
    pub async fn deliver(&mut self, payload: String) -> Result<(), McpHostError> {
        if self.chan.is_none() {
            // 冷启动缝合(决策 D16):缓存为空而首帧不是 initialize ——
            // claude 的真握手被 agent fallback 吃掉了。合成宿主自有握手
            // 入缓存;spawn_channel 据此走 start_replayed(重放 + 按 id
            // 吞掉合成 initialize 的响应)。
            {
                let mut cache = self.handshake.lock().expect("handshake mutex");
                if cache.is_empty() && json_method(&payload).as_deref() != Some("initialize") {
                    synthesize_handshake(&mut cache);
                }
            }
            self.spawn_channel().await?;
        }
        let Some(chan) = self.chan.as_ref() else {
            return Err(McpHostError::BackendUnavailable("spawn failed".to_string()));
        };
        if chan.feed(payload).is_err() {
            // 泵死(子进程崩溃/EOF):收摊并计一次失败 —— pump 级失败与
            // spawn 级失败一样累计入上限,否则"起得来但活不住"的后端会
            // 每帧无限重生(feed 只是入队、那一刻 pump 还活着,不能据此
            // 判定健康并清零)。下一帧惰性重生;连续 3 次死 → 60s 冷却。
            self.chan = None;
            self.note_failure();
            return Err(McpHostError::BackendUnavailable(
                "backend subprocess died".to_string(),
            ));
        }
        // feed 入队成功。连续失败计数只在冷却到期时清零(见 spawn_channel),
        // 不在此清零 —— 见上面注释。
        Ok(())
    }

    /// 收摊当前后端(响应 HubToClient::RemoteMcpClosed)。握手缓存
    /// 保留,之后的惰性重生靠它重放续接。
    pub fn shutdown(&mut self) {
        self.chan = None; // drop → 泵退出 → kill_on_drop 收尸
    }

    fn note_failure(&mut self) {
        self.consecutive_failures += 1;
        if self.consecutive_failures >= MAX_CONSECUTIVE_SPAWN_FAILURES {
            self.cooldown_until = Some(tokio::time::Instant::now() + SPAWN_COOLDOWN);
        }
    }

    async fn spawn_channel(&mut self) -> Result<(), McpHostError> {
        if let Some(until) = self.cooldown_until {
            if tokio::time::Instant::now() < until {
                return Err(McpHostError::BackendUnavailable(
                    "backend restarting too fast; in cooldown".to_string(),
                ));
            }
            self.cooldown_until = None;
            self.consecutive_failures = 0;
        }
        let (prog, args) = self.backend.clone();
        let empty = self.handshake.lock().expect("handshake mutex").is_empty();
        let started = if empty {
            McpChannel::start(
                &prog,
                &args,
                self.cwd.as_deref(),
                self.out_tx.clone(),
                self.handshake.clone(),
            )
        } else {
            McpChannel::start_replayed(
                &prog,
                &args,
                self.cwd.as_deref(),
                self.out_tx.clone(),
                self.handshake.clone(),
            )
            .await
        };
        match started {
            Ok(ch) => {
                self.chan = Some(ch);
                Ok(())
            }
            Err(e) => {
                self.note_failure();
                tracing::warn!(
                    error = %e,
                    failures = self.consecutive_failures,
                    "failed to start MCP backend subprocess"
                );
                Err(McpHostError::BackendUnavailable(format!(
                    "failed to start backend: {e}"
                )))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 测试环境探测:PATH 上有无 node(echo 桩需要)。无则该测试 skip。
    pub(super) fn node_available() -> bool {
        let Some(path) = std::env::var_os("PATH") else { return false };
        std::env::split_paths(&path).any(|d| d.join("node").is_file())
    }

    #[test]
    fn parse_backend_splits_program_and_args() {
        assert_eq!(
            parse_backend("npx -y @playwright/mcp@0.0.76 --headless"),
            Some((
                "npx".to_string(),
                vec![
                    "-y".to_string(),
                    "@playwright/mcp@0.0.76".to_string(),
                    "--headless".to_string()
                ]
            ))
        );
        assert_eq!(parse_backend("node"), Some(("node".to_string(), vec![])));
        assert_eq!(parse_backend(""), None);
        assert_eq!(parse_backend("   "), None);
    }

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
        // args[3](若有)只能是 --output-dir(产物 staging);除此之外不夹带。
        for a in &args[3..] {
            assert!(a.starts_with("--output-dir="), "默认命令不得夹带其他参数: {args:?}");
        }
        assert!(args.len() <= 4, "默认命令不得夹带其他参数: {args:?}");
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
    fn builtin_backend_includes_output_dir() {
        let cfg = BrowserConfig {
            enabled: true,
            backend: None,
            profile_dir: Some(std::env::temp_dir().join("cc-test-profile")),
        };
        let (prog, args) = backend_command_from(None, &cfg).expect("default backend");
        assert_eq!(prog, "npx");
        assert!(
            args.iter().any(|a| a.starts_with("--user-data-dir=")),
            "args={args:?}"
        );
        assert!(
            args.iter().any(|a| a.starts_with("--output-dir=")),
            "args={args:?}"
        );
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

    #[test]
    fn capable_from_env_wins_even_when_disabled() {
        // env 注入后端 ⇒ capable,纵使 enabled=false(与 backend_command_from
        // 的 env-over-disabled 语义对齐)。
        let cfg = BrowserConfig {
            enabled: false,
            backend: None,
            profile_dir: None,
        };
        assert!(capable_from(Some("node /tmp/echo.mjs"), &cfg));
        // 空白 env 串 ⇒ parse_backend None ⇒ 不可用(显式"没有后端")。
        assert!(!capable_from(Some("   "), &cfg));
    }

    #[test]
    fn capable_from_disabled_without_env_is_false() {
        let cfg = BrowserConfig {
            enabled: false,
            backend: None,
            profile_dir: Some(std::path::PathBuf::from("/unused")),
        };
        assert!(!capable_from(None, &cfg));
    }

    #[test]
    fn capable_from_explicit_backend_is_true() {
        let cfg = BrowserConfig {
            enabled: true,
            backend: Some("node /tmp/x.mjs".to_string()),
            profile_dir: None,
        };
        assert!(capable_from(None, &cfg));
    }

    #[test]
    fn capable_from_default_path_with_profile_dir_is_true() {
        let cfg = BrowserConfig {
            enabled: true,
            backend: None,
            profile_dir: Some(std::path::PathBuf::from("/some/profile")),
        };
        assert!(capable_from(None, &cfg));
    }

    #[test]
    fn capable_from_has_no_directory_side_effect() {
        // 纯判断绝不建 profile 目录(对照 backend_command_from 会建)。
        let dir = tempfile::tempdir().unwrap();
        let prof = dir.path().join("never-created");
        let cfg = BrowserConfig {
            enabled: true,
            backend: None,
            profile_dir: Some(prof.clone()),
        };
        assert!(capable_from(None, &cfg));
        assert!(
            !prof.exists(),
            "capable_from must not create the profile dir: {}",
            prof.display()
        );
    }

    #[tokio::test]
    async fn echo_stub_roundtrips_tools_list() {
        if !node_available() {
            return; // 无 node → skip
        }
        let fixture = concat!(env!("CARGO_MANIFEST_DIR"), "/../../test-fixtures/echo-mcp.mjs");
        let mut proc =
            McpProcess::spawn("node", &[fixture.to_string()], None).expect("spawn echo stub");
        proc.feed(r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#)
            .await
            .unwrap();
        let resp = proc.next_frame().await.expect("got a frame");
        assert!(resp.contains("echo"));
        proc.shutdown().await;
    }

    #[tokio::test]
    async fn channel_pumps_frames_both_ways() {
        if !node_available() {
            return;
        }
        let fixture = concat!(env!("CARGO_MANIFEST_DIR"), "/../../test-fixtures/echo-mcp.mjs");
        let (out_tx, mut out_rx) = mpsc::channel(8);
        let chan = McpChannel::start(
            "node",
            &[fixture.to_string()],
            None,
            out_tx,
            Arc::new(Mutex::new(Vec::new())),
        )
        .expect("start channel");
        chan.feed(r#"{"jsonrpc":"2.0","id":7,"method":"tools/list"}"#.to_string())
            .unwrap();
        let got = out_rx.recv().await.expect("a response frame");
        assert!(got.contains("echo"));
    }

    /// 重生必须:(1) 重放缓存握手进新子进程,(2) 把重放出的 initialize
    /// 响应按 id 吞掉(claude 手里已有一份,重复帧会污染配对)。echo 桩
    /// 对任何带 id 的请求按 id 应答、忽略无 id 帧,恰好压测重放管线。
    #[tokio::test]
    async fn start_replayed_replays_handshake_and_swallows_response() {
        if !node_available() {
            return;
        }
        let fixture = concat!(env!("CARGO_MANIFEST_DIR"), "/../../test-fixtures/echo-mcp.mjs");
        let args = vec![fixture.to_string()];
        let (out_tx, mut out_rx) = mpsc::channel(8);
        let cache = Arc::new(Mutex::new(Vec::new()));
        let chan = McpChannel::start("node", &args, None, out_tx.clone(), cache.clone()).expect("start");

        // 正常握手:initialize 响应到 out_rx,两帧入缓存。
        chan.feed(
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"t","version":"0"}}}"#
                .to_string(),
        )
        .unwrap();
        let init_resp = out_rx.recv().await.expect("initialize response");
        assert!(init_resp.contains("serverInfo"));
        chan.feed(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#.to_string())
            .unwrap();
        assert_eq!(cache.lock().unwrap().len(), 2);

        // 收摊后重生:握手重放,重放出的 initialize 响应(id 1)必须被吞。
        drop(chan);
        let chan = McpChannel::start_replayed("node", &args, None, out_tx.clone(), cache.clone())
            .await
            .expect("start_replayed");

        chan.feed(r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#.to_string())
            .unwrap();
        let next = tokio::time::timeout(std::time::Duration::from_secs(10), out_rx.recv())
            .await
            .expect("a frame within 10s")
            .expect("channel alive");
        // 重生后的第一帧必须是 tools/list 响应,不是重复的 initialize 响应。
        assert!(next.contains(r#""id":2"#), "expected tools/list response, got: {next}");
        assert!(!next.contains("serverInfo"), "duplicate initialize response leaked: {next}");
        assert!(next.contains("echo"));
        assert!(out_rx.try_recv().is_err(), "nothing else may be queued in between");
    }

    /// 回归:重放期间后端在 initialize 响应前发出大量帧(> 旧的 0..10
    /// 上限)时,initialize 响应仍必须被吞掉、绝不泄漏给 claude。
    #[tokio::test]
    async fn start_replayed_swallows_initialize_despite_many_interleaved_frames() {
        if !node_available() {
            return;
        }
        let fixture =
            concat!(env!("CARGO_MANIFEST_DIR"), "/../../test-fixtures/chatty-init-mcp.mjs");
        let args = vec![fixture.to_string()];
        let (out_tx, mut out_rx) = mpsc::channel(64);
        let cache = Arc::new(Mutex::new(Vec::new()));

        // 冷启动 + 握手:排空冷启动帧直到看到 initialize 响应(serverInfo)。
        let chan = McpChannel::start("node", &args, None, out_tx.clone(), cache.clone()).expect("start");
        chan.feed(r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#.to_string())
            .unwrap();
        loop {
            let f = tokio::time::timeout(std::time::Duration::from_secs(10), out_rx.recv())
                .await
                .expect("frame within 10s")
                .expect("channel alive");
            if f.contains("serverInfo") {
                break;
            }
        }
        chan.feed(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#.to_string())
            .unwrap();
        assert_eq!(cache.lock().unwrap().len(), 2);

        // 重生重放,再发 tools/list:排空直到 tools/list 响应(id 2),
        // 期间绝不能出现重复的 initialize 响应(serverInfo)。
        drop(chan);
        let chan = McpChannel::start_replayed("node", &args, None, out_tx.clone(), cache.clone())
            .await
            .expect("start_replayed");
        chan.feed(r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#.to_string())
            .unwrap();
        loop {
            let f = tokio::time::timeout(std::time::Duration::from_secs(10), out_rx.recv())
                .await
                .expect("frame within 10s")
                .expect("channel alive");
            assert!(
                !f.contains("serverInfo"),
                "duplicate initialize response leaked after replay: {f}"
            );
            if f.contains(r#""id":2"#) {
                break;
            }
        }
    }

    /// 握手缓存由宿主拥有、仅共享进通道:通道收摊(RemoteMcpClosed /
    /// 后端崩溃)不得丢缓存;重放路径把同一帧再喂回 feed 不得增长缓存。
    #[tokio::test]
    async fn shared_cache_survives_channel_drop_and_dedups() {
        if !node_available() {
            return;
        }
        let fixture = concat!(env!("CARGO_MANIFEST_DIR"), "/../../test-fixtures/echo-mcp.mjs");
        let (out_tx, mut out_rx) = mpsc::channel(8);
        let cache = Arc::new(Mutex::new(Vec::new()));
        let chan = McpChannel::start("node", &[fixture.to_string()], None, out_tx, cache.clone())
            .expect("start channel");
        let init = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#;
        chan.feed(init.to_string()).unwrap();
        let _ = out_rx.recv().await.expect("initialize response");
        assert_eq!(cache.lock().unwrap().len(), 1);
        chan.feed(init.to_string()).unwrap();
        assert_eq!(cache.lock().unwrap().len(), 1, "replayed frame must not grow cache");
        drop(chan);
        assert_eq!(cache.lock().unwrap().len(), 1, "cache outlives the channel");
    }

    #[tokio::test]
    async fn host_lazy_spawns_and_roundtrips_via_echo_stub() {
        if !node_available() {
            return;
        }
        let fixture = concat!(env!("CARGO_MANIFEST_DIR"), "/../../test-fixtures/echo-mcp.mjs");
        let (out_tx, mut out_rx) = mpsc::channel(8);
        let mut host = McpHost::new(("node".to_string(), vec![fixture.to_string()]), out_tx, None);
        // 首帧触发惰性 spawn;echo 桩应答按 id 配对回来。
        host.deliver(
            r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"echo","arguments":{"text":"hi"}}}"#
                .to_string(),
        )
        .await
        .expect("deliver");
        let resp = out_rx.recv().await.expect("echo response");
        assert!(resp.contains(r#""id":3"#) && resp.contains("echo: hi"), "got: {resp}");
    }

    #[tokio::test]
    async fn host_spawn_failure_caps_then_cools_down() {
        // 不存在的程序:每次 deliver 都 spawn 失败;到上限后进入冷却,
        // 冷却中不再尝试 spawn、错误文案可区分(快速失败)。
        let (out_tx, _out_rx) = mpsc::channel(8);
        let mut host = McpHost::new(
            ("/nonexistent/cloudcode-test-backend".to_string(), vec![]),
            out_tx,
            None,
        );
        for _ in 0..MAX_CONSECUTIVE_SPAWN_FAILURES {
            let err = host
                .deliver(r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#.to_string())
                .await
                .expect_err("spawn must fail");
            assert!(err.to_string().contains("failed to start backend"), "got: {err}");
        }
        let err = host
            .deliver(r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#.to_string())
            .await
            .expect_err("must be cooling down");
        assert!(err.to_string().contains("cooldown"), "got: {err}");
    }

    #[tokio::test]
    async fn host_shutdown_keeps_handshake_cache_for_respawn() {
        if !node_available() {
            return;
        }
        let fixture = concat!(env!("CARGO_MANIFEST_DIR"), "/../../test-fixtures/echo-mcp.mjs");
        let (out_tx, mut out_rx) = mpsc::channel(8);
        let mut host = McpHost::new(("node".to_string(), vec![fixture.to_string()]), out_tx, None);
        host.deliver(
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"t","version":"0"}}}"#
                .to_string(),
        )
        .await
        .expect("deliver initialize");
        let init_resp = out_rx.recv().await.expect("initialize response");
        assert!(init_resp.contains("serverInfo"));
        host.deliver(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#.to_string())
            .await
            .expect("deliver initialized");

        // 模拟 HubToClient::RemoteMcpClosed:收摊。缓存必须健在。
        host.shutdown();
        assert_eq!(host.handshake.lock().unwrap().len(), 2);

        // 下一帧触发带重放的惰性重生:直接得到 tools/list 响应,且不
        // 泄漏重复的 initialize 响应。
        host.deliver(r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#.to_string())
            .await
            .expect("deliver after shutdown");
        let next = tokio::time::timeout(std::time::Duration::from_secs(10), out_rx.recv())
            .await
            .expect("frame within 10s")
            .expect("alive");
        assert!(next.contains(r#""id":2"#) && !next.contains("serverInfo"), "got: {next}");
    }

    /// 回归:后端 spawn 正常但 pump 秒死("起得来活不住"),也必须在
    /// 有限次 deliver 内撞上冷却,而不是每帧无限重生。
    #[tokio::test]
    async fn host_pump_death_loop_is_bounded_by_cooldown() {
        if !node_available() {
            return;
        }
        let fixture =
            concat!(env!("CARGO_MANIFEST_DIR"), "/../../test-fixtures/exit-on-frame-mcp.mjs");
        let (out_tx, _out_rx) = mpsc::channel(8);
        let mut host = McpHost::new(("node".to_string(), vec![fixture.to_string()]), out_tx, None);
        let mut saw_cooldown = false;
        for _ in 0..30 {
            if let Err(e) = host
                .deliver(r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#.to_string())
                .await
            {
                if e.to_string().contains("cooldown") {
                    saw_cooldown = true;
                    break;
                }
            }
            // 给泵一点时间感知子进程退出,使下一帧的 feed 命中"泵已死"。
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        assert!(
            saw_cooldown,
            "spawn-ok-but-pump-dies backend must hit the cooldown within 30 delivers, not storm forever"
        );
    }

    #[test]
    fn detect_artifacts_from_markdown_links() {
        let staging = std::env::temp_dir().join("cc-detect-test");
        let _ = std::fs::create_dir_all(&staging);
        // 造两个 staging 文件:一个被链接引用(shot.png)、一个不被引用(orphan.png)
        std::fs::write(staging.join("shot.png"), b"x").unwrap();
        std::fs::write(staging.join("orphan.png"), b"x").unwrap();

        // 带 filename 的截图响应:链接 target 是 ./shot.png
        // NOTE: \n inside this JSON string literal is the two characters backslash-n
        // (as it would appear in a real JSON-encoded MCP response), not a real newline.
        let payload = "{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"content\":[{\"type\":\"text\",\"text\":\"### Result\\n- [Screenshot of viewport](./shot.png)\\n### Ran Playwright code\"}]}}";
        let found = detect_artifacts(payload, &staging);
        // 只检测到被链接引用且存在于 staging 的文件
        assert_eq!(found, vec![("./shot.png".to_string(), "shot.png".to_string())]);

        // 链接指向不存在于 staging 的文件 → 不检测
        let none = "{\"text\":\"- [x](./missing.png)\"}";
        assert!(detect_artifacts(none, &staging).is_empty());

        // 无 markdown 链接 → 空
        assert!(detect_artifacts("{\"text\":\"plain\"}", &staging).is_empty());

        let _ = std::fs::remove_dir_all(&staging);
    }

    #[tokio::test]
    async fn host_synthesizes_handshake_when_cold_started_mid_session() {
        if !node_available() {
            return;
        }
        // 决策 D16 的缝:claude 冷启动时 initialize 被 agent 侧 fallback
        // 权威应答(client 不在线),宿主缓存里没有真握手;client 上线后
        // 第一帧直接是 tools/list —— 宿主必须自己合成握手喂后端,并吞掉
        // 合成 initialize 的响应,否则后端报「未初始化」。
        let fixture = concat!(env!("CARGO_MANIFEST_DIR"), "/../../test-fixtures/echo-mcp.mjs");
        let (out_tx, mut out_rx) = mpsc::channel(8);
        let mut host = McpHost::new(("node".to_string(), vec![fixture.to_string()]), out_tx, None);
        host.deliver(r#"{"jsonrpc":"2.0","id":9,"method":"tools/list"}"#.to_string())
            .await
            .expect("deliver");
        let resp = tokio::time::timeout(std::time::Duration::from_secs(10), out_rx.recv())
            .await
            .expect("frame within 10s")
            .expect("alive");
        assert!(
            resp.contains(r#""id":9"#),
            "first visible frame is the tools/list response: {resp}"
        );
        assert!(
            !resp.contains("serverInfo"),
            "synthesized initialize response must be swallowed: {resp}"
        );
        assert_eq!(
            host.handshake.lock().unwrap().len(),
            2,
            "synthesized handshake cached for future respawns"
        );
    }

    #[test]
    fn rewrite_artifact_links_replaces_targets() {
        let payload = "- [Screenshot of viewport](./shot.png)\n- [PDF](./doc.pdf)";
        let repl = vec![
            ("./shot.png".to_string(), "{{CC_WS}}/.cloudcode/browser-artifacts/shot.png".to_string()),
            ("./doc.pdf".to_string(), "[browser artifact not transferred: doc.pdf (12 MiB); generated on client only]".to_string()),
        ];
        let out = rewrite_artifact_links(payload, &repl);
        // shot.png link now points at the placeholder path
        assert!(out.contains("[Screenshot of viewport]({{CC_WS}}/.cloudcode/browser-artifacts/shot.png)"));
        // doc.pdf link now carries the oversize note
        assert!(out.contains("[PDF]([browser artifact not transferred: doc.pdf (12 MiB); generated on client only])"));
        // original targets are gone
        assert!(!out.contains("(./shot.png)"));
        assert!(!out.contains("(./doc.pdf)"));
    }

    /// playwright chromium 是否已装(macOS `~/Library/Caches/ms-playwright`,
    /// Linux `~/.cache/ms-playwright`)。无则真后端测试 skip,保 CI 在无 chromium
    /// 机器上常绿。
    fn playwright_chromium_available() -> bool {
        let Some(home) = dirs::home_dir().or_else(|| std::env::var_os("HOME").map(Into::into))
        else {
            return false;
        };
        let caches = [
            home.join("Library/Caches/ms-playwright"), // macOS
            home.join(".cache/ms-playwright"),         // Linux
        ];
        caches.iter().any(|dir| {
            let Ok(entries) = std::fs::read_dir(dir) else { return false };
            entries.flatten().any(|e| {
                e.file_name().to_string_lossy().starts_with("chromium") && e.path().is_dir()
            })
        })
    }

    /// 集成测试:用真 `@playwright/mcp` 后端(非 echo 桩)驱动真 chromium 走完
    /// 一次 MCP 往返——证明 `McpHost` 能 spawn 真 playwright-mcp、开真 chromium、
    /// 完成握手并把一次真实的 browser_navigate 应答回送(Task 7.3 后端层入 CI)。
    /// node + chromium 门控:任一缺失即 skip,保 CI 常绿。
    #[tokio::test]
    async fn host_roundtrips_via_real_playwright_mcp() {
        if !node_available() || !playwright_chromium_available() {
            return;
        }
        // 隔离的临时 profile —— 不碰用户真实 cc-browser 资料目录。
        let profile = tempfile::tempdir().expect("tempdir");
        let user_data_dir = format!("--user-data-dir={}", profile.path().display());
        let prog = "npx".to_string();
        let args = vec![
            "-y".to_string(),
            PLAYWRIGHT_MCP_PKG.to_string(),
            "--headless".to_string(),
            user_data_dir,
        ];

        let (out_tx, mut out_rx) = mpsc::channel(16);
        let mut host = McpHost::new((prog, args), out_tx, None);

        // 1) initialize(惰性 spawn 真后端)+ notifications/initialized。
        host.deliver(
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"cloudcode-itest","version":"0"}}}"#
                .to_string(),
        )
        .await
        .expect("deliver initialize — backend must spawn");
        host.deliver(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#.to_string())
            .await
            .expect("deliver initialized — backend must be alive");

        // 读到 initialize 响应(serverInfo / id:1)。npx 首跑可能现装包,给 30s。
        let init_resp = loop {
            let frame = tokio::time::timeout(std::time::Duration::from_secs(30), out_rx.recv())
                .await
                .expect("initialize response within 30s")
                .expect("backend alive (out channel not closed)");
            if frame.contains(r#""id":1"#) {
                break frame;
            }
            // 后端在握手期可能先发别的通知;跳过非 id:1 帧。
        };
        assert!(
            init_resp.contains("serverInfo"),
            "initialize response must carry serverInfo: {init_resp}"
        );

        // 2) tools/call browser_navigate → example.com(id 2)。首跑要起 chromium +
        //    导航,可能 ~60s,给 90s。
        host.deliver(
            r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"browser_navigate","arguments":{"url":"https://example.com"}}}"#
                .to_string(),
        )
        .await
        .expect("deliver browser_navigate — backend must be alive");

        let nav_resp = loop {
            let frame = tokio::time::timeout(std::time::Duration::from_secs(90), out_rx.recv())
                .await
                .expect("browser_navigate response within 90s (real chromium launch)")
                .expect("backend alive (out channel not closed)");
            if frame.contains(r#""id":2"#) {
                break frame;
            }
            // 期间可能夹带 log/progress 通知;只认 id:2 响应。
        };
        // 不强校验 navigate 载荷:关键是一帧真实的 id:2 响应回来了,证明整条
        // McpHost → playwright-mcp → chromium → 响应 往返打通。
        assert!(
            nav_resp.contains(r#""id":2"#),
            "real id-2 navigate response must round-trip back: {nav_resp}"
        );

        host.shutdown();
    }

    /// 集成测试(node+chromium 门控):真 `@playwright/mcp` 截图——以 claude 的
    /// 真实习惯(显式 `filename`)调 `browser_take_screenshot`,逼出 CWD 相对落盘——
    /// 必须把文件写进 staging 目录;再证 `detect_artifacts` 识别出它、
    /// `rewrite_artifact_links` 把应答里的链接改写成
    /// `{{CC_WS}}/.cloudcode/browser-artifacts/...`。环境缺失(无 node / chromium 起不来)
    /// 一律 skip 保 CI 常绿;只有真断言(没落盘 / 没识别 / 没改写)才算失败。
    #[tokio::test]
    async fn screenshot_lands_in_staging_and_is_detected() {
        if !node_available() {
            eprintln!("skip: node not available");
            return;
        }

        // 唯一 staging 目录(按 pid 区分),既当 CWD 又当 --output-dir。
        let staging = std::env::temp_dir().join(format!("cc-art-it-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&staging);
        std::fs::create_dir_all(&staging).expect("create staging dir");

        let args: Vec<String> = vec![
            "-y".into(),
            PLAYWRIGHT_MCP_PKG.into(),
            "--headless".into(),
            format!("--output-dir={}", staging.display()),
        ];
        let mut proc = match McpProcess::spawn("npx", &args, Some(staging.as_path())) {
            Ok(p) => p,
            Err(_) => {
                eprintln!("skip: spawn failed");
                let _ = std::fs::remove_dir_all(&staging);
                return;
            }
        };

        // 按 id 匹配应答的小工具:跳过握手期的通知/进度帧,EOF/超时即 None。
        async fn await_id(
            proc: &mut McpProcess,
            id: &str,
            secs: u64,
        ) -> Option<String> {
            loop {
                let frame = match tokio::time::timeout(
                    std::time::Duration::from_secs(secs),
                    proc.next_frame(),
                )
                .await
                {
                    Ok(Some(f)) => f,
                    // 超时或后端 EOF —— 环境问题,交由调用方 skip。
                    Ok(None) | Err(_) => return None,
                };
                if frame.contains(id) {
                    return Some(frame);
                }
            }
        }

        // 1) initialize(协议 2025-06-18)+ notifications/initialized。
        if proc
            .feed(
                r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"cloudcode-itest","version":"0"}}}"#,
            )
            .await
            .is_err()
        {
            eprintln!("skip: feed initialize failed");
            proc.shutdown().await;
            let _ = std::fs::remove_dir_all(&staging);
            return;
        }
        // npx 首跑可能现装包,给 30s 读 initialize 响应。
        if await_id(&mut proc, r#""id":1"#, 30).await.is_none() {
            eprintln!("skip: no initialize response (env)");
            proc.shutdown().await;
            let _ = std::fs::remove_dir_all(&staging);
            return;
        }
        if proc
            .feed(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#)
            .await
            .is_err()
        {
            eprintln!("skip: feed initialized failed");
            proc.shutdown().await;
            let _ = std::fs::remove_dir_all(&staging);
            return;
        }

        // 2) browser_navigate → example.com(id 2)。首跑要起 chromium,给 90s。
        if proc
            .feed(
                r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"browser_navigate","arguments":{"url":"https://example.com"}}}"#,
            )
            .await
            .is_err()
        {
            eprintln!("skip: feed navigate failed");
            proc.shutdown().await;
            let _ = std::fs::remove_dir_all(&staging);
            return;
        }
        if await_id(&mut proc, r#""id":2"#, 90).await.is_none() {
            eprintln!("skip: navigate failed (chromium cannot launch in this env)");
            proc.shutdown().await;
            let _ = std::fs::remove_dir_all(&staging);
            return;
        }

        // 3) browser_wait_for time=1(id 3)——避开 "execution context destroyed" 竞态。
        if proc
            .feed(
                r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"browser_wait_for","arguments":{"time":1}}}"#,
            )
            .await
            .is_err()
        {
            eprintln!("skip: feed wait_for failed");
            proc.shutdown().await;
            let _ = std::fs::remove_dir_all(&staging);
            return;
        }
        if await_id(&mut proc, r#""id":3"#, 30).await.is_none() {
            eprintln!("skip: wait_for failed (env)");
            proc.shutdown().await;
            let _ = std::fs::remove_dir_all(&staging);
            return;
        }

        // 4) browser_take_screenshot —— 必带显式 filename(复刻 claude 习惯,逼 CWD 相对落盘)。
        if proc
            .feed(
                r#"{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"browser_take_screenshot","arguments":{"type":"png","filename":"shot.png"}}}"#,
            )
            .await
            .is_err()
        {
            eprintln!("skip: feed screenshot failed");
            proc.shutdown().await;
            let _ = std::fs::remove_dir_all(&staging);
            return;
        }
        let screenshot_resp = match await_id(&mut proc, r#""id":4"#, 90).await {
            Some(r) => r,
            None => {
                eprintln!("skip: screenshot failed (env)");
                proc.shutdown().await;
                let _ = std::fs::remove_dir_all(&staging);
                return;
            }
        };

        proc.shutdown().await;

        // ── 真正的验证 ──────────────────────────────────────────────────
        assert!(
            staging.join("shot.png").is_file(),
            "staging missing screenshot; resp={screenshot_resp}"
        );
        let found = detect_artifacts(&screenshot_resp, &staging);
        assert!(
            found.iter().any(|(_, b)| b == "shot.png"),
            "not detected; resp={screenshot_resp}"
        );
        let repl: Vec<(String, String)> = found
            .iter()
            .map(|(t, b)| {
                (
                    t.clone(),
                    format!("{}/{}/{}", WS_PLACEHOLDER, ARTIFACT_DIR_REL, b),
                )
            })
            .collect();
        let rewritten = rewrite_artifact_links(&screenshot_resp, &repl);
        assert!(
            rewritten.contains(&format!("{}/{}/shot.png", WS_PLACEHOLDER, ARTIFACT_DIR_REL)),
            "rewrite missing; got={rewritten}"
        );

        let _ = std::fs::remove_dir_all(&staging);
    }
}
