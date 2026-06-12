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
#[allow(dead_code)] // Task 8 接线后使用
pub const CC_BROWSER_SERVER: &str = "cc-browser";

/// 把空白分隔的命令串拆成 (程序, argv)。空串/全空白 → None。
fn parse_backend(cmd: &str) -> Option<(String, Vec<String>)> {
    let mut parts = cmd.split_whitespace().map(|s| s.to_string());
    let prog = parts.next()?;
    Some((prog, parts.collect()))
}

/// 解析后端命令(计划①唯一来源,决策 D9):环境变量
/// `CC_REMOTE_MCP_BACKEND`,空白分隔,首段为程序、其余为 argv。
/// 未设置 → None(本机不提供远程-MCP 能力,Hello 能力位为 false)。
/// 计划②在此之上叠加 `[browser]` 配置段与内置默认后端。
#[allow(dead_code)] // Task 8 接线后使用
pub fn backend_command() -> Option<(String, Vec<String>)> {
    parse_backend(&std::env::var("CC_REMOTE_MCP_BACKEND").ok()?)
}

/// 已 spawn 的 MCP 子进程,说「按行分隔的 JSON-RPC over stdio」。
#[allow(dead_code)] // Task 6+ 接线后使用
pub struct McpProcess {
    child: Child,
    stdin: ChildStdin,
    lines: Lines<BufReader<ChildStdout>>,
}

#[allow(dead_code)] // Task 6+ 接线后使用
impl McpProcess {
    pub fn spawn(program: &str, args: &[String]) -> std::io::Result<Self> {
        let mut child = Command::new(program)
            .args(args)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::inherit())
            .kill_on_drop(true)
            .spawn()?;
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
        out_tx: mpsc::Sender<String>,
        handshake: Arc<Mutex<Vec<String>>>,
    ) -> std::io::Result<Self> {
        tracing::info!(program, ?args, "starting MCP backend subprocess");
        let proc = McpProcess::spawn(program, args)?;
        Ok(Self::from_process(proc, out_tx, handshake))
    }

    /// 重生:spawn 新子进程,先把缓存握手帧重放进去(重放出的
    /// initialize 响应按 id 吞掉 —— claude 手里已有一份),再接泵。
    /// 重放期间冒出的无关帧(如 server 主动通知)照常转发 out_tx。
    pub async fn start_replayed(
        program: &str,
        args: &[String],
        out_tx: mpsc::Sender<String>,
        handshake: Arc<Mutex<Vec<String>>>,
    ) -> std::io::Result<Self> {
        tracing::info!(program, ?args, "restarting MCP backend subprocess (handshake replay)");
        let mut proc = McpProcess::spawn(program, args)?;
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
}

impl McpHost {
    pub fn new(backend: (String, Vec<String>), out_tx: mpsc::Sender<String>) -> Self {
        Self {
            backend,
            chan: None,
            handshake: Arc::new(Mutex::new(Vec::new())),
            out_tx,
            consecutive_failures: 0,
            cooldown_until: None,
        }
    }

    /// 投递一帧给后端;后端没在跑就先(按重放语义)拉起。
    /// Err = 后端不可用,调用方应回发 RemoteMcpClosed 快速失败。
    pub async fn deliver(&mut self, payload: String) -> Result<(), McpHostError> {
        if self.chan.is_none() {
            self.spawn_channel().await?;
        }
        let Some(chan) = self.chan.as_ref() else {
            return Err(McpHostError::BackendUnavailable("spawn failed".to_string()));
        };
        if chan.feed(payload).is_err() {
            // 泵死(子进程崩溃/EOF):收摊并计一次失败;下一帧惰性重生。
            self.chan = None;
            self.note_failure();
            return Err(McpHostError::BackendUnavailable(
                "backend subprocess died".to_string(),
            ));
        }
        // feed 成功视为后端活着:清零连续失败计数(上限只惩罚连续失败,
        // 偶发崩溃 + claude 主动重试 = 每次重试一次重生机会)。
        self.consecutive_failures = 0;
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
            McpChannel::start(&prog, &args, self.out_tx.clone(), self.handshake.clone())
        } else {
            McpChannel::start_replayed(&prog, &args, self.out_tx.clone(), self.handshake.clone())
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

    #[tokio::test]
    async fn echo_stub_roundtrips_tools_list() {
        if !node_available() {
            return; // 无 node → skip
        }
        let fixture = concat!(env!("CARGO_MANIFEST_DIR"), "/../../test-fixtures/echo-mcp.mjs");
        let mut proc =
            McpProcess::spawn("node", &[fixture.to_string()]).expect("spawn echo stub");
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
        let chan = McpChannel::start("node", &args, out_tx.clone(), cache.clone()).expect("start");

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
        let chan = McpChannel::start_replayed("node", &args, out_tx.clone(), cache.clone())
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
        let chan = McpChannel::start("node", &args, out_tx.clone(), cache.clone()).expect("start");
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
        let chan = McpChannel::start_replayed("node", &args, out_tx.clone(), cache.clone())
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
        let chan = McpChannel::start("node", &[fixture.to_string()], out_tx, cache.clone())
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
        let mut host = McpHost::new(("node".to_string(), vec![fixture.to_string()]), out_tx);
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
        let mut host = McpHost::new(("node".to_string(), vec![fixture.to_string()]), out_tx);
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
}
