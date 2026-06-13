//! 远程-MCP proxy(agent 侧):进程内 localhost HTTP MCP 端点。
//! claude(MCP client)连到这里;帧经既有 agent<->hub ws 以
//! ClientMsg::RemoteMcp 隧道给绑定 client 的后端子进程。
//!
//! 传输:Streamable HTTP、POST 阻塞式。claude POST 一条 JSON-RPC 请求,
//! proxy 转发给 client 并**阻塞**到按 JSON-RPC `id` 配对的响应回来,把
//! 响应体作为 POST 响应返回;通知(无 `id`)转发后立刻 202 无体。
//!
//! proxy 是哑中继 —— 不实现 MCP 语义(握手、工具 schema 都在 claude 与
//! client 后端之间端到端流动),只:按 token→session_id 路由、按
//! (session, server, id) 配对、隧道不透明 JSON 文本、按 method 选超时档。
//!
//! 【铁坑,绝不回退】传输层故障(token 未注册、超时、通道拆除)对
//! JSON-RPC **请求**一律返回 HTTP 200 + JSON-RPC error 对象,绝不裸回
//! 非 2xx:claude 把 MCP POST 的任何非 2xx 当成「需要认证」,触发 OAuth
//! 探测瀑布并报误导性 `SDK auth failed: HTTP 404`(M1-M3 实测教训)。
//!
//! 移植自 feature/local-browser:crates/agent/src/mcp_endpoint.rs,
//! 通用化:Browser* → RemoteMcp*,帧带 server 字段,长档超时改为
//! LONG_CALL_TOOLS 名单驱动(决策 D3/D13/D14)。

use dashmap::DashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{mpsc, oneshot, RwLock};
use uuid::Uuid;

use crate::pty::OutFrame;
use crate::tunnel::ClientMsg;

/// claude 眼里固定的 MCP server 名(计划①唯一插槽)。与 client 侧
/// `crates/client/src/mcp_host.rs::CC_BROWSER_SERVER` 手工 lockstep。
pub const CC_BROWSER_SERVER: &str = "cc-browser";

/// claude 眼里 agent 本机无头浏览器的 MCP server 名(计划②)。stdio
/// 条目由 claude 直接 spawn,不走帧、不进 client,无需跨端 lockstep。
pub const WEB_SERVER: &str = "web";

/// 两后端共用的 playwright-mcp pin 版本(spec 组件 6)。与 client 侧
/// `crates/client/src/mcp_host.rs::PLAYWRIGHT_MCP_PKG` 手工 lockstep;
/// 升级须两处同改并重跑双后端冒烟。
pub const PLAYWRIGHT_MCP_PKG: &str = "@playwright/mcp@0.0.76";

/// 占位符:client 端把产物路径重写成 `{{CC_WS}}/.cloudcode/browser-artifacts/<name>`,
/// 本 proxy 在交给 claude 前替换成本会话工作区绝对路径。
/// LOCKSTEP: 与 client `crates/client/src/mcp_host.rs` 的 `WS_PLACEHOLDER` 必须一致。
pub const WS_PLACEHOLDER: &str = "{{CC_WS}}";

/// 把响应文本里的 `{{CC_WS}}` 占位符替换成本会话工作区绝对路径。
/// `ws_abs` 为空时不替换(无映射时的安全 no-op)。
pub fn substitute_ws_placeholder(payload: &str, ws_abs: &str) -> String {
    if ws_abs.is_empty() || !payload.contains(WS_PLACEHOLDER) {
        return payload.to_string();
    }
    payload.replace(WS_PLACEHOLDER, ws_abs)
}

/// 把空白分隔的命令串拆成 (程序, argv)。空串/全空白 → None。镜像
/// client 侧 mcp_host.rs::parse_backend(两 bin crate 无共享 lib,
/// 按 CC_BROWSER_SERVER 先例就地小复制)。
fn parse_command(cmd: &str) -> Option<(String, Vec<String>)> {
    let mut parts = cmd.split_whitespace().map(|s| s.to_string());
    let prog = parts.next()?;
    Some((prog, parts.collect()))
}

/// `[browser]` 段 → 注入配置里 web stdio 条目的命令(决策 P3)。
/// web_enabled 先于 web_backend 检查:`web_enabled=false` 恒返回 None,
/// 即便同时设了 web_backend(也不会解析它)。web_enabled=true 时显式
/// 覆盖解析失败(空白串)→ None;两种 None 都让注入退回计划①单
/// cc-browser。缺省(web_enabled=true 且无覆盖)= 内置默认
/// `npx -y <pin> --headless`。
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

/// 单后端引导:web_enabled=false(或 web 命令解析失败)时,注入配置里
/// 只有 `cc-browser`、没有 `web`。此时绝不能让 claude 去用一个不存在的
/// `web` server(否则 unknown-server 报错 + 误导)。只描述 cc-browser。
pub const GUIDANCE_PROMPT_CC_BROWSER_ONLY: &str = r#"A browser MCP server is available:

- `cc-browser`: a VISIBLE browser window on the USER'S LOCAL machine,
  connected through the cloudcode CLI. The user can see it, log into
  sites in it, and operate it by hand. Its logins persist across
  sessions.

Use `cc-browser` for web browsing the user asks for. If a `cc-browser`
tool call returns a 'not connected' style error, relay its instructions
to the user (they need to open the cloudcode CLI on their local
machine), then retry after they confirm."#;

/// 按「web 是否真被注入」选引导文案:有 web ⇒ 双后端文案;无 ⇒ 单
/// cc-browser 文案(不提 web,避免 unknown-server 陷阱)。
pub fn guidance_prompt(has_web: bool) -> &'static str {
    if has_web {
        GUIDANCE_PROMPT
    } else {
        GUIDANCE_PROMPT_CC_BROWSER_ONLY
    }
}

/// 在飞请求的配对键:(session_id, server 名, 规范化 JSON-RPC id)。
/// server 进键位是为计划②同会话多 server 时 id 互不冲突。
type PendingKey = (Uuid, String, String);

#[derive(Clone)]
pub struct McpProxy {
    /// claude 持有的 token → session_id 路由(注册即覆盖:工作区稳定
    /// token 在每次 reattach 重指向 hub 新铸的 session_id)。
    routes: Arc<DashMap<String, Uuid>>,
    /// 阻塞中的 POST,等响应按 (session, server, id) 配对。
    pending: Arc<DashMap<PendingKey, oneshot::Sender<String>>>,
    /// agent ws 起来后注入:让 proxy 能向 hub 发帧。
    to_hub: Arc<RwLock<Option<mpsc::Sender<OutFrame>>>>,
    /// 当前有 capable client 在线的会话集合(PtyOpen 标记 / PtyClose
    /// 摘除)。在线 → 帧转发;离线 → 权威 fallback(Task 14)。
    attached: Arc<DashMap<Uuid, ()>>,
    /// 无 client 在线时 tools/list 的权威应答内容:JSON **数组**原文
    /// (来自 [remote_mcp].tools_manifest,缺省 "[]")。数据不是代码,
    /// 不破坏 proxy 的 backend 无关性;dev-browser 的 manifest 内容
    /// 属计划②(决策 D17)。
    static_tools: Arc<String>,
    /// 每 token 一条服务端通知流(claude 的 GET SSE 订阅)。键是
    /// token 而非 session_id:claude 的 GET 长连横跨多次 reattach
    /// (session_id 会换),token 才与 claude 进程同寿。
    notify: Arc<DashMap<String, mpsc::Sender<String>>>,
    /// token → 本会话工作区绝对路径(= claude 的 cwd / fs resolve base)。
    /// 给 `{{CC_WS}}` 落地用。与 `routes` 同寿:register 覆盖、unregister 清。
    workspaces: Arc<DashMap<String, String>>,
}

impl Default for McpProxy {
    fn default() -> Self {
        Self::new()
    }
}

impl McpProxy {
    pub fn new() -> Self {
        Self::with_static_tools("[]".to_string())
    }

    /// 带静态工具表构造(main.rs 启动时从 manifest 文件载入)。
    pub fn with_static_tools(static_tools: String) -> Self {
        Self {
            routes: Arc::new(DashMap::new()),
            pending: Arc::new(DashMap::new()),
            to_hub: Arc::new(RwLock::new(None)),
            attached: Arc::new(DashMap::new()),
            static_tools: Arc::new(static_tools),
            notify: Arc::new(DashMap::new()),
            workspaces: Arc::new(DashMap::new()),
        }
    }

    /// token → session 路由注册(会话打开时)。已知 token 重注册 =
    /// 覆盖改路由(reattach 语义,决策 D12)。`workspace_abs` = 本会话
    /// 工作区绝对路径,供 `{{CC_WS}}` 落地。
    pub fn register(&self, token: String, session_id: Uuid, workspace_abs: String) {
        self.routes.insert(token.clone(), session_id);
        self.workspaces.insert(token, workspace_abs);
    }

    pub fn unregister(&self, token: &str) {
        self.routes.remove(token);
        // 同步清掉该 token 的通知订阅,避免 workspace 删除后残留。
        self.notify.remove(token);
        self.workspaces.remove(token);
    }

    pub fn session_for(&self, token: &str) -> Option<Uuid> {
        self.routes.get(token).map(|r| *r.value())
    }

    /// 取本 token 对应会话的工作区绝对路径(`{{CC_WS}}` 落地用)。
    pub fn workspace_for(&self, token: &str) -> Option<String> {
        self.workspaces.get(token).map(|e| e.value().clone())
    }

    pub async fn set_hub_sender(&self, tx: mpsc::Sender<OutFrame>) {
        *self.to_hub.write().await = Some(tx);
    }

    async fn send_to_hub(&self, frame: OutFrame) {
        // 先 clone 出 sender、放掉读锁再 await send —— 否则并发重连在
        // set_hub_sender 拿写锁会被一个在飞 send 卡住。
        let tx = self.to_hub.read().await.as_ref().cloned();
        if let Some(tx) = tx {
            let _ = tx.send(frame).await;
        }
    }

    /// capable client 已 attach 到该会话(来自 PtyOpen.remote_mcp_capable)。
    pub fn set_attached(&self, session_id: Uuid) {
        self.attached.insert(session_id, ());
    }

    /// 该会话此刻是否有 capable client 在线(转发 vs fallback 的开关)。
    pub fn is_attached(&self, session_id: Uuid) -> bool {
        self.attached.contains_key(&session_id)
    }

    /// client 离线(hub 在每次 client detach——含合盖——都发 PtyClose):
    /// 摘除在线标记并立刻 fail 该会话全部在飞请求(spec 降级④)。
    /// 注意与 RemoteMcpClosed 的分工:那个只 fail_pending、不摘标记
    /// (client 还在线,后端下一帧惰性重生);这个两者都做。
    pub fn detach(&self, session_id: Uuid) {
        self.attached.remove(&session_id);
        self.fail_pending(session_id, "client detached");
        // 工具真实可用性变了:促使 claude 重拉 tools/list(spec 降级③)。
        self.notify_list_changed(session_id);
    }

    /// 订阅某 token 的服务端通知流(GET SSE handler 调用)。同 token
    /// 重订 = 覆盖旧 sender(旧流随之收尾),与 claude 重连语义一致。
    pub fn subscribe(&self, token: &str) -> mpsc::Receiver<String> {
        let (tx, rx) = mpsc::channel(8);
        self.notify.insert(token.to_string(), tx);
        rx
    }

    /// 向路由到 `session_id` 的所有 token 的订阅流推一条
    /// list_changed(attach/detach 时机)。没有订阅流(claude 未开
    /// GET,见 D8 验证项)→ 静默丢弃。
    pub fn notify_list_changed(&self, session_id: Uuid) {
        for e in self.routes.iter() {
            if *e.value() == session_id {
                if let Some(tx) = self.notify.get(e.key()) {
                    let _ = tx.try_send(LIST_CHANGED_FRAME.to_string());
                }
            }
        }
    }

    /// 把 `session_id` 的所有在飞请求以 JSON-RPC 错误(-32002)收尾
    /// (client 拆通道 / 后端崩溃 / client 掉线)。
    pub fn fail_pending(&self, session_id: Uuid, reason: &str) {
        let keys: Vec<PendingKey> = self
            .pending
            .iter()
            .filter(|e| e.key().0 == session_id)
            .map(|e| e.key().clone())
            .collect();
        for key in keys {
            if let Some((k, tx)) = self.pending.remove(&key) {
                let _ = tx.send(jsonrpc_error(&k.2, -32002, reason));
            }
        }
    }

    /// 把一条回程帧配对到阻塞中的 POST。ws.rs 收到
    /// ServerMsg::RemoteMcp 时调用。true = 配对成功;false = 无人等
    /// (如 server 主动通知,计划①丢弃 —— 与 M1-M3 一致)。
    pub fn resolve_response(&self, session_id: Uuid, server: &str, payload: String) -> bool {
        let id = extract_id_key(&payload);
        tracing::debug!(%session_id, server, has_id = id.is_some(), "remote MCP response from hub");
        let Some(id) = id else {
            return false;
        };
        if let Some((_, tx)) = self
            .pending
            .remove(&(session_id, server.to_string(), id))
        {
            return tx.send(payload).is_ok();
        }
        tracing::debug!(%session_id, "remote MCP response had no pending waiter");
        false
    }
}

/// 短档:握手/元数据/垃圾。低于 claude 自身 ~30s 的 MCP 连接超时,
/// 保证我们的 JSON-RPC 错误先于其客户端超时到达。
const REQUEST_TIMEOUT: Duration = Duration::from_secs(25);

/// 中档:tools/call(首调可能触发 client 侧后端拉起,放宽)。
const CALL_TIMEOUT: Duration = Duration::from_secs(120);

/// 长档:阻塞等真人操作的工具调用(分钟级)。
const LONG_CALL_TIMEOUT: Duration = Duration::from_secs(600);

/// 长档工具名单(数据,非浏览器代码):计划②的「请用户接管」工具在此
/// 登记;计划①不提供该工具,名单仅保证机制就绪(决策 D14)。
const LONG_CALL_TOOLS: &[&str] = &["request_handoff"];

/// method(+ 工具名)感知的三档超时选择。
fn timeout_for(body: &str) -> Duration {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(body) else {
        return REQUEST_TIMEOUT;
    };
    if v.get("method").and_then(|m| m.as_str()) != Some("tools/call") {
        return REQUEST_TIMEOUT;
    }
    let tool = v
        .get("params")
        .and_then(|p| p.get("name"))
        .and_then(|n| n.as_str());
    match tool {
        Some(t) if LONG_CALL_TOOLS.contains(&t) => LONG_CALL_TIMEOUT,
        _ => CALL_TIMEOUT,
    }
}

/// 按请求 id(extract_id_key 的规范键,如 `1` 或 `"abc"`)构造
/// JSON-RPC 错误响应体。
fn jsonrpc_error(id_raw: &str, code: i64, message: &str) -> String {
    format!(
        r#"{{"jsonrpc":"2.0","id":{id_raw},"error":{{"code":{code},"message":{msg}}}}}"#,
        msg = serde_json::to_string(message).unwrap_or_else(|_| "\"error\"".to_string())
    )
}

/// 服务端通知:工具集(真实可用性)变了,请重拉 tools/list。
const LIST_CHANGED_FRAME: &str =
    r#"{"jsonrpc":"2.0","method":"notifications/tools/list_changed"}"#;

/// 调用时无可用 client/后端的可执行文案(-32004,决策 D13)。claude
/// 会把它转达给用户;webterm-only 接入恒走此路径。
const NO_CLIENT_MSG: &str = "cc-browser backend is not connected: no cloudcode CLI client \
with a configured MCP backend is attached to this session (the web terminal cannot host \
local tools). Ask the user to open the cloudcode CLI on their local machine, then retry \
after they confirm it is connected.";

/// 无 client 在线时的权威应答(决策 D7/D16):initialize 本地应答
/// (回显请求的 protocolVersion、声明 tools.listChanged),tools/list
/// 用静态表,其余请求 → -32004 可执行文案。
fn fallback_request(id_raw: &str, body: &str, tools_json: &str) -> String {
    let v: Option<serde_json::Value> = serde_json::from_str(body).ok();
    let method = v
        .as_ref()
        .and_then(|x| x.get("method"))
        .and_then(|m| m.as_str())
        .unwrap_or("");
    match method {
        "initialize" => {
            let proto = v
                .as_ref()
                .and_then(|x| x.get("params"))
                .and_then(|p| p.get("protocolVersion"))
                .and_then(|s| s.as_str())
                .unwrap_or("2025-06-18");
            format!(
                r#"{{"jsonrpc":"2.0","id":{id_raw},"result":{{"protocolVersion":{proto_json},"capabilities":{{"tools":{{"listChanged":true}}}},"serverInfo":{{"name":"{CC_BROWSER_SERVER}","version":"{ver}"}}}}}}"#,
                proto_json = serde_json::Value::String(proto.to_string()),
                ver = env!("CARGO_PKG_VERSION"),
            )
        }
        "tools/list" => format!(
            r#"{{"jsonrpc":"2.0","id":{id_raw},"result":{{"tools":{tools_json}}}}}"#
        ),
        _ => jsonrpc_error(id_raw, -32004, NO_CLIENT_MSG),
    }
}

/// 读静态工具表(JSON 数组文件)。读不到 / 不是数组 → 告警 + 空表,
/// 坏 manifest 绝不拖垮 agent 启动。
pub fn load_tools_manifest(path: Option<&std::path::Path>) -> String {
    let Some(path) = path else {
        return "[]".to_string();
    };
    match std::fs::read_to_string(path) {
        Ok(s) => match serde_json::from_str::<Vec<serde_json::Value>>(&s) {
            Ok(_) => s.trim().to_string(),
            Err(e) => {
                tracing::warn!(error = %e, path = %path.display(),
                    "tools_manifest is not a JSON array; using empty list");
                "[]".to_string()
            }
        },
        Err(e) => {
            tracing::warn!(error = %e, path = %path.display(),
                "cannot read tools_manifest; using empty list");
            "[]".to_string()
        }
    }
}

/// token 前 8 字符,日志用(不泄密)。
fn token_prefix(token: &str) -> &str {
    let end = token
        .char_indices()
        .nth(8)
        .map(|(i, _)| i)
        .unwrap_or(token.len());
    &token[..end]
}

/// 取 JSON-RPC `id` 的规范字符串键(数字→`1`,字符串→`"abc"`)。
/// 通知(无 id / id=null)与坏 JSON → None。
fn extract_id_key(body: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(body).ok()?;
    match v.get("id") {
        Some(serde_json::Value::Null) | None => None,
        Some(id) => Some(id.to_string()),
    }
}

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

/// 从先前写盘的 mcp-remote.json 把 token 捞回来:agent 重启后重新采用,
/// 而不是铸新(tmux 里幸存的 claude 内存里还持着旧 token)。
pub fn extract_token_from_config(json: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(json).ok()?;
    let url = v
        .get("mcpServers")?
        .get(CC_BROWSER_SERVER)?
        .get("url")?
        .as_str()?;
    let token = url.rsplit('/').next()?;
    if token.is_empty() {
        return None;
    }
    Some(token.to_string())
}

/// 合法工作区 token:恰 32 个 ASCII hex(`Uuid::new_v4().simple()`
/// 铸造格式)。守住 pty.rs 自愈采用路径,防止被篡改的配置把任意
/// (可猜)token 走私进路由表。
pub fn is_valid_token(token: &str) -> bool {
    token.len() == 32 && token.bytes().all(|b| b.is_ascii_hexdigit())
}

/// 拼装 claude 的每会话 MCP 注入参数(纯函数,单测)。铁律(D11):
/// 进程级 --mcp-config + --strict-mcp-config,即用即弃;绝不写全局
/// ~/.claude.json,绝不使用 `claude mcp add`。
pub fn claude_mcp_args(cfg_path: &std::path::Path, has_web: bool) -> Vec<String> {
    vec![
        "--mcp-config".to_string(),
        cfg_path.to_string_lossy().to_string(),
        "--strict-mcp-config".to_string(),
        "--append-system-prompt".to_string(),
        guidance_prompt(has_web).to_string(),
    ]
}

/// 一次 claude POST 的结果,axum handler 映射成 HTTP 响应。
///
/// 对 JSON-RPC **请求**的传输层故障(token 未注册、超时)以
/// `Response`(JSON-RPC error 对象 @ HTTP 200)返回,绝不裸非 2xx
/// (模块头的 OAuth 误判坑)。
pub enum PostOutcome {
    /// 一个 JSON-RPC 响应体(application/json, 200):client 转回的真
    /// 响应,或本地为传输层故障合成的 JSON-RPC error。
    Response(String),
    /// 通知已受理,无体(202)。
    Accepted,
}

/// 核心 POST 处理,抽出便于单测。
pub async fn handle_post(token: &str, body: String, state: &McpProxy) -> PostOutcome {
    let id = extract_id_key(&body);
    let session = state.session_for(token);
    tracing::debug!(
        token = %token_prefix(token),
        is_request = id.is_some(),
        session = ?session,
        "remote MCP POST"
    );

    match (id, session) {
        // 未知 token 的请求:200 + JSON-RPC error(绝不 404)。
        (Some(id), None) => {
            tracing::warn!(token = %token_prefix(token), "remote MCP POST for unknown token");
            PostOutcome::Response(jsonrpc_error(
                &id,
                -32001,
                "remote MCP session not registered (token unknown or expired)",
            ))
        }
        // 路由已注册但无 capable client 在线(冷启动 / webterm-only /
        // client 掉线后):权威 fallback —— 始终广告(决策 D7)。
        (Some(id), Some(session_id)) if !state.is_attached(session_id) => {
            tracing::debug!(%session_id, "remote MCP request while detached; fallback answering");
            PostOutcome::Response(fallback_request(&id, &body, state.static_tools.as_str()))
        }
        // 已知会话的请求:转发并阻塞等配对响应,method 感知选档。
        (Some(id), Some(session_id)) => {
            let timeout = timeout_for(&body);
            let (tx, rx) = oneshot::channel();
            state
                .pending
                .insert((session_id, CC_BROWSER_SERVER.to_string(), id.clone()), tx);
            state
                .send_to_hub(OutFrame::Text(ClientMsg::RemoteMcp {
                    session_id,
                    server: CC_BROWSER_SERVER.to_string(),
                    payload: body,
                }))
                .await;
            match tokio::time::timeout(timeout, rx).await {
                Ok(Ok(resp)) => {
                    let ws = state.workspace_for(token).unwrap_or_default();
                    PostOutcome::Response(substitute_ws_placeholder(&resp, &ws))
                }
                _ => {
                    state
                        .pending
                        .remove(&(session_id, CC_BROWSER_SERVER.to_string(), id.clone()));
                    tracing::warn!(
                        token = %token_prefix(token),
                        %session_id,
                        timeout_secs = timeout.as_secs(),
                        "remote MCP request timed out awaiting client response"
                    );
                    PostOutcome::Response(jsonrpc_error(
                        &id,
                        -32000,
                        "remote MCP request timed out (the backend may still be starting \
                         on the user's machine — retrying usually succeeds)",
                    ))
                }
            }
        }
        // 未知 token 的通知:没东西可投也没东西可回;202 而非 404。
        (None, None) => {
            tracing::warn!(
                token = %token_prefix(token),
                "remote MCP notification for unknown token; dropping"
            );
            PostOutcome::Accepted
        }
        // 无 client 在线:通知本地吞掉(202),不投递。
        (None, Some(session_id)) if !state.is_attached(session_id) => PostOutcome::Accepted,
        // 已知会话的通知:转发,无响应可等。
        (None, Some(session_id)) => {
            state
                .send_to_hub(OutFrame::Text(ClientMsg::RemoteMcp {
                    session_id,
                    server: CC_BROWSER_SERVER.to_string(),
                    payload: body,
                }))
                .await;
            PostOutcome::Accepted
        }
    }
}

/// 构建 proxy 的 axum 路由(不绑监听)。POST `/mcp/:token` 即阻塞式
/// JSON-RPC 中继;GET 同路径暂回 405(Phase E Task 15 换成 SSE 通知
/// 流);`/healthz` 供探活。绑定与 serve loop 分开,便于调用方先绑
/// (失败即崩启动)再喂监听器进 serve loop(见 `serve_on`)。
fn router(state: McpProxy) -> axum::Router {
    use axum::extract::{Path, State};
    use axum::http::StatusCode;
    use axum::response::IntoResponse;
    use axum::routing::{get, post};

    axum::Router::new()
        .route("/healthz", get(|| async { "ok" }))
        .route(
            "/mcp/:token",
            post(
                |Path(token): Path<String>, State(st): State<McpProxy>, body: String| async move {
                    match handle_post(&token, body, &st).await {
                        PostOutcome::Response(b) => (
                            [(axum::http::header::CONTENT_TYPE, "application/json")],
                            b,
                        )
                            .into_response(),
                        PostOutcome::Accepted => StatusCode::ACCEPTED.into_response(),
                    }
                },
            )
            .get(
                |Path(token): Path<String>, State(st): State<McpProxy>| async move {
                    if st.session_for(&token).is_none() {
                        return StatusCode::METHOD_NOT_ALLOWED.into_response();
                    }
                    let rx = st.subscribe(&token);
                    let stream = futures::stream::unfold(rx, |mut rx| async move {
                        rx.recv().await.map(|m| {
                            (
                                Ok::<_, std::convert::Infallible>(
                                    axum::response::sse::Event::default()
                                        .event("message")
                                        .data(m),
                                ),
                                rx,
                            )
                        })
                    });
                    axum::response::sse::Sse::new(stream)
                        .keep_alive(axum::response::sse::KeepAlive::default())
                        .into_response()
                },
            ),
        )
        .with_state(state)
}

/// 在一个**已绑好**的监听器上跑 serve loop。绑定与 serve loop 分开:
/// 端口冲突是致命且可操作的,要在调用方(agent serve())绑定时以 `?`
/// 崩掉启动,而不是埋在 spawn 里只打一行日志后让 MCP 面永久失活。
/// 这里只剩(罕见的)绑定后 serve-loop 错误,留给调用方 log-and-exit。
pub async fn serve_on(listener: tokio::net::TcpListener, state: McpProxy) -> std::io::Result<()> {
    axum::serve(listener, router(state))
        .await
        .map_err(std::io::Error::other)
}

#[cfg(test)]
mod tests {
    use super::*;

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

    #[tokio::test]
    async fn unknown_token_request_gets_jsonrpc_error_not_404() {
        // 带 id 的请求打到未知 token:绝不裸回非 2xx(OAuth 误判坑),
        // 而是 HTTP 200 + JSON-RPC error(-32001)。
        let state = McpProxy::new();
        let out = handle_post(
            "nope",
            r#"{"jsonrpc":"2.0","id":1,"method":"x"}"#.to_string(),
            &state,
        )
        .await;
        match out {
            PostOutcome::Response(body) => {
                assert!(body.contains("\"error\""), "carries an error object: {body}");
                assert!(body.contains("-32001"), "unknown-token code: {body}");
                assert!(body.contains("\"id\":1"), "keyed to the request id: {body}");
                let v: serde_json::Value = serde_json::from_str(&body).expect("valid JSON");
                assert_eq!(v["jsonrpc"], "2.0");
            }
            _ => panic!("expected a Response carrying a JSON-RPC error"),
        }
    }

    #[tokio::test]
    async fn notification_to_unknown_token_is_accepted_not_404() {
        let state = McpProxy::new();
        let out = handle_post(
            "nope",
            r#"{"jsonrpc":"2.0","method":"notify"}"#.to_string(),
            &state,
        )
        .await;
        assert!(matches!(out, PostOutcome::Accepted));
    }

    #[test]
    fn jsonrpc_error_has_valid_shape() {
        let body = jsonrpc_error("1", -32000, "remote MCP request timed out");
        let v: serde_json::Value = serde_json::from_str(&body).expect("valid JSON");
        assert_eq!(v["jsonrpc"], "2.0");
        assert_eq!(v["id"], 1);
        assert_eq!(v["error"]["code"], -32000);
        assert_eq!(v["error"]["message"], "remote MCP request timed out");

        // 字符串 id 原样穿回(id_raw 已是带引号的规范键)。
        let body = jsonrpc_error("\"abc\"", -32001, "x");
        let v: serde_json::Value = serde_json::from_str(&body).expect("valid JSON");
        assert_eq!(v["id"], "abc");

        // 文案含 JSON 破坏字符也要保持合法(转义)。
        let body = jsonrpc_error("1", -1, "has \"quotes\" and \\ backslash");
        let v: serde_json::Value = serde_json::from_str(&body).expect("valid JSON");
        assert_eq!(v["error"]["message"], "has \"quotes\" and \\ backslash");
    }

    #[tokio::test]
    async fn notification_is_forwarded_and_accepted() {
        let state = McpProxy::new();
        let sid = Uuid::new_v4();
        state.register("t".into(), sid, "/test/ws".to_string());
        state.set_attached(sid);
        let (hub_tx, mut hub_rx) = mpsc::channel(4);
        state.set_hub_sender(hub_tx).await;
        let out = handle_post(
            "t",
            r#"{"jsonrpc":"2.0","method":"notify"}"#.to_string(),
            &state,
        )
        .await;
        assert!(matches!(out, PostOutcome::Accepted));
        // 已转发,且帧上带固定 server 名。
        match hub_rx.recv().await.expect("forwarded") {
            OutFrame::Text(ClientMsg::RemoteMcp { session_id, server, .. }) => {
                assert_eq!(session_id, sid);
                assert_eq!(server, CC_BROWSER_SERVER);
            }
            _ => panic!("expected a RemoteMcp frame"),
        }
    }

    #[tokio::test]
    async fn request_blocks_then_resolves_on_matching_response() {
        let state = McpProxy::new();
        let sid = Uuid::new_v4();
        state.register("t".into(), sid, "/test/ws".to_string());
        state.set_attached(sid);
        let (hub_tx, mut hub_rx) = mpsc::channel(4);
        state.set_hub_sender(hub_tx).await;

        let st2 = state.clone();
        let poster = tokio::spawn(async move {
            handle_post(
                "t",
                r#"{"jsonrpc":"2.0","id":42,"method":"tools/list"}"#.to_string(),
                &st2,
            )
            .await
        });

        match hub_rx.recv().await.expect("forwarded to hub") {
            OutFrame::Text(ClientMsg::RemoteMcp { session_id, .. }) => assert_eq!(session_id, sid),
            _ => panic!("expected a RemoteMcp frame"),
        }

        // 模拟 client 应答经 ws 回来(同 id 配对)。
        let resolved = state.resolve_response(
            sid,
            CC_BROWSER_SERVER,
            r#"{"jsonrpc":"2.0","id":42,"result":{"tools":[]}}"#.to_string(),
        );
        assert!(resolved);

        match poster.await.unwrap() {
            PostOutcome::Response(b) => assert!(b.contains("\"id\":42") && b.contains("tools")),
            _ => panic!("expected a Response"),
        }
    }

    #[tokio::test]
    async fn fail_pending_fails_one_session_and_leaves_other_intact() {
        let state = McpProxy::new();
        let sid_a = Uuid::new_v4();
        let sid_b = Uuid::new_v4();
        let srv = CC_BROWSER_SERVER.to_string();

        let (tx_a1, rx_a1) = oneshot::channel::<String>();
        let (tx_a2, rx_a2) = oneshot::channel::<String>();
        state.pending.insert((sid_a, srv.clone(), "1".to_string()), tx_a1);
        state.pending.insert((sid_a, srv.clone(), "2".to_string()), tx_a2);
        let (tx_b, _rx_b) = oneshot::channel::<String>();
        state.pending.insert((sid_b, srv.clone(), "3".to_string()), tx_b);

        state.fail_pending(sid_a, "backend unavailable");

        let body_a1 = rx_a1.await.expect("a1 resolved");
        assert!(body_a1.contains("-32002"), "expected -32002 in: {body_a1}");
        assert!(body_a1.contains("backend unavailable"), "reason in: {body_a1}");
        let body_a2 = rx_a2.await.expect("a2 resolved");
        assert!(body_a2.contains("-32002"), "expected -32002 in: {body_a2}");

        assert!(!state.pending.contains_key(&(sid_a, srv.clone(), "1".to_string())));
        assert!(!state.pending.contains_key(&(sid_a, srv.clone(), "2".to_string())));
        assert!(state.pending.contains_key(&(sid_b, srv, "3".to_string())));
    }

    #[test]
    fn timeout_for_is_method_and_tool_aware() {
        // 长档:LONG_CALL_TOOLS 名单内的 tools/call(②的人工接管)。
        assert_eq!(
            timeout_for(
                r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"request_handoff","arguments":{"reason":"login"}}}"#
            ),
            LONG_CALL_TIMEOUT
        );
        assert_eq!(LONG_CALL_TIMEOUT, Duration::from_secs(600));
        // 中档:其余 tools/call(首调可能触发后端拉起)。
        assert_eq!(
            timeout_for(
                r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"browser_navigate"}}"#
            ),
            CALL_TIMEOUT
        );
        assert_eq!(
            timeout_for(r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{}}"#),
            CALL_TIMEOUT
        );
        assert_eq!(CALL_TIMEOUT, Duration::from_secs(120));
        // 短档:握手/元数据/垃圾(低于 claude 自身 ~30s 连接超时)。
        assert_eq!(
            timeout_for(r#"{"jsonrpc":"2.0","id":1,"method":"initialize"}"#),
            REQUEST_TIMEOUT
        );
        assert_eq!(
            timeout_for(r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#),
            REQUEST_TIMEOUT
        );
        assert_eq!(timeout_for("not json"), REQUEST_TIMEOUT);
        assert_eq!(REQUEST_TIMEOUT, Duration::from_secs(25));
    }

    #[test]
    fn config_has_http_url_with_token_under_cc_browser() {
        let s = mcp_config_json(7110, "abc123", None);
        assert!(s.contains("\"cc-browser\""));
        assert!(s.contains("\"type\":\"http\""));
        assert!(s.contains("http://127.0.0.1:7110/mcp/abc123"));
        let _: serde_json::Value = serde_json::from_str(&s).unwrap();
    }

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

    #[test]
    fn claude_args_carry_strict_flag_and_guidance() {
        let args = claude_mcp_args(std::path::Path::new("/ws/.cloudcode/mcp-remote.json"), true);
        assert_eq!(
            args,
            vec![
                "--mcp-config".to_string(),
                "/ws/.cloudcode/mcp-remote.json".to_string(),
                "--strict-mcp-config".to_string(),
                "--append-system-prompt".to_string(),
                GUIDANCE_PROMPT.to_string(),
            ]
        );
        // 引导文案通用化:点名 server,不写死任何工具名(决策 D11)。
        assert!(GUIDANCE_PROMPT.contains("cc-browser"));
        assert!(!GUIDANCE_PROMPT.contains("browser_navigate"));
    }

    #[test]
    fn claude_args_without_web_embed_single_server_prompt() {
        // has_web=false ⇒ 注入里只有 cc-browser,引导也必须只提 cc-browser,
        // 绝不让 claude 去用不存在的 `web`(unknown-server 陷阱)。
        let args = claude_mcp_args(std::path::Path::new("/ws/.cloudcode/mcp-remote.json"), false);
        assert_eq!(
            args,
            vec![
                "--mcp-config".to_string(),
                "/ws/.cloudcode/mcp-remote.json".to_string(),
                "--strict-mcp-config".to_string(),
                "--append-system-prompt".to_string(),
                GUIDANCE_PROMPT_CC_BROWSER_ONLY.to_string(),
            ]
        );
    }

    #[test]
    fn guidance_prompt_selects_by_injected_servers() {
        // 有 web:双后端文案(点名两 server)。
        let dual = guidance_prompt(true);
        assert!(dual.contains("Two browser"));
        assert!(dual.contains("`web`"));
        assert!(dual.contains("`cc-browser`"));
        // 无 web:单 cc-browser 文案 —— 提 cc-browser,但绝不提 `web`/两 server。
        let single = guidance_prompt(false);
        assert!(single.contains("`cc-browser`"));
        assert!(!single.contains("`web`"));
        assert!(!single.contains("Two browser"));
    }

    #[test]
    fn parse_command_splits_empty_single_and_multi() {
        // 空串 / 全空白 → None。
        assert_eq!(parse_command(""), None);
        assert_eq!(parse_command("   "), None);
        // 单 token → 程序名 + 空 argv。
        assert_eq!(parse_command("npx"), Some(("npx".to_string(), vec![])));
        // 多 token → 程序名 + 余下按空白拆分(含折叠多空白)。
        assert_eq!(
            parse_command("npx  -y  pkg --flag"),
            Some((
                "npx".to_string(),
                vec!["-y".to_string(), "pkg".to_string(), "--flag".to_string()]
            ))
        );
    }

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

    #[test]
    fn extract_token_roundtrip_and_garbage() {
        let json = mcp_config_json(7110, "abc123", None);
        assert_eq!(extract_token_from_config(&json), Some("abc123".to_string()));
        assert_eq!(extract_token_from_config("not json at all"), None);
        assert_eq!(extract_token_from_config(""), None);
        assert_eq!(extract_token_from_config(r#"{"other":"value"}"#), None);
        assert_eq!(
            extract_token_from_config(r#"{"mcpServers":{"cc-browser":{"type":"http"}}}"#),
            None
        );
        assert_eq!(
            extract_token_from_config(
                r#"{"mcpServers":{"cc-browser":{"url":"http://127.0.0.1:7110/mcp/"}}}"#
            ),
            None
        );
    }

    #[test]
    fn token_validation_accepts_minted_rejects_malformed() {
        let minted = Uuid::new_v4().simple().to_string();
        assert!(is_valid_token(&minted));
        assert!(is_valid_token("ABCDEF0123456789abcdef0123456789"));
        assert!(!is_valid_token(""));
        assert!(!is_valid_token("abc123"));
        assert!(!is_valid_token(&"a".repeat(31)));
        assert!(!is_valid_token(&"a".repeat(33)));
        assert!(!is_valid_token("g".repeat(32).as_str()));
        assert!(!is_valid_token("../../../../etc/passwd00000000000"));
    }

    #[test]
    fn register_overwrite_unregister_routing() {
        let st = McpProxy::new();
        let sid1 = Uuid::new_v4();
        let sid2 = Uuid::new_v4();
        let tok = "stable-workspace-token".to_string();
        st.register(tok.clone(), sid1, "/test/ws".to_string());
        assert_eq!(st.session_for(&tok), Some(sid1));
        // reattach:同 token 对 hub 新铸的 session_id 重注册 = 覆盖改路由。
        st.register(tok.clone(), sid2, "/test/ws2".to_string());
        assert_eq!(st.session_for(&tok), Some(sid2));
        st.unregister(&tok);
        assert_eq!(st.session_for(&tok), None);
    }

    #[test]
    fn id_key_distinguishes_number_and_string() {
        assert_eq!(extract_id_key(r#"{"id":1}"#), Some("1".to_string()));
        assert_eq!(extract_id_key(r#"{"id":"a"}"#), Some("\"a\"".to_string()));
        assert_eq!(extract_id_key(r#"{"method":"x"}"#), None);
        assert_eq!(extract_id_key(r#"{"id":null}"#), None);
    }

    /// 绑 :0 拿一个空闲端口再放掉给 serve 重绑。存在极小 TOCTOU 窗口,
    /// 单测试进程内可忽略,远比写死端口稳。
    pub(super) fn free_port() -> u16 {
        let l = std::net::TcpListener::bind("127.0.0.1:0").expect("bind :0");
        l.local_addr().expect("local_addr").port()
    }

    /// 轮询 /healthz 直到 serve 绑定完成(连接拒绝则重试)。
    pub(super) async fn wait_healthz(client: &reqwest::Client, base: &str) -> String {
        for _ in 0..50 {
            match client.get(format!("{base}/healthz")).send().await {
                Ok(resp) => return resp.text().await.unwrap(),
                Err(_) => tokio::time::sleep(Duration::from_millis(20)).await,
            }
        }
        panic!("endpoint never came up on {base}");
    }

    /// 唯一走真 TCP + axum 路由的测试(其余直接调 handle_post)。
    #[tokio::test]
    async fn real_http_post_roundtrips_via_endpoint() {
        let state = McpProxy::new();
        let sid = Uuid::new_v4();
        let token = "tok-e2e";
        state.register(token.into(), sid, "/test/ws".to_string());
        state.set_attached(sid);

        let (hub_tx, mut hub_rx) = mpsc::channel(4);
        state.set_hub_sender(hub_tx).await;

        // 绑自己的 127.0.0.1 监听器再喂 serve_on —— 镜像生产里 agent
        // serve() 先绑(失败即崩启动)、再 spawn serve loop 的形状。
        let port = free_port();
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", port))
            .await
            .expect("bind 127.0.0.1 test listener");
        let serve_state = state.clone();
        tokio::spawn(async move {
            let _ = serve_on(listener, serve_state).await;
        });

        // 模拟 client+hub:取走转发帧,按 id 喂回一条合成响应。
        let resp_state = state.clone();
        tokio::spawn(async move {
            if let Some(OutFrame::Text(ClientMsg::RemoteMcp { session_id, server, payload })) =
                hub_rx.recv().await
            {
                assert_eq!(session_id, sid);
                let id = extract_id_key(&payload).expect("request had an id");
                let body = format!(
                    r#"{{"jsonrpc":"2.0","id":{id},"result":{{"tools":[{{"name":"echo"}}]}}}}"#
                );
                assert!(resp_state.resolve_response(session_id, &server, body));
            }
        });

        let client = reqwest::Client::new();
        let base = format!("http://127.0.0.1:{port}");
        assert_eq!(wait_healthz(&client, &base).await, "ok");

        let resp = client
            .post(format!("{base}/mcp/{token}"))
            .body(r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#)
            .send()
            .await
            .expect("POST to endpoint");
        assert_eq!(resp.status(), reqwest::StatusCode::OK);
        let text = resp.text().await.unwrap();
        assert!(text.contains("\"id\":1"), "response keeps the request id: {text}");
        assert!(text.contains("echo"), "carries the simulated result: {text}");

        // 未知 token 的请求:HTTP 200 + JSON-RPC error,绝不 404。
        let unknown = client
            .post(format!("{base}/mcp/does-not-exist"))
            .body(r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#)
            .send()
            .await
            .expect("POST unknown token");
        assert_eq!(unknown.status(), reqwest::StatusCode::OK);
        let body = unknown.text().await.unwrap();
        assert!(body.contains("\"error\""), "JSON-RPC error body: {body}");
        assert!(body.contains("-32001"), "unknown-token code: {body}");
    }

    /// 测试环境探测:PATH 上有无 node(echo 桩需要)。无则 skip。
    fn node_available() -> bool {
        let Some(path) = std::env::var_os("PATH") else {
            return false;
        };
        std::env::split_paths(&path).any(|d| d.join("node").is_file())
    }

    /// 端到端 loopback:真 axum HTTP 端点 ← reqwest POST tools/call;
    /// 「hub+client」由测试体内联扮演 —— 从 to_hub 通道取
    /// ClientMsg::RemoteMcp 帧,**原文**喂给真 node echo 桩,把桩的
    /// 应答经 resolve_response 配对回去。覆盖:HTTP 入口、id 配对、
    /// 帧封装、与真实 MCP-over-stdio 后端的字节级互通。
    #[tokio::test]
    async fn loopback_tools_call_roundtrips_through_pipe_and_echo_backend() {
        if !node_available() {
            return; // 无 node → skip
        }
        let state = McpProxy::new();
        let sid = Uuid::new_v4();
        let token = "tok-loopback";
        state.register(token.into(), sid, "/test/ws".to_string());
        state.set_attached(sid);
        let (hub_tx, mut hub_rx) = mpsc::channel(4);
        state.set_hub_sender(hub_tx).await;

        // 绑自己的 127.0.0.1 监听器再喂 serve_on(Task 10 移除了
        // serve(state, port),绑定上提给调用方)。镜像
        // real_http_post_roundtrips_via_endpoint 的端口发现 + serve 形状。
        let port = free_port();
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", port))
            .await
            .expect("bind 127.0.0.1 test listener");
        let serve_state = state.clone();
        tokio::spawn(async move {
            let _ = serve_on(listener, serve_state).await;
        });

        // 内联 hub+client:隧道帧 → echo 桩 stdin;桩 stdout → 配对回包。
        let resp_state = state.clone();
        tokio::spawn(async move {
            use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
            let fixture =
                concat!(env!("CARGO_MANIFEST_DIR"), "/../../test-fixtures/echo-mcp.mjs");
            let mut child = tokio::process::Command::new("node")
                .arg(fixture)
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::piped())
                .kill_on_drop(true)
                .spawn()
                .expect("spawn echo backend");
            let mut stdin = child.stdin.take().expect("stdin");
            let stdout = child.stdout.take().expect("stdout");
            let mut lines = tokio::io::BufReader::new(stdout).lines();
            while let Some(OutFrame::Text(ClientMsg::RemoteMcp {
                session_id,
                server,
                payload,
            })) = hub_rx.recv().await
            {
                assert_eq!(server, CC_BROWSER_SERVER);
                stdin.write_all(payload.as_bytes()).await.unwrap();
                stdin.write_all(b"\n").await.unwrap();
                stdin.flush().await.unwrap();
                if let Ok(Some(line)) = lines.next_line().await {
                    resp_state.resolve_response(session_id, &server, line);
                }
            }
        });

        let client = reqwest::Client::new();
        let base = format!("http://127.0.0.1:{port}");
        assert_eq!(wait_healthz(&client, &base).await, "ok");

        let resp = client
            .post(format!("{base}/mcp/{token}"))
            .body(
                r#"{"jsonrpc":"2.0","id":11,"method":"tools/call","params":{"name":"echo","arguments":{"text":"pipe"}}}"#,
            )
            .send()
            .await
            .expect("POST tools/call");
        assert_eq!(resp.status(), reqwest::StatusCode::OK);
        let text = resp.text().await.unwrap();
        assert!(text.contains(r#""id":11"#), "response keeps id: {text}");
        assert!(text.contains("echo: pipe"), "echo result came back: {text}");
    }

    #[test]
    fn attach_detach_lifecycle() {
        let state = McpProxy::new();
        let sid = Uuid::new_v4();
        assert!(!state.is_attached(sid));
        state.set_attached(sid);
        assert!(state.is_attached(sid));
        state.detach(sid);
        assert!(!state.is_attached(sid));
    }

    #[tokio::test]
    async fn detach_fails_pending_requests() {
        // spec 降级④:client 掉线瞬间,在飞请求立刻以 JSON-RPC 错误
        // 收尾,绝不等满超时档。
        let state = McpProxy::new();
        let sid = Uuid::new_v4();
        state.set_attached(sid);
        let (tx, rx) = oneshot::channel::<String>();
        state
            .pending
            .insert((sid, CC_BROWSER_SERVER.to_string(), "7".to_string()), tx);
        state.detach(sid);
        let body = rx.await.expect("failed fast");
        assert!(body.contains("-32002"), "fail_pending error code: {body}");
        assert!(body.contains("client detached"), "reason: {body}");
        assert!(!state.is_attached(sid));
    }

    #[tokio::test]
    async fn detached_initialize_is_answered_authoritatively() {
        // 冷启动(注册了路由、无 client 在线):initialize 由 proxy 权威
        // 应答 —— 回显请求的 protocolVersion、声明 tools.listChanged,
        // claude 才能完成握手并在之后消费 list_changed(决策 D16)。
        let state = McpProxy::with_static_tools(
            r#"[{"name":"echo","description":"d","inputSchema":{"type":"object"}}]"#.to_string(),
        );
        let sid = Uuid::new_v4();
        state.register("t".into(), sid, "/test/ws".to_string());
        let out = handle_post(
            "t",
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"claude","version":"1"}}}"#
                .to_string(),
            &state,
        )
        .await;
        match out {
            PostOutcome::Response(b) => {
                let v: serde_json::Value = serde_json::from_str(&b).unwrap();
                assert_eq!(v["id"], 1);
                assert_eq!(
                    v["result"]["protocolVersion"], "2024-11-05",
                    "echoes the requested protocolVersion"
                );
                assert_eq!(v["result"]["capabilities"]["tools"]["listChanged"], true);
                assert_eq!(v["result"]["serverInfo"]["name"], "cc-browser");
            }
            _ => panic!("expected an authoritative response"),
        }
    }

    #[tokio::test]
    async fn detached_tools_list_serves_static_manifest_or_empty() {
        let state = McpProxy::with_static_tools(r#"[{"name":"echo"}]"#.to_string());
        let sid = Uuid::new_v4();
        state.register("t".into(), sid, "/test/ws".to_string());
        match handle_post(
            "t",
            r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#.to_string(),
            &state,
        )
        .await
        {
            PostOutcome::Response(b) => {
                let v: serde_json::Value = serde_json::from_str(&b).unwrap();
                assert_eq!(v["result"]["tools"][0]["name"], "echo");
            }
            _ => panic!("expected a Response"),
        }
        // 缺省构造 = 空表(始终广告:server 健在、工具暂无),不是错误。
        let bare = McpProxy::new();
        bare.register("t".into(), Uuid::new_v4(), "/test/ws".to_string());
        match handle_post(
            "t",
            r#"{"jsonrpc":"2.0","id":3,"method":"tools/list"}"#.to_string(),
            &bare,
        )
        .await
        {
            PostOutcome::Response(b) => {
                let v: serde_json::Value = serde_json::from_str(&b).unwrap();
                assert_eq!(v["result"]["tools"], serde_json::json!([]));
            }
            _ => panic!("expected a Response"),
        }
    }

    #[tokio::test]
    async fn detached_tools_call_gets_actionable_error() {
        // spec 降级②:无 client 调用 → JSON-RPC 错误(非传输失败),
        // 文案可执行 —— claude 把"打开 cloudcode CLI"转达给用户;
        // webterm-only 接入恒走此路径。
        let state = McpProxy::new();
        let sid = Uuid::new_v4();
        state.register("t".into(), sid, "/test/ws".to_string());
        let out = handle_post(
            "t",
            r#"{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"echo","arguments":{}}}"#
                .to_string(),
            &state,
        )
        .await;
        match out {
            PostOutcome::Response(b) => {
                let v: serde_json::Value = serde_json::from_str(&b).unwrap();
                assert_eq!(v["error"]["code"], -32004);
                let msg = v["error"]["message"].as_str().unwrap();
                assert!(msg.contains("cloudcode CLI"), "actionable wording: {msg}");
            }
            _ => panic!("expected a JSON-RPC error, not a transport failure"),
        }
    }

    #[tokio::test]
    async fn detached_notification_is_swallowed_not_forwarded() {
        let state = McpProxy::new();
        let sid = Uuid::new_v4();
        state.register("t".into(), sid, "/test/ws".to_string());
        let (hub_tx, mut hub_rx) = mpsc::channel(4);
        state.set_hub_sender(hub_tx).await;
        let out = handle_post(
            "t",
            r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#.to_string(),
            &state,
        )
        .await;
        assert!(matches!(out, PostOutcome::Accepted));
        assert!(hub_rx.try_recv().is_err(), "nothing may be forwarded while detached");
    }

    #[tokio::test]
    async fn attach_detach_pushes_list_changed_to_subscribed_stream() {
        let state = McpProxy::new();
        let sid = Uuid::new_v4();
        state.register("tok-n".into(), sid, "/test/ws".to_string());
        let mut rx = state.subscribe("tok-n");

        // 模拟 client 上线(pty.rs 在 PtyOpen 时:set_attached + notify)。
        state.set_attached(sid);
        state.notify_list_changed(sid);
        let frame = rx.recv().await.expect("notification pushed");
        assert_eq!(frame, LIST_CHANGED_FRAME);
        let v: serde_json::Value = serde_json::from_str(&frame).unwrap();
        assert_eq!(v["method"], "notifications/tools/list_changed");

        // 模拟 client 掉线:detach 自带 list_changed。
        state.detach(sid);
        let frame = rx.recv().await.expect("notification on detach");
        assert_eq!(frame, LIST_CHANGED_FRAME);
    }

    #[tokio::test]
    async fn notify_without_subscriber_is_noop() {
        // claude 不开 GET 流(D8 的未验证假设不成立时)→ 通知静默丢弃,
        // 不 panic、不阻塞、无副作用。
        let state = McpProxy::new();
        let sid = Uuid::new_v4();
        state.register("tok-x".into(), sid, "/test/ws".to_string());
        state.notify_list_changed(sid);
    }

    #[tokio::test]
    async fn sse_get_stream_delivers_notification_over_real_http() {
        let state = McpProxy::new();
        let sid = Uuid::new_v4();
        let token = "tok-sse";
        state.register(token.into(), sid, "/test/ws".to_string());
        // 镜像本模块其余真 TCP 测试:先绑 127.0.0.1 监听器(Task 10 移除了
        // serve(state, port),绑定上提给调用方),再喂 serve_on。
        let port = free_port();
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", port))
            .await
            .expect("bind 127.0.0.1 test listener");
        let serve_state = state.clone();
        tokio::spawn(async move {
            let _ = serve_on(listener, serve_state).await;
        });
        let client = reqwest::Client::new();
        let base = format!("http://127.0.0.1:{port}");
        assert_eq!(wait_healthz(&client, &base).await, "ok");

        let resp = client
            .get(format!("{base}/mcp/{token}"))
            .send()
            .await
            .expect("GET sse");
        assert_eq!(resp.status(), reqwest::StatusCode::OK);
        // GET handler 同步完成 subscribe,但经网络有传播窗:轮询直到
        // 订阅出现再触发通知。
        for _ in 0..50 {
            if state.notify.contains_key(token) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(state.notify.contains_key(token), "GET must register a subscription");
        state.notify_list_changed(sid);

        let mut stream = resp;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            let item = tokio::time::timeout_at(deadline, stream.chunk())
                .await
                .expect("sse chunk within 5s")
                .expect("chunk read ok");
            let Some(bytes) = item else { panic!("sse stream ended early") };
            let text = String::from_utf8_lossy(&bytes).to_string();
            if text.contains("tools/list_changed") {
                break; // SSE 事件携带通知帧,到达即过
            }
            // keepalive 注释行等:继续读。
        }

        // 未注册 token 的 GET:405。
        let nope = client
            .get(format!("{base}/mcp/unknown"))
            .send()
            .await
            .expect("GET unknown");
        assert_eq!(nope.status(), reqwest::StatusCode::METHOD_NOT_ALLOWED);
    }

    #[test]
    fn tools_manifest_loading_tolerates_garbage() {
        assert_eq!(load_tools_manifest(None), "[]");
        let dir = tempfile::tempdir().unwrap();
        let good = dir.path().join("good.json");
        std::fs::write(&good, r#"[{"name":"t"}]"#).unwrap();
        assert_eq!(load_tools_manifest(Some(&good)), r#"[{"name":"t"}]"#);
        let bad = dir.path().join("bad.json");
        std::fs::write(&bad, r#"{"not":"array"}"#).unwrap();
        assert_eq!(load_tools_manifest(Some(&bad)), "[]");
        assert_eq!(load_tools_manifest(Some(&dir.path().join("missing.json"))), "[]");
    }

    #[test]
    fn ws_placeholder_substitution() {
        // 典型:响应里的占位符路径被落地成绝对路径
        let payload = r#"{"jsonrpc":"2.0","id":7,"result":{"content":[{"type":"text","text":"- [Screenshot]({{CC_WS}}/.cloudcode/browser-artifacts/shot.png)"}]}}"#;
        let out = substitute_ws_placeholder(payload, "/ws/acct/work");
        assert!(out.contains("/ws/acct/work/.cloudcode/browser-artifacts/shot.png"));
        assert!(!out.contains("{{CC_WS}}"));

        // 多次出现都替换
        let two = "{{CC_WS}}/a {{CC_WS}}/b";
        assert_eq!(substitute_ws_placeholder(two, "/X"), "/X/a /X/b");

        // 无占位符:原样返回(no-op)
        let plain = r#"{"result":"hello"}"#;
        assert_eq!(substitute_ws_placeholder(plain, "/X"), plain);

        // ws_abs 为空:不替换(防止把占位符替成空导致烂路径)
        assert_eq!(substitute_ws_placeholder(two, ""), two);
    }

    #[test]
    fn register_stores_workspace_and_lookup() {
        let proxy = McpProxy::new();
        let token = "a".repeat(32);
        let sid = Uuid::new_v4();
        proxy.register(token.clone(), sid, "/ws/acct/work".to_string());
        assert_eq!(proxy.workspace_for(&token).as_deref(), Some("/ws/acct/work"));
        // 覆盖式重注册(reattach 语义)更新 workspace
        let sid2 = Uuid::new_v4();
        proxy.register(token.clone(), sid2, "/ws/acct/work2".to_string());
        assert_eq!(proxy.workspace_for(&token).as_deref(), Some("/ws/acct/work2"));
        // unregister 清除
        proxy.unregister(&token);
        assert_eq!(proxy.workspace_for(&token), None);
    }
}
