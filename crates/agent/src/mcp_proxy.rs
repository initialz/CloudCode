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

/// 注入给 claude 的通用引导(决策 D11):说明 cc-browser 的工具在用户
/// 本地机器执行、收到「未连接」错误时如何转告用户。不写死任何工具名
/// —— 工具表由后端运行时决定。
pub const GUIDANCE_PROMPT: &str = "The `cc-browser` MCP server provides tools (such as web \
browsing) that run on the USER'S LOCAL machine through the cloudcode CLI — not on this host. \
Prefer these tools when the user asks for anything involving their local browser or web pages. \
If a cc-browser tool call returns a 'not connected' style error, relay its instructions to the \
user (they need to open the cloudcode CLI on their local machine), then retry after they \
confirm.";

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
}

impl Default for McpProxy {
    fn default() -> Self {
        Self::new()
    }
}

impl McpProxy {
    pub fn new() -> Self {
        Self {
            routes: Arc::new(DashMap::new()),
            pending: Arc::new(DashMap::new()),
            to_hub: Arc::new(RwLock::new(None)),
            attached: Arc::new(DashMap::new()),
        }
    }

    /// token → session 路由注册(会话打开时)。已知 token 重注册 =
    /// 覆盖改路由(reattach 语义,决策 D12)。
    pub fn register(&self, token: String, session_id: Uuid) {
        self.routes.insert(token, session_id);
    }

    pub fn unregister(&self, token: &str) {
        self.routes.remove(token);
    }

    pub fn session_for(&self, token: &str) -> Option<Uuid> {
        self.routes.get(token).map(|r| *r.value())
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

/// 生成 claude 要加载的 `--mcp-config` JSON(Streamable HTTP 指向本
/// proxy)。server 名固定 cc-browser。
pub fn mcp_config_json(port: u16, token: &str) -> String {
    format!(
        r#"{{"mcpServers":{{"{CC_BROWSER_SERVER}":{{"type":"http","url":"http://127.0.0.1:{port}/mcp/{token}"}}}}}}"#
    )
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
pub fn claude_mcp_args(cfg_path: &std::path::Path) -> Vec<String> {
    vec![
        "--mcp-config".to_string(),
        cfg_path.to_string_lossy().to_string(),
        "--strict-mcp-config".to_string(),
        "--append-system-prompt".to_string(),
        GUIDANCE_PROMPT.to_string(),
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
                Ok(Ok(resp)) => PostOutcome::Response(resp),
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
            .get(|| async { StatusCode::METHOD_NOT_ALLOWED }),
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
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))
}

#[cfg(test)]
mod tests {
    use super::*;

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
        state.register("t".into(), sid);
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
        state.register("t".into(), sid);
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
        let s = mcp_config_json(7110, "abc123");
        assert!(s.contains("\"cc-browser\""));
        assert!(s.contains("\"type\":\"http\""));
        assert!(s.contains("http://127.0.0.1:7110/mcp/abc123"));
        let _: serde_json::Value = serde_json::from_str(&s).unwrap();
    }

    #[test]
    fn claude_args_carry_strict_flag_and_guidance() {
        let args = claude_mcp_args(std::path::Path::new("/ws/.cloudcode/mcp-remote.json"));
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
    fn extract_token_roundtrip_and_garbage() {
        let json = mcp_config_json(7110, "abc123");
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
        st.register(tok.clone(), sid1);
        assert_eq!(st.session_for(&tok), Some(sid1));
        // reattach:同 token 对 hub 新铸的 session_id 重注册 = 覆盖改路由。
        st.register(tok.clone(), sid2);
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
        state.register(token.into(), sid);

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
        state.register(token.into(), sid);
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
}
