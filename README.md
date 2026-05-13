# CloudCode

> Drive your own `claude` from any terminal, anywhere.

`claude /login` runs on one host — your workstation, a home server, a cloud VM. CloudCode lets you talk to that claude from a laptop, a phone, any SSH terminal, without copying credentials around. The remote terminal **is** the native claude TUI; CloudCode just streams PTY bytes.

**Solo use only.** Subscription plans (Claude Max / Pro) are per-individual under Anthropic's Terms of Service. Sharing one across users violates them and the account may get banned. One user → one subscription → one agent.

![architecture](docs/architecture.svg?v=3)

## Highlights

- **Native claude TUI, end to end.** No wrapper layer — slash commands, todos, diffs, permission prompts all work because CloudCode forwards raw PTY bytes from `tmux+claude` on the agent.
- **Persistent workspaces.** Close the laptop, lose Wi-Fi, switch from terminal to phone. tmux + claude keep running on the agent. Reconnect and pick up exactly where you left off, mid-task.
- **macOS Seatbelt sandbox (opt-in).** Each workspace's claude runs sealed off from `~/.ssh`, Keychain, sibling workspaces, and cross-account state. Network stays open.
- **Self-hosted admin UI.** Single binary, embedded React SPA at `/admin/`. Manage accounts and agents with **two-way strict-whitelist ACL** (per-account agent access, per-agent account access), browse live & historical workspaces, sessions, and audit events.
- **Credentials stay put.** OAuth tokens never leave the agent host. The client only ever sees terminal bytes.

## Modules

```
cloudcode         the client — raw-mode PTY pump, runs on the laptop
cloudcode-agent   runs `tmux` + `claude` per workspace on the host where you logged in
cloudcode-hub     public-facing gateway + admin UI; bundled SPA
```

Agent dials out to the hub over WSS (NAT-friendly). Client connects to the hub; hub multiplexes PTY bytes across agents on a per-session uuid frame.

## Quick start

```bash
# on the public host:
curl -fsSL https://raw.githubusercontent.com/initialz/cloudcode/main/install.sh | sh -s -- hub
cloudcode-hub --init && cloudcode-hub daemon start --config ./hub.toml
# save both tokens it prints: one for agents, one for the admin UI

# on the host with your claude login:
curl -fsSL https://raw.githubusercontent.com/initialz/cloudcode/main/install.sh | sh -s -- agent
cloudcode-agent --init && $EDITOR agent.toml && cloudcode-agent daemon start --config ./agent.toml

# on your laptop:
curl -fsSL https://raw.githubusercontent.com/initialz/cloudcode/main/install.sh | sh -s -- client
cloudcode --init && $EDITOR ~/.config/cloudcode/config.toml && cloudcode
```

Open the admin UI at `http://<hub>:7101/admin/`, paste the admin token, grant your account access to the agent, and you're done. Build from source: `cargo build --release --workspace`.

## Architecture

[`docs/architecture.svg`](docs/architecture.svg) · [`hub.example.toml`](hub.example.toml) · [`agent.example.toml`](agent.example.toml)

## Acknowledgements

macOS Seatbelt sandbox design inspired by [boxsh](https://github.com/xicilion/boxsh). No boxsh code is included (boxsh is GPL v3); CloudCode is MIT.

## License

MIT. Provided as is, without warranty. The authors are not liable for any use that violates third-party Terms of Service.
