# CloudCode

> Drive your own `claude` from any terminal, anywhere.

`claude /login` runs on one host — your workstation, a home server, a cloud VM. CloudCode lets you talk to that claude from a laptop, a phone, any SSH terminal, without copying credentials around. The remote terminal **is** the native claude TUI; CloudCode just streams PTY bytes.

**Solo use only.** Subscription plans (Claude Max / Pro) are per-individual under Anthropic's Terms of Service. Sharing one across users violates them and the account may get banned. One user → one subscription → one agent.

![architecture](docs/architecture.svg?v=5)

## Highlights

- **Native claude TUI, end to end.** No wrapper layer — slash commands, todos, diffs, permission prompts all work because CloudCode forwards raw PTY bytes from `tmux+claude` on the agent.
- **Persistent workspaces.** Close the laptop, lose Wi-Fi, switch from terminal to phone. tmux + claude keep running on the agent. Reconnect and pick up exactly where you left off, mid-task.
- **Webterm with native UX.** Self-hosted SPA at `/` — DOM-rendered xterm.js with native scroll, native text selection, browser Cmd+C copy, 50k-line scrollback persisted across page refresh via IndexedDB.
- **File manager.** Browse workspace files in-browser, multi-select, download as zip archive (files and directories).
- **macOS Seatbelt sandbox (opt-in).** Each workspace's claude runs sealed off from `~/.ssh`, Keychain, sibling workspaces, and cross-account state. Browser automation (Playwright) allowed.
- **Self-hosted admin UI.** Single binary, embedded React SPA at `/admin/`. Manage accounts (with real names) and agents with **two-way strict-whitelist ACL**, browse live & historical workspaces, sessions, and audit events.
- **Credentials stay put.** OAuth tokens never leave the agent host. The client only ever sees terminal bytes.

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

> **Browser preset:** the default web-browsing backends on both the agent (headless) and the client (visible window) run `@playwright/mcp` via `npx` — install **node >= 18** on both machines to use them. Without node, everything else works; browser tool calls return an actionable error instead.

Open the admin UI at `http://<hub>:7101/admin/`, paste the admin token, grant your account access to the agent, and you're done. The user-facing browser client lives at `http://<hub>:7100/`.

### Windows (WSL2)

CloudCode's agent uses tmux for session persistence; native Windows builds aren't shipped yet. The recommended path is WSL2.

**From a native Windows PowerShell window (not inside WSL):**

```powershell
# default is agent; set CC_COMPONENT to "hub" or "client" if needed
iwr -useb https://raw.githubusercontent.com/initialz/cloudcode/main/install.ps1 | iex
```

The script makes sure WSL2 is installed and then runs the Linux installer inside your default distro.

**If you're already inside a WSL shell** (e.g. `wsl` then a bash prompt), skip the PowerShell wrapper and use the Linux one-liner directly:

```bash
curl -fsSL https://raw.githubusercontent.com/initialz/cloudcode/main/install.sh | sudo sh -s -- agent
```

After install your agent lives inside WSL2. Start it from a WSL shell with `cloudcode-agent daemon start --config ./agent.toml` after editing the config.

## Documentation

→ **[User Guide](docs/USER_GUIDE.md)** — installation in depth, multi-tool setup, web UI walkthrough, CLI menu / persistence rules, macOS sandbox, admin UI, self-update, troubleshooting.

→ [`docs/architecture.svg`](docs/architecture.svg) · [`hub.example.toml`](hub.example.toml) · [`agent.example.toml`](agent.example.toml)

## Acknowledgements

macOS Seatbelt sandbox design inspired by [boxsh](https://github.com/xicilion/boxsh). No boxsh code is included (boxsh is GPL v3); CloudCode is MIT.

## License

MIT. Provided as is, without warranty. The authors are not liable for any use that violates third-party Terms of Service.
