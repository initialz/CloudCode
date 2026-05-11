# cloudcode

> Remote-control your own `claude` from anywhere. A TUI chat client talks to a hub, which forwards every prompt to an agent on a machine that has already done `claude /login`.

## Intended use & disclaimer

**This project is only for remotely controlling _your own_ coding CLI.** Typical setup: you `claude /login`'d on a workstation / server / home box, and you want to drive `claude` from a laptop, a phone, or an SSH terminal that doesn't have your subscription credentials.

**Do not share a subscription account across multiple people.** Subscription plans (Claude Max / Pro) are issued per individual under the provider's Terms of Service. Routing many humans' prompts onto one subscription violates those terms. The recommended topology is **one user → one subscription → one agent**.

If you use this software to violate any provider's Terms of Service or applicable laws, **you are solely responsible for the consequences**. The authors and contributors provide this software as-is, with no warranty, and accept no liability for your usage.

## Components

- **`cloudcode-hub`** — public-facing gateway: account-token auth, ACL, workspace mutex, JSONL audit log. Routes session traffic between clients and agents.
- **`cloudcode-agent`** — long-running daemon on a host where you've `claude /login`'d. Dials out to the hub over WSS. When the hub pushes a user turn, the agent fork+execs `claude -p --output-format stream-json` in the selected workspace and streams the result back. Multi-turn conversations are stitched together with `--resume <session_id>`.
- **`cloudcode`** — TUI client on your laptop. Run `cloudcode` to open an interactive session with claude on a remote agent; slash commands manage workspaces on the agent side.

## Architecture

![cloudcode architecture](docs/architecture.svg)

Source: [`docs/architecture.drawio`](docs/architecture.drawio) (open with [diagrams.net](https://app.diagrams.net)).

## Install

### Option A — Prebuilt binary

Hub (public host):

```bash
curl -fsSL https://raw.githubusercontent.com/initialz/cloudcode/main/install.sh | sh -s -- hub
```

Agent (any host where you've run `claude /login`; behind NAT is fine):

```bash
curl -fsSL https://raw.githubusercontent.com/initialz/cloudcode/main/install.sh | sh -s -- agent
```

Client (laptop):

```bash
curl -fsSL https://raw.githubusercontent.com/initialz/cloudcode/main/install.sh | sh -s -- client
```

Supported: Linux x86_64 / aarch64, macOS aarch64.

### Option B — Build from source

```bash
git clone https://github.com/initialz/cloudcode.git
cd cloudcode
cargo build --release --workspace

sudo install -m 0755 target/release/cloudcode-hub   /usr/local/bin/
sudo install -m 0755 target/release/cloudcode-agent /usr/local/bin/
sudo install -m 0755 target/release/cloudcode       /usr/local/bin/
```

## Usage

### Hub (administrator)

```bash
cloudcode-hub --init                     # writes ./hub.toml
cloudcode-hub gen-token alice            # one token per user
$EDITOR ./hub.toml                       # paste [[accounts]] and [[agents]] blocks
cloudcode-hub --config ./hub.toml        # foreground; logs to stdout
# or
cloudcode-hub daemon start --config ./hub.toml   # background
```

### Agent (one-time setup)

Run the agent as the same OS user that did `claude /login`. The agent never reads OAuth credentials itself; it just `fork+exec`s `claude`, and `claude` finds its own credentials (keychain on macOS, `~/.claude/.credentials.json` on Linux).

```bash
cloudcode-agent --init                   # writes ./agent.toml + prints [[agents]] block
$EDITOR ./agent.toml                     # edit [hub].url

cloudcode-agent --config ./agent.toml    # foreground; logs to stdout
# or
cloudcode-agent daemon start --config ./agent.toml   # background
```

The `[[agents]]` block printed by `--init` goes into the hub admin's `hub.toml`.

### Client (developer)

```toml
# ~/.config/cloudcode/config.toml  (macOS: ~/Library/Application Support/cloudcode/config.toml)
hub_url = "https://your-hub-host"
token   = "cc_xxx_from_admin"
```

Run `cloudcode` — drops you into a TUI:

```bash
cloudcode                            # uses workspace "default"
cloudcode --workspace projA          # open straight into a named workspace
cloudcode --agent peter-mbp          # pin a specific agent
```

#### TUI keybindings

| Key | Action |
|---|---|
| `Enter` | Send the typed message |
| `Alt+Enter` | Insert newline |
| `Ctrl+C` | Interrupt the running turn (sends SIGINT to claude); if no turn is active, quit |
| `Ctrl+D` | Quit |
| `PgUp` / `PgDn` | Scroll history |

#### Slash commands (parsed locally, not sent to claude)

| Command | Effect |
|---|---|
| `/ws list` | Ask the agent to list workspace directories |
| `/ws create <name>` | Create a new workspace dir on the agent |
| `/ws use <name>` | Switch the current session to a different workspace; conversation starts fresh |
| `/ws remove <name>` | Delete a workspace (refused if any session has it open) |
| `/reset` | Drop the current conversation, stay in the same workspace |
| `/status` | Show session info |
| `/help` | List commands |
| `/exit` / `/quit` | Close the session |

Workspaces are named directories under `<workspace_root>` on the agent host (default `~/cloudcode-agent/workspaces/<name>`). A given workspace can be held by at most one session at a time across the whole fleet — the hub enforces this.

> Daemon-mode logs: `~/.local/state/cloudcode/{hub,agent}.log`. Lifecycle: `cloudcode-{hub,agent} daemon {status,stop,restart}`.

## Configuration reference

[`hub.example.toml`](hub.example.toml) · [`agent.example.toml`](agent.example.toml)

## License

MIT. The software is provided "as is", without warranty of any kind. The authors are not liable for any use that violates third-party terms of service.
