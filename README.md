# cloudcode

> 自托管 LLM 网关，让团队以集中受控的方式使用 AI coding CLI（如 Claude Code）。
> Hub 持有真实 API key、按账号下发 token、记录审计；客户端体验和裸跑 AI CLI 一致。

---

## 概述

**cloudcode** 是一对小工具：

- **`cloudcode-hub`**（服务端）—— 持有真实的 LLM provider API key，按账号 token 鉴权，记录每次请求的审计日志。
- **`cloudcode`**（客户端）—— 在开发者本地启动原生 AI CLI（`claude` 等），透明把流量经 hub 转发，体验与裸跑 AI CLI 完全一致。

典型使用场景：

- 团队不想让每位开发者手持 Anthropic API key
- 需要审计「谁用了多少 token / 调用了哪些模型」
- 想统一控制每个账号能访问哪些 provider

源代码、依赖、构建产物始终在开发者本地——**hub 只代理 HTTPS API 请求，不传源码、不拉 PTY**。

## 架构

```
┌─────────────────┐    HTTPS    ┌────────────────────┐    HTTPS    ┌────────────────┐
│ Developer       │             │  cloudcode-hub     │             │  Anthropic API │
│                 │             │                    │             │                │
│ $ cd ~/myproj   │             │  - token 鉴权      │             │                │
│ $ cloudcode run │─────────────│  - ACL 检查        │─────────────│                │
│     claude      │   account   │  - 转发 + 流式     │  real key   │                │
│                 │   token     │  - JSONL 审计      │             │                │
│ (本地源码 +     │             │                    │             │                │
│  本地依赖)      │             │                    │             │                │
└─────────────────┘             └────────────────────┘             └────────────────┘
```

## 安装

### Hub（远端服务器）

```bash
curl -fsSL https://raw.githubusercontent.com/initialz/cloudcode/main/install.sh | sh -s -- hub
```

在 Linux 上脚本会：

1. 检测平台并从 GitHub Releases 拉对应的 musl 静态二进制
2. 安装到 `/usr/local/bin/`
3. 创建 `cloudcode` 系统用户与 `/etc/cloudcode/`、`/var/log/cloudcode/`、`/var/lib/cloudcode/`
4. 写 `/etc/systemd/system/cloudcode-hub.service`（带 `NoNewPrivileges`、`ProtectSystem`、`ProtectHome` 等 hardening）

可选标记：

| 标记 | 作用 |
|------|------|
| `--no-service` | 只装二进制，跳过 systemd unit |
| `--service` | 强制装 systemd unit（非 Linux 自动跳过） |
| `--prefix DIR` | 自定义安装前缀，默认 `/usr/local` |
| `--version vX.Y.Z` | 锁定版本，默认 `latest` |

支持平台：Linux x86_64、Linux aarch64、macOS aarch64（Apple Silicon）。

### Client（开发者本地）

```bash
curl -fsSL https://raw.githubusercontent.com/initialz/cloudcode/main/install.sh | sh -s -- client
```

同样支持 Linux x86_64/aarch64 与 macOS arm64。

## 配置

### Hub

1. 创建配置文件：

   ```bash
   sudo cp /etc/cloudcode/hub.example.toml /etc/cloudcode/hub.toml
   sudo $EDITOR /etc/cloudcode/hub.toml
   sudo chown cloudcode:cloudcode /etc/cloudcode/hub.toml
   sudo chmod 640 /etc/cloudcode/hub.toml
   ```

2. 填入真实的 Anthropic API key：

   ```toml
   [server]
   listen = "0.0.0.0:7000"
   audit_log = "/var/log/cloudcode/audit.jsonl"

   [anthropic]
   upstream = "https://api.anthropic.com"
   api_key = "sk-ant-..."
   ```

3. 为每个用户生成 token：

   ```bash
   cloudcode-hub gen-token alice
   ```

   命令会输出一段明文 token（仅这一次）和 `argon2id` 哈希。把哈希粘到 `hub.toml` 的 `[[accounts]]` 段，把明文 token 安全交给用户：

   ```toml
   [[accounts]]
   name = "alice"
   token_hash = "$argon2id$v=19$m=19456,t=2,p=1$..."
   allowed_providers = ["anthropic"]
   ```

   `allowed_providers` 控制账号能访问哪些 provider，`["*"]` 表示全部。

4. 启动服务：

   ```bash
   sudo systemctl enable --now cloudcode-hub
   journalctl -u cloudcode-hub -f
   ```

> 在生产环境，建议在 hub 前置一个 TLS 终止层（Caddy / nginx / Cloudflare），不要把 7000 端口直接暴露到公网。

### Client

在 `~/.config/cloudcode/config.toml` 写入：

```toml
hub_url = "https://hub.example.com"
token   = "cc_xxx_from_admin"
```

`cloudcode config` 会显示当前配置，没建过的话也会输出创建模板。

## 使用

在任意项目目录运行：

```bash
cd ~/code/myproj
cloudcode run claude
```

`cloudcode` 注入两个环境变量后 `exec` 真正的 `claude` 二进制：

- `ANTHROPIC_BASE_URL` 指向 hub
- `ANTHROPIC_AUTH_TOKEN` 是用户的 cloudcode token

之后体验和直接跑 `claude` 一模一样——本地源码、本地依赖、本地终端 UI。工具发出的每个 `POST /v1/messages`（包括 SSE 流式响应）都被 hub 鉴权、记录、透明转发到 `api.anthropic.com`。

### 子命令一览

```
cloudcode run <tool>          启动 AI CLI 工具（MVP 仅支持 claude）
cloudcode config              显示当前 client 配置

cloudcode-hub serve [--config hub.toml]   启动 hub
cloudcode-hub gen-token <name>            生成新账号 token
```

## 审计日志

每次请求追加一行 JSON 到配置中的 `audit_log`：

```jsonl
{"ts":"2026-05-11T01:59:48Z","event":"auth_denied","provider":"anthropic","status":401,"reason":"missing token"}
{"ts":"2026-05-11T01:59:49Z","event":"messages_request","account":"alice","provider":"anthropic","model":"claude-opus-4-7","status":200,"stream":true}
{"ts":"2026-05-11T02:01:14Z","event":"auth_denied","account":"bob","provider":"anthropic","status":403,"reason":"provider not allowed"}
```

格式开放，可直接灌入任何日志/审计系统（Loki、ELK、ClickHouse 等）。

## 当前状态

MVP 已可用：

- ✅ Anthropic Claude Code 转发（含 SSE 流式）
- ✅ 按账号 argon2id token 鉴权
- ✅ Provider 级 ACL
- ✅ JSONL 审计日志
- ✅ systemd 服务化、curl 一键安装

后续路线：

- OpenAI / codex 支持（同一架构、加一条 `/openai/*` 路由）
- 解析 SSE 提取实际 token 用量
- Web 管理界面（账号 CRUD、token 撤销、审计查看）
- 配额、限速、token 轮换

## 本地开发

```bash
cargo build              # 编译两个二进制
cargo run -p cloudcode-hub -- gen-token alice
cargo run -p cloudcode-hub -- serve --config /tmp/hub.toml
cargo run -p cloudcode-client -- run claude
```

`cargo build --release` 出 `target/release/cloudcode-hub` 和 `cloudcode`。release workflow 用 musl target 产出静态二进制，无 glibc 依赖。

## License

MIT
