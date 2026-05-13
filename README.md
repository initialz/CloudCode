# cloudcode

> Drive your own `claude` from any terminal, anywhere.

You've run `claude /login` once on some machine — your workstation, a home server, a cloud VM. cloudcode lets you talk to that claude from a laptop, a phone, or any SSH terminal, without copying credentials around.

**Solo use only.** Subscription plans (Claude Max / Pro) are per-individual under Anthropic's Terms of Service. Sharing one across multiple users violates those terms; the account may get banned. Recommended topology: **one user → one subscription → one agent**.

## Quick start

Three pieces. Install the one you need.

### As an admin (run the hub)

The hub lives on a host with a public address.

```bash
curl -fsSL https://raw.githubusercontent.com/initialz/cloudcode/main/install.sh | sh -s -- hub
cloudcode-hub --init                              # writes hub.toml + prints agent registration & admin tokens
cloudcode-hub daemon start --config ./hub.toml
```

`--init` prints **two** one-shot plaintext tokens; save both before they scroll off:

- **agent registration token** → hand to every agent operator (goes into `agent.toml`)
- **admin UI token** → keep for yourself; you'll paste it once into the admin login page

By default the admin UI listens on `127.0.0.1:7101`. Open <http://localhost:7101/admin/login> (or run an SSH tunnel from your laptop), paste the admin token, and you get:

- **Dashboard** — accounts / active sessions / sessions in the last 24h / online agents, plus an hourly chart of session starts.
- **Accounts** — create, rotate token, disable / enable, delete. Newly issued tokens are displayed once with a Copy button.
- **Sessions** — paged list, filter by account / agent / workspace, "active only" toggle. Live sessions show a pulsing dot; closed ones show duration and close reason.
- **Audit** — paged timeline, filter by account / agent / kind / time range. Detail column opens a modal with the full JSON event.

The whole UI is a React SPA built with Vite and Tailwind. It's bundled into the hub binary at build time — single-file deploy, no extra static directory to ship.

For remote access, put a TLS-terminating reverse proxy in front of `127.0.0.1:7101`. SSH tunneling also works:

```bash
ssh -N -L 7101:127.0.0.1:7101 user@hub-host
# then open http://127.0.0.1:7101/admin/ in your local browser
```

### As an agent operator (run claude on this host)

The agent runs on the box where `claude /login` worked. Needs `tmux` and a working `claude` binary, both as the same OS user.

```bash
curl -fsSL https://raw.githubusercontent.com/initialz/cloudcode/main/install.sh | sh -s -- agent
cloudcode-agent --init                            # writes agent.toml template
$EDITOR ./agent.toml                              # set [hub].url and [auth].registration_token
cloudcode-agent daemon start --config ./agent.toml
```

### As a client (the laptop)

```bash
curl -fsSL https://raw.githubusercontent.com/initialz/cloudcode/main/install.sh | sh -s -- client
cloudcode --init                                  # writes ~/.config/cloudcode/config.toml
$EDITOR ~/.config/cloudcode/config.toml           # set hub_url + token (your account token)
cloudcode
```

That last command opens the picker. Choose an agent → choose a workspace → drop into claude.

Supported binaries: Linux x86_64 / aarch64, macOS aarch64. Build from source: `cargo build --release --workspace`.

## Using cloudcode

```bash
cloudcode                                         # last agent + workspace picker
cloudcode --agent peter-mbp                       # pin a specific agent
cloudcode -- --continue                           # forward any arg to remote claude
cloudcode -- --model opus
cloudcode -- "fix the failing test"
```

Everything after `--` is passed straight through to the spawned `claude`.

### The picker

```
   ____ _                 _  ____          _
  / ___| | ___  _   _  __| |/ ___|___   __| | ___
 | |   | |/ _ \| | | |/ _` | |   / _ \ / _` |/ _ \
 | |___| | (_) | |_| | (_| | |__| (_) | (_| |  __/
  \____|_|\___/ \__,_|\__,_|\____\___/ \__,_|\___|

   Select workspace on alpha:
     1  default
     2  proja
   ▶ 3  projb
     4  scratch
```

- ↑↓ (or `j` / `k`) — move
- Enter — open
- `c` — create new workspace
- `d` — delete the highlighted one
- Esc — back / quit
- `q` — quit

Inside, your terminal **is** the claude TUI: status bar, slash commands, todo board, diffs, permission prompts.

## Things to know

- **To leave without losing state: just close your terminal** (cmd-W / shut the laptop / disconnect SSH). Claude and tmux keep running on the agent. Reconnect whenever — you're back at the same prompt, todo, in-progress work.
- **`/exit` inside claude ends the session.** Next time you open the workspace it'll be a fresh claude with no memory. Use this only when you really want to reset.
- **Hop terminals freely.** Open a workspace from a different machine — the old client is bumped back to its menu and the new one takes over the same live tmux session. claude state is preserved.
- **Long tasks survive disconnects.** "Go fix this bug, run the tests" → close cloudcode → come back later, it's done (or still running).
- **OAuth stays on the agent host.** The laptop only sees PTY bytes. Credentials never leave where you ran `claude /login`.
- **Don't `tmux kill-server` on the agent.** That nukes all running sessions for every workspace. Daemon restarts (`cloudcode-agent daemon restart`) are safe.

## Workspace sandbox (macOS, opt-in)

`[sandbox] enabled = true` in `agent.toml` wraps each spawned claude in a Seatbelt sandbox:

- writes only inside the active workspace plus a few cache dirs
- secrets (`~/.ssh`, `~/.aws`, `~/.gnupg`, Keychain), shell init files, `.git/hooks/`, `~/Library/LaunchAgents`, camera, microphone all denied
- cross-user and cross-workspace reads denied
- network stays open

Off by default. Opt in once you've confirmed your project's tooling runs under it. Linux support is coming.

## Recording

Every session is recorded as an asciinema cast on the agent:

```
~/.local/state/cloudcode/agent/recordings/<account>/<workspace>/<session>.cast
```

Replay with `asciinema play <file>`. Only output is captured — keystrokes aren't, so pasted tokens stay out of the archive.

## Configuration reference

[`hub.example.toml`](hub.example.toml) · [`agent.example.toml`](agent.example.toml)

Daemon logs: `~/.local/state/cloudcode/{hub,agent}.log`. Lifecycle: `cloudcode-{hub,agent} daemon {status,stop,restart}`.

## Architecture

[`docs/architecture.svg`](docs/architecture.svg) for the network diagram. tl;dr: agent dials out to the hub over WSS (NAT-friendly), hub relays PTY traffic between client and agent.

## Acknowledgements

The macOS Seatbelt sandbox design was inspired by [boxsh](https://github.com/xicilion/boxsh)'s approach to running AI coding agents inside OS-enforced isolation. Implementation is original; no boxsh code is included (boxsh is GPL v3).

## License

MIT. Provided as is, without warranty. The authors are not liable for any use that violates third-party Terms of Service.
