# cloudcode

> Self-hosted LLM gateway: centralise credentials, let your team use Claude Code transparently, and keep a per-request audit trail.

## Intended use & disclaimer

**This project is only for remotely controlling _your own_ coding CLI** — typically so you can run `claude` from a laptop while the OAuth credentials stay on a different machine (a workstation, a server, a sandbox) that you also own and logged into with `claude /login`.

**Do not share a subscription account across multiple people.** Subscription plans (e.g. Claude Max / Pro) are issued per individual under the provider's Terms of Service. Running one agent that proxies multiple humans' traffic onto a single subscription violates those terms. The recommended topology is **one user → one subscription → one agent**.

If you use this software to violate any provider's Terms of Service or applicable laws, **you are solely responsible for the consequences**. The authors and contributors of cloudcode provide this software as-is, with no warranty, and accept no liability for your usage.

## Components

- **`cloudcode-hub`** — gateway: account-token auth, ACL, JSONL audit log; runs on a publicly reachable host.
- **`cloudcode-agent`** — outbound WS tunnel endpoint that forwards requests using the locally stored claude OAuth credentials; run it as the same OS user that did `claude /login`.
- **`cloudcode`** — client launcher that starts `claude` on a developer's machine and routes its traffic through the hub.

## Architecture

![cloudcode architecture](docs/architecture.svg)

Source: [`docs/architecture.drawio`](docs/architecture.drawio) (open with [diagrams.net](https://app.diagrams.net)).

## Install

### Option A — Prebuilt binary (recommended)

Hub (public host):

```bash
curl -fsSL https://raw.githubusercontent.com/initialz/cloudcode/main/install.sh | sh -s -- hub
```

Agent (any machine where you've run `claude /login`; behind NAT is fine):

```bash
curl -fsSL https://raw.githubusercontent.com/initialz/cloudcode/main/install.sh | sh -s -- agent
```

Client (developer workstation):

```bash
curl -fsSL https://raw.githubusercontent.com/initialz/cloudcode/main/install.sh | sh -s -- client
```

Supported: Linux x86_64 / aarch64, macOS aarch64.

### Option B — Build from source

Requires a recent stable Rust toolchain (`rustup` recommended).

```bash
git clone https://github.com/initialz/cloudcode.git
cd cloudcode
cargo build --release --workspace

# Pick whichever subset you need on this host:
sudo install -m 0755 target/release/cloudcode-hub   /usr/local/bin/
sudo install -m 0755 target/release/cloudcode-agent /usr/local/bin/
sudo install -m 0755 target/release/cloudcode       /usr/local/bin/
```

Or just run the binaries directly out of `target/release/` without installing.

## Usage

### Hub (administrator)

```bash
# One-time: write a fresh hub.toml in the current directory.
# Refuses to overwrite if hub.toml already exists.
cloudcode-hub --init                     # or: --init --config /custom/path.toml

cloudcode-hub gen-token alice            # one token per user
$EDITOR ./hub.toml                       # paste [[accounts]] and [[agents]] blocks
cloudcode-hub --config ./hub.toml        # foreground; logs to stdout
# or
cloudcode-hub daemon start --config ./hub.toml   # background
```

### Agent (one-time setup)

Run the agent as the same OS user that did `claude /login`. The agent will read `~/.claude/.credentials.json` automatically — no file copying or `chmod` required.

```bash
# One-time: write a fresh agent.toml in the current directory, with an
# auto-generated shared_secret, and print an [[agents]] block to hand to
# your hub admin. Refuses to overwrite if agent.toml already exists.
cloudcode-agent --init                   # or: --init --config /custom/path.toml

$EDITOR ./agent.toml                     # edit [hub].url

cloudcode-agent --config ./agent.toml    # foreground; logs to stdout
# or
cloudcode-agent daemon start --config ./agent.toml   # background
```

### Client (developer)

```toml
# ~/.config/cloudcode/config.toml
hub_url = "https://your-hub-host"
token   = "cc_xxx_from_admin"
```

```bash
cd ~/code/myproj
cloudcode run claude
```

The experience is identical to running `claude` directly — every API call flows through the hub for auth, routing, and audit.

> Daemon-mode logs land in `~/.local/state/cloudcode/{hub,agent}.log`. Lifecycle: `cloudcode-hub|cloudcode-agent daemon {status,stop,restart}`.

## Configuration reference

[`hub.example.toml`](hub.example.toml) · [`agent.example.toml`](agent.example.toml)

## License

MIT. The software is provided "as is", without warranty of any kind. The authors are not liable for any use that violates third-party terms of service.
