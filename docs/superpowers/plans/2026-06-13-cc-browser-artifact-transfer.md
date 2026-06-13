# cc-browser 文件产物回传 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 让 `cc-browser` 后端(playwright-mcp 跑在 client)产生的文件产物(截图/PDF)经现成 FsWrite 管道镜像回 agent workspace,并把 MCP 响应里的 client 本地路径重写成 agent 可读的绝对路径,使 agent 上的 claude 能用 Read 直接看见。

**Architecture:** client 端在 MCP 响应里检测被 markdown 链接引用、且落在已知 staging 目录(= 后端 spawn CWD == `--output-dir`)的文件,在 spawn 出去的任务里用 `FsWriteInit/FsWriteChunk` 上传到 `.cloudcode/browser-artifacts/`,拿回 `final_name` 后把链接 target 重写成占位符 `{{CC_WS}}/.cloudcode/browser-artifacts/<final_name>`;agent 的 mcp_proxy 在把响应交给 claude 前按 token 查本会话工作区绝对路径,把 `{{CC_WS}}` 落地成绝对路径。零协议/帧改动(复用既有 FsWrite 帧)。

**Tech Stack:** Rust(tokio + axum + serde_json + dashmap + base64),复用 `crates/client/src/relay.rs` 的 `spawn_upload`/`upload_one_file` + `pending_uploads` 路由;`crates/agent/src/fs.rs` 的 `write_init/write_chunk/resolve_safe`(无需改);`@playwright/mcp@0.0.76`。

**Spec:** `docs/superpowers/specs/2026-06-13-cc-browser-artifact-transfer-design.md`

---

## File Structure

| 文件 | 职责 | 改动 |
|------|------|------|
| `crates/agent/src/mcp_proxy.rs` | `{{CC_WS}}` 落地 + token→workspace 映射 | 加纯函数 `substitute_ws_placeholder`、`WS_PLACEHOLDER` const、`workspaces` map、`register` 多一参、`workspace_for`、`handle_post` 调用 |
| `crates/agent/src/pty.rs` | 铸 token 时把本会话工作区绝对路径(`cwd`)登记进 proxy | 改 `register(...)` 调用站点 |
| `crates/client/src/mcp_host.rs` | staging 目录、后端 `--output-dir`、spawn CWD、检测/重写纯函数、`WS_PLACEHOLDER` const | 加 `artifact_dir()`、`ARTIFACT_DIR_REL`/`WS_PLACEHOLDER` const、`--output-dir` 注入、CWD 线程化、`detect_artifacts`、`rewrite_artifact_links` |
| `crates/client/src/relay.rs` | 在 host 响应臂检测产物 → spawn 转移任务;大小上限 | `upload_one_file` 加 `dest_dir` 参、`spawn_artifact_transfer`、改 `host_out_rx` 臂、`ARTIFACT_MAX_BYTES` |

**Lockstep:** `WS_PLACEHOLDER = "{{CC_WS}}"` 在 `mcp_proxy.rs`(写入方)与 `mcp_host.rs`(读取/落地方对端)各定义一份,注释互指,改一处必改另一处(与 `PLAYWRIGHT_MCP_PKG` 同惯例)。

**为何 `{{CC_WS}}` 用可打印 ASCII 而非控制字符:** 占位符要嵌进响应的 **JSON 字符串值**里;若用 `\u{1}` 等控制字符会让 JSON 非法(未转义控制字符)。`{{CC_WS}}` 是合法 JSON 字符串内容,且与真实页面内容碰撞概率极低。

---

## Task 1: agent — `{{CC_WS}}` 落地纯函数

**Files:**
- Modify: `crates/agent/src/mcp_proxy.rs`(在 consts 区加 `WS_PLACEHOLDER`,在文件内加纯函数 + 单测)

- [ ] **Step 1: Write the failing test**

加到 `crates/agent/src/mcp_proxy.rs` 的 `#[cfg(test)] mod tests` 里:

```rust
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
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p agent ws_placeholder_substitution`
Expected: FAIL —— `cannot find function substitute_ws_placeholder`

- [ ] **Step 3: Write minimal implementation**

在 `crates/agent/src/mcp_proxy.rs` 顶部 const 区(`CC_BROWSER_SERVER` 附近)加:

```rust
/// 占位符:client 端把产物路径重写成 `{{CC_WS}}/.cloudcode/browser-artifacts/<name>`,
/// 本 proxy 在交给 claude 前替换成本会话工作区绝对路径。
/// LOCKSTEP: 与 client `crates/client/src/mcp_host.rs` 的 `WS_PLACEHOLDER` 必须一致。
pub const WS_PLACEHOLDER: &str = "{{CC_WS}}";
```

在文件内(consts 之后、随便一处顶层)加纯函数:

```rust
/// 把响应文本里的 `{{CC_WS}}` 占位符替换成本会话工作区绝对路径。
/// `ws_abs` 为空时不替换(无映射时的安全 no-op)。
pub fn substitute_ws_placeholder(payload: &str, ws_abs: &str) -> String {
    if ws_abs.is_empty() || !payload.contains(WS_PLACEHOLDER) {
        return payload.to_string();
    }
    payload.replace(WS_PLACEHOLDER, ws_abs)
}
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p agent ws_placeholder_substitution`
Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add crates/agent/src/mcp_proxy.rs
git commit -m "agent: pure {{CC_WS}} placeholder substitution helper"
```

---

## Task 2: agent — token→workspace 映射 + register 扩展 + handle_post 接线

**Files:**
- Modify: `crates/agent/src/mcp_proxy.rs`(`McpProxy` 加 `workspaces` 字段、`register` 多一参、`workspace_for`、`handle_post` 调用 `substitute_ws_placeholder`)
- Modify: `crates/agent/src/pty.rs`(register 调用站点传 `cwd`)

- [ ] **Step 1: Write the failing test**

加到 `mcp_proxy.rs` tests:

```rust
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
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p agent register_stores_workspace_and_lookup`
Expected: FAIL —— `register` takes 2 args, no `workspace_for`

- [ ] **Step 3: Write minimal implementation**

在 `McpProxy` 结构体(`mcp_proxy.rs:143` 起)加字段:

```rust
    /// token → 本会话工作区绝对路径(= claude 的 cwd / fs resolve base)。
    /// 给 `{{CC_WS}}` 落地用。与 `routes` 同寿:register 覆盖、unregister 清。
    workspaces: Arc<DashMap<String, String>>,
```

在 `with_static_tools`(`mcp_proxy.rs:177`)初始化:

```rust
            workspaces: Arc::new(DashMap::new()),
```

改 `register`(`mcp_proxy.rs:191`):

```rust
    /// token → session 路由注册(会话打开时)。已知 token 重注册 =
    /// 覆盖改路由(reattach 语义,决策 D12)。`workspace_abs` = 本会话
    /// 工作区绝对路径,供 `{{CC_WS}}` 落地。
    pub fn register(&self, token: String, session_id: Uuid, workspace_abs: String) {
        self.routes.insert(token.clone(), session_id);
        self.workspaces.insert(token, workspace_abs);
    }
```

在 `unregister`(`mcp_proxy.rs:195`)加一行:

```rust
        self.workspaces.remove(token);
```

加 getter(放在 `session_for` 附近):

```rust
    /// 取本 token 对应会话的工作区绝对路径(`{{CC_WS}}` 落地用)。
    pub fn workspace_for(&self, token: &str) -> Option<String> {
        self.workspaces.get(token).map(|e| e.value().clone())
    }
```

在 `handle_post`(`mcp_proxy.rs:497`)里,把客户端返回的响应做落地替换。定位 `mcp_proxy.rs:538` 的 `Ok(Ok(resp)) => PostOutcome::Response(resp),` 改为:

```rust
                Ok(Ok(resp)) => {
                    let ws = state.workspace_for(token).unwrap_or_default();
                    PostOutcome::Response(substitute_ws_placeholder(&resp, &ws))
                }
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p agent register_stores_workspace_and_lookup`
Expected: PASS

- [ ] **Step 5: Fix the pty.rs register call site + other register callers**

`crates/agent/src/pty.rs:652` 当前是 `self.mcp.register(token.clone(), session_id);`。`cwd`(本会话工作区绝对路径,`pty.rs:607` 定义)在作用域内。改为:

```rust
                    self.mcp.register(
                        token.clone(),
                        session_id,
                        cwd.to_string_lossy().to_string(),
                    );
```

搜索其它 `register(` 调用站点并补第三参(测试里的 `register_overwrite_unregister_routing` 等):

Run: `grep -rn "\.register(" crates/agent/src`
对每个测试调用补 `, "/test/ws".to_string()`(任意占位绝对路径即可,这些测试不检验 workspace)。

- [ ] **Step 6: Run all agent tests + clippy**

Run: `cargo test -p agent && cargo clippy -p agent -- -D warnings`
Expected: PASS,0 warnings

- [ ] **Step 7: Commit**

```bash
git add crates/agent/src/mcp_proxy.rs crates/agent/src/pty.rs
git commit -m "agent: register per-token workspace path; land {{CC_WS}} in responses"
```

---

## Task 3: client — staging 目录 + 后端 `--output-dir` + spawn CWD

**Files:**
- Modify: `crates/client/src/mcp_host.rs`(`WS_PLACEHOLDER`/`ARTIFACT_DIR_REL` const、`artifact_dir()`、`--output-dir` 注入、CWD 线程化)

- [ ] **Step 1: Write the failing test**

加到 `mcp_host.rs` tests:

```rust
#[test]
fn builtin_backend_includes_output_dir() {
    // 默认后端(无 env、无 cfg.backend)应带 --user-data-dir 和 --output-dir
    let cfg = BrowserConfig { enabled: true, backend: None, profile_dir: Some(std::env::temp_dir().join("cc-test-profile")) };
    let (prog, args) = backend_command_from(None, &cfg).expect("default backend");
    assert_eq!(prog, "npx");
    assert!(args.iter().any(|a| a.starts_with("--user-data-dir=")), "args={args:?}");
    assert!(args.iter().any(|a| a.starts_with("--output-dir=")), "args={args:?}");
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p client builtin_backend_includes_output_dir`
Expected: FAIL —— 无 `--output-dir=` 参数

- [ ] **Step 3: Write minimal implementation**

在 `mcp_host.rs` const 区(`PLAYWRIGHT_MCP_PKG` 旁)加:

```rust
/// 占位符:产物路径重写成 `{{CC_WS}}/<ARTIFACT_DIR_REL>/<name>`,由 agent
/// mcp_proxy 落地成工作区绝对路径。
/// LOCKSTEP: 与 agent `crates/agent/src/mcp_proxy.rs` 的 `WS_PLACEHOLDER` 必须一致。
pub const WS_PLACEHOLDER: &str = "{{CC_WS}}";

/// 产物在 agent workspace 里的相对目录(FsWrite 目标 + 重写路径用)。
pub const ARTIFACT_DIR_REL: &str = ".cloudcode/browser-artifacts";
```

加 staging 目录助手(`default_profile_dir` 旁):

```rust
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
```

在 `backend_command_from`(`mcp_host.rs:104`)的内置默认分支(`mcp_host.rs:118-126`)追加 `--output-dir`。把 `vec![...]` 块改为:

```rust
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
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p client builtin_backend_includes_output_dir`
Expected: PASS

- [ ] **Step 5: Thread spawn CWD through McpProcess → McpChannel → McpHost**

改 `McpProcess::spawn`(`mcp_host.rs:159`)加 `cwd` 参:

```rust
    pub fn spawn(program: &str, args: &[String], cwd: Option<&std::path::Path>) -> std::io::Result<Self> {
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
```

改 `McpChannel::start`(`mcp_host.rs:253`)与 `start_replayed`(`mcp_host.rs:267`)各加 `cwd: Option<&std::path::Path>` 参,并把内部 `McpProcess::spawn(program, args)` 改为 `McpProcess::spawn(program, args, cwd)`(两处:`mcp_host.rs:260` 和 `:274`)。

`McpHost` 结构体(`mcp_host.rs:386`)加字段 `cwd: Option<std::path::PathBuf>,`。改 `McpHost::new`(`mcp_host.rs:395`)签名:

```rust
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
```

改 `spawn_channel`(`mcp_host.rs:455`)里两处 `McpChannel::start*(&prog, &args, ...)`,在 `self.out_tx.clone()` 前补 `self.cwd.as_deref()`:

```rust
        let started = if empty {
            McpChannel::start(&prog, &args, self.cwd.as_deref(), self.out_tx.clone(), self.handshake.clone())
        } else {
            McpChannel::start_replayed(&prog, &args, self.cwd.as_deref(), self.out_tx.clone(), self.handshake.clone()).await
        };
```

(对应改 `start`/`start_replayed` 形参顺序为 `(program, args, cwd, out_tx, handshake)`。)

- [ ] **Step 6: Fix all McpProcess::spawn / McpHost::new / McpChannel::start call sites**

Run: `grep -rn "McpProcess::spawn\|McpHost::new\|McpChannel::start" crates/client/src`
- `mcp_host.rs:685`(echo 桩测试):`McpProcess::spawn("node", &[fixture.to_string()], None)`。
- `relay.rs:163`:见 Task 6(届时改;本任务先让它编译——临时传 `None`)。本任务把 `relay.rs:163` 改为 `McpHost::new(b, host_out_tx.clone(), None)` 占位,Task 6 再换成真 staging。
- 集成测试 `host_roundtrips_via_real_playwright_mcp` 里的 `McpHost::new(...)`:补 `None` 第三参(该测试不验证产物)。

- [ ] **Step 7: Run client tests + clippy**

Run: `cargo test -p client && cargo clippy -p client -- -D warnings`
Expected: PASS,0 warnings

- [ ] **Step 8: Commit**

```bash
git add crates/client/src/mcp_host.rs crates/client/src/relay.rs
git commit -m "client: staging dir, --output-dir, thread backend spawn CWD"
```

---

## Task 4: client — 响应驱动的产物检测纯函数

**Files:**
- Modify: `crates/client/src/mcp_host.rs`(加 `detect_artifacts` + 单测)

- [ ] **Step 1: Write the failing test**

加到 `mcp_host.rs` tests(用真实 playwright-mcp 截图响应形态):

```rust
#[test]
fn detect_artifacts_from_markdown_links() {
    let staging = std::env::temp_dir().join("cc-detect-test");
    let _ = std::fs::create_dir_all(&staging);
    // 造两个 staging 文件:一个被链接引用(shot.png)、一个不被引用(orphan.png)
    std::fs::write(staging.join("shot.png"), b"x").unwrap();
    std::fs::write(staging.join("orphan.png"), b"x").unwrap();

    // 带 filename 的截图响应:链接 target 是 ./shot.png
    let payload = r#"{"jsonrpc":"2.0","id":1,"result":{"content":[{"type":"text","text":"### Result\n- [Screenshot of viewport](./shot.png)\n### Ran Playwright code"}]}}"#;
    let found = detect_artifacts(payload, &staging);
    // 只检测到被链接引用且存在于 staging 的文件
    assert_eq!(found, vec![("./shot.png".to_string(), "shot.png".to_string())]);

    // 链接指向不存在于 staging 的文件 → 不检测
    let none = r#"{"text":"- [x](./missing.png)"}"#;
    assert!(detect_artifacts(none, &staging).is_empty());

    // 无 markdown 链接 → 空
    assert!(detect_artifacts(r#"{"text":"plain"}"#, &staging).is_empty());

    let _ = std::fs::remove_dir_all(&staging);
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p client detect_artifacts_from_markdown_links`
Expected: FAIL —— `cannot find function detect_artifacts`

- [ ] **Step 3: Write minimal implementation**

加纯函数到 `mcp_host.rs`:

```rust
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
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p client detect_artifacts_from_markdown_links`
Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add crates/client/src/mcp_host.rs
git commit -m "client: response-driven artifact detection (markdown links -> staging files)"
```

---

## Task 5: client — 产物路径重写纯函数

**Files:**
- Modify: `crates/client/src/mcp_host.rs`(加 `rewrite_artifact_links` + 单测)

- [ ] **Step 1: Write the failing test**

```rust
#[test]
fn rewrite_artifact_links_replaces_targets() {
    let payload = r#"- [Screenshot of viewport](./shot.png)\n- [PDF](./doc.pdf)"#;
    let repl = vec![
        ("./shot.png".to_string(), "{{CC_WS}}/.cloudcode/browser-artifacts/shot.png".to_string()),
        ("./doc.pdf".to_string(), "[browser artifact not transferred: doc.pdf (12 MiB); generated on client only]".to_string()),
    ];
    let out = rewrite_artifact_links(payload, &repl);
    assert!(out.contains("](({{CC_WS}}/.cloudcode/browser-artifacts/shot.png)".trim_start_matches('(')));
    assert!(out.contains("[Screenshot of viewport]({{CC_WS}}/.cloudcode/browser-artifacts/shot.png)"));
    assert!(out.contains("[PDF]([browser artifact not transferred: doc.pdf (12 MiB); generated on client only])"));
    assert!(!out.contains("(./shot.png)"));
    assert!(!out.contains("(./doc.pdf)"));
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p client rewrite_artifact_links_replaces_targets`
Expected: FAIL —— `cannot find function rewrite_artifact_links`

- [ ] **Step 3: Write minimal implementation**

```rust
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
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p client rewrite_artifact_links_replaces_targets`
Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add crates/client/src/mcp_host.rs
git commit -m "client: artifact link rewrite helper"
```

---

## Task 6: client — relay 接线:检测 → spawn 转移任务 → 重写 → 发出

**Files:**
- Modify: `crates/client/src/relay.rs`(`upload_one_file` 加 `dest_dir`、`spawn_artifact_transfer`、`host_out_rx` 臂、`ARTIFACT_MAX_BYTES`、staging 接入)

- [ ] **Step 1: Write the failing test(大小上限决策纯函数)**

把"超限判定 + 提示文案"抽成纯函数以便测试。加到 `relay.rs`(`#[cfg(test)] mod tests` 若无则新建):

```rust
#[cfg(test)]
mod artifact_tests {
    use super::*;

    #[test]
    fn oversize_note_formats() {
        // 11 MiB > 10 MiB 上限 → 提示文案
        let note = oversize_artifact_note("big.pdf", 11 * 1024 * 1024);
        assert!(note.contains("not transferred"));
        assert!(note.contains("big.pdf"));
        assert!(note.contains("MiB"));
    }

    #[test]
    fn under_cap_passes() {
        assert!(!is_over_cap(ARTIFACT_MAX_BYTES));
        assert!(is_over_cap(ARTIFACT_MAX_BYTES + 1));
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p client artifact_tests`
Expected: FAIL —— 函数/常量未定义

- [ ] **Step 3: Write minimal implementation(常量 + 纯助手 + 通用化 upload)**

`relay.rs` 顶部 const 区(`UPLOAD_DIR` 旁)加:

```rust
/// 浏览器产物在 agent workspace 里的目标目录(与 mcp_host::ARTIFACT_DIR_REL 一致)。
const ARTIFACT_DIR: &str = ".cloudcode/browser-artifacts";
/// 单个产物回传的大小上限;超过则不传、改写成提示。
const ARTIFACT_MAX_BYTES: u64 = 10 * 1024 * 1024;

fn is_over_cap(size: u64) -> bool {
    size > ARTIFACT_MAX_BYTES
}

fn oversize_artifact_note(basename: &str, size: u64) -> String {
    let mib = size / (1024 * 1024);
    format!("[browser artifact not transferred: {basename} ({mib} MiB); generated on client only]")
}
```

通用化 `upload_one_file`(`relay.rs:366`):把硬编码的 `UPLOAD_DIR` 换成参数 `dest_dir: &str`。签名加一参,函数体里 `let dest = format!("{UPLOAD_DIR}/{file_name}");` 改为 `let dest = format!("{dest_dir}/{file_name}");`。同步改 `spawn_upload`(`relay.rs:344`)里的调用:`upload_one_file(&out_tx, request_id, &agent, &workspace, UPLOAD_DIR, &path, res_rx)`。

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p client artifact_tests`
Expected: PASS

- [ ] **Step 5: Add `spawn_artifact_transfer`(先过滤超限、只为要上传的产物注册通道)**

加到 `relay.rs`(`spawn_upload` 旁)。它复用 `pending_uploads` + `upload_one_file`,在 spawn 任务里跑(不阻塞 select! 循环),完成后发出重写过的 `RemoteMcp` 响应。超限产物在 spawn 前同步判定、不注册通道(避免 `pending_uploads` 泄漏):

```rust
/// 把一条含产物的 MCP 响应转成:逐个产物经 FsWrite 上传到
/// `.cloudcode/browser-artifacts/`,再把响应里的链接 target 重写成
/// `{{CC_WS}}/...`(或超限/失败提示),最后发出重写后的 RemoteMcp。
/// 跑在 spawn 任务里:select! 循环继续路由 FsWriteResult,不会死锁。
fn spawn_artifact_transfer(
    out_tx: &mpsc::Sender<OutFrame>,
    pending_uploads: &mut HashMap<Uuid, mpsc::Sender<HubToClient>>,
    agent: &str,
    workspace: &str,
    staging: std::path::PathBuf,
    payload: String,
    artifacts: Vec<(String, String)>,
) {
    // 立即决定每个产物:超限 → 提示(不注册通道);否则注册并排进上传队列。
    let mut immediate: Vec<(String, String)> = Vec::new(); // 超限/缺失 → (target, note)
    let mut jobs: Vec<(Uuid, String, std::path::PathBuf, mpsc::Receiver<HubToClient>)> = Vec::new();
    for (target, base) in artifacts {
        let abs_path = staging.join(&base);
        let size = std::fs::metadata(&abs_path).map(|m| m.len()).unwrap_or(0);
        if is_over_cap(size) {
            tracing::warn!(%base, size, "browser artifact over size cap; not transferred");
            immediate.push((target, oversize_artifact_note(&base, size)));
            continue;
        }
        let request_id = Uuid::new_v4();
        let (res_tx, res_rx) = mpsc::channel::<HubToClient>(1);
        pending_uploads.insert(request_id, res_tx);
        jobs.push((request_id, target, abs_path, res_rx));
    }

    let out_tx = out_tx.clone();
    let agent = agent.to_string();
    let workspace = workspace.to_string();

    tokio::spawn(async move {
        let mut repl = immediate;
        for (request_id, target, abs_path, res_rx) in jobs {
            match upload_one_file(&out_tx, request_id, &agent, &workspace, ARTIFACT_DIR, &abs_path.to_string_lossy(), res_rx).await {
                Ok(final_name) => repl.push((
                    target,
                    format!("{}/{ARTIFACT_DIR}/{final_name}", crate::mcp_host::WS_PLACEHOLDER),
                )),
                Err(_) => {
                    let base = abs_path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
                    tracing::warn!(%base, "browser artifact upload failed");
                    repl.push((target, format!("[browser artifact transfer failed: {base}]")));
                }
            }
        }
        let rewritten = crate::mcp_host::rewrite_artifact_links(&payload, &repl);
        let _ = out_tx.send(OutFrame::Text(ClientToHub::RemoteMcp {
            server: crate::mcp_host::CC_BROWSER_SERVER.to_string(),
            payload: rewritten,
        })).await;
    });
}
```

- [ ] **Step 6: Wire staging + the `host_out_rx` arm**

先在 relay 启动处接入 staging。把 `relay.rs:162-163` 的 McpHost 构造改为(注意:Task 3 Step 6 已把第三参临时填 `None`,这里换成真 staging):

```rust
    let artifact_staging = crate::mcp_host::artifact_dir();
    let mut mcp_host: Option<crate::mcp_host::McpHost> = crate::mcp_host::backend_command(browser)
        .map(|b| crate::mcp_host::McpHost::new(b, host_out_tx.clone(), artifact_staging.clone()));
```

把 `host_out_rx.recv()` 臂(`relay.rs:289`)整段替换为:检测 → 无产物内联发、有产物则 spawn 转移任务:

```rust
            out = host_out_rx.recv() => {
                if let Some(payload) = out {
                    let artifacts = match &artifact_staging {
                        Some(dir) => crate::mcp_host::detect_artifacts(&payload, dir),
                        None => Vec::new(),
                    };
                    if artifacts.is_empty() {
                        let _ = wire.out_tx.send(OutFrame::Text(ClientToHub::RemoteMcp {
                            server: crate::mcp_host::CC_BROWSER_SERVER.to_string(),
                            payload,
                        })).await;
                    } else {
                        // 检测非空 ⇒ staging 必为 Some。
                        let dir = artifact_staging.clone().expect("staging present when artifacts detected");
                        spawn_artifact_transfer(
                            &wire.out_tx,
                            &mut pending_uploads,
                            agent,
                            workspace,
                            dir,
                            payload,
                            artifacts,
                        );
                    }
                }
            }
```

- [ ] **Step 7: Run client tests + clippy**

Run: `cargo test -p client && cargo clippy -p client -- -D warnings`
Expected: PASS,0 warnings

- [ ] **Step 8: Commit**

```bash
git add crates/client/src/relay.rs
git commit -m "client: relay artifact transfer — detect, FsWrite mirror, rewrite paths"
```

---

## Task 7: 集成测试 — 真 playwright-mcp 截图 → 检测 + staging 落盘

**Files:**
- Modify: `crates/client/src/mcp_host.rs`(扩展既有 node+chromium 门控集成测试,或新增一个)

- [ ] **Step 1: Write the gated integration test**

加到 `mcp_host.rs` tests,门控同既有 `host_roundtrips_via_real_playwright_mcp`(探测 node + 可跑 chromium;不可用则 `return`/skip)。本测试不需要 agent/hub——只验证 client 侧"真截图 → staging 出现文件 → detect_artifacts 命中 → rewrite 产出 `{{CC_WS}}` 路径":

```rust
#[tokio::test]
async fn screenshot_lands_in_staging_and_is_detected() {
    if !node_available() { eprintln!("skip: no node"); return; }
    let staging = std::env::temp_dir().join(format!("cc-art-it-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&staging);

    // 直接 spawn playwright-mcp,CWD + --output-dir 都钉到 staging。
    let args: Vec<String> = vec![
        "-y".into(), PLAYWRIGHT_MCP_PKG.into(), "--headless".into(),
        format!("--output-dir={}", staging.display()),
    ];
    let mut proc = match McpProcess::spawn("npx", &args, Some(staging.as_path())) {
        Ok(p) => p, Err(_) => { eprintln!("skip: spawn failed"); return; }
    };
    // initialize → navigate example.com → wait → screenshot(filename: shot.png)
    // (用与既有集成测试相同的 feed/next_frame 手法驱动;最后一帧是截图响应文本)
    // ... 见既有 host_roundtrips_via_real_playwright_mcp 的请求驱动样板 ...
    let screenshot_resp: String = drive_screenshot(&mut proc, "shot.png").await; // 测试内联辅助

    // staging 里出现 shot.png
    assert!(staging.join("shot.png").is_file(), "staging missing screenshot; resp={screenshot_resp}");
    // detect 命中,rewrite 产出占位符路径
    let found = detect_artifacts(&screenshot_resp, &staging);
    assert!(found.iter().any(|(_, b)| b == "shot.png"), "not detected; resp={screenshot_resp}");
    let repl: Vec<(String, String)> = found.iter().map(|(t, b)| (t.clone(), format!("{WS_PLACEHOLDER}/{ARTIFACT_DIR_REL}/{b}"))).collect();
    let rewritten = rewrite_artifact_links(&screenshot_resp, &repl);
    assert!(rewritten.contains(&format!("{WS_PLACEHOLDER}/{ARTIFACT_DIR_REL}/shot.png")));

    let _ = std::fs::remove_dir_all(&staging);
}
```

实现 `drive_screenshot` 内联辅助时,复用既有集成测试里 `proc.feed(json)/proc.next_frame()` 的写法:依次发 `initialize`、`notifications/initialized`、`browser_navigate{url:"https://example.com"}`、`browser_wait_for{time:1}`、`browser_take_screenshot{type:"png",filename:"shot.png"}`,返回最后一条截图响应文本。**关键**:截图必须带 `filename:"shot.png"`(复现 claude 的真实习惯,验证 CWD 落盘路径)。

- [ ] **Step 2: Run it (best-effort; skips without node/chromium)**

Run: `cargo test -p client screenshot_lands_in_staging_and_is_detected -- --nocapture`
Expected: PASS(有 node+chromium 时真跑);无则打印 skip 并 PASS。

- [ ] **Step 3: Commit**

```bash
git add crates/client/src/mcp_host.rs
git commit -m "test: integration — real screenshot lands in staging, detected + rewritten"
```

---

## Task 8: 文档 + 工作区全量回归

**Files:**
- Modify: `README.md`(浏览器预设 blockquote 补一句产物回传)

- [ ] **Step 1: Update README browser-preset note**

在 README 既有 `> **Browser preset:**` blockquote 末尾补一句:

```markdown
> 使用 cc-browser(本地有头)后端时,浏览器在你本机产生的截图/PDF 会自动回传到 agent workspace 的 `.cloudcode/browser-artifacts/`,远程 claude 可直接读取(单个产物 ≤ 10 MiB)。
```

- [ ] **Step 2: Full workspace build + test + clippy**

Run:
```bash
cargo build 2>&1 | tail -5
cargo test 2>&1 | tail -20
cargo clippy -p agent -p client -- -D warnings 2>&1 | tail -10
```
Expected: build 0 警告;全部测试 PASS;agent+client clippy 0 warnings。

- [ ] **Step 3: Commit**

```bash
git add README.md
git commit -m "docs: note cc-browser artifact transfer in README"
```

---

## Self-Review 检查清单(实现者跑完所有任务后)

- [ ] **Spec 覆盖**:目标 1(产物可读)= Task 3-7;目标 2(透明)= 检测+重写+落地全自动;目标 3(复用 FsWrite/零协议改动)= Task 6 复用 `upload_one_file`/帧;目标 4(顺序)= spawn 任务上传完成才发响应;目标 5(自动镜像)= host_out_rx 臂自动触发。
- [ ] **占位符 lockstep**:`WS_PLACEHOLDER` 在 mcp_proxy.rs 与 mcp_host.rs 一致。
- [ ] **类型一致**:`detect_artifacts` 返回 `Vec<(String,String)>`(target, basename),`rewrite_artifact_links` 吃 `&[(String,String)]`(target, newval),`spawn_artifact_transfer` 衔接二者;`register` 三参在所有调用站点一致。
- [ ] **无产物路径零回归**:`web` 后端 / 无 staging / 无 markdown 链接 → 响应原样内联发出。
- [ ] **真机验证清单**(交用户):cc-browser 真截图 → claude 在 agent Read 成功看见图;`browser_pdf_save` 响应形态确认(开放问题 1);下载文件落点(开放问题 1)。
