# CloudCode User Guide

A detailed walkthrough of every CloudCode component. If you're just trying it out, the [README Quick Start](../README.md#quick-start) gets you running in 5 minutes — come back here when you want to actually use the thing day-to-day.

- [Overview](#overview)
- [Architecture](#architecture)
- [Installation](#installation)
- [Configuration](#configuration)
- [Web UI (webterm)](#web-ui-webterm)
- [CLI client](#cli-client)
- [Multi-tool: running claude + codex side-by-side](#multi-tool-running-claude--codex-side-by-side)
- [Workspaces](#workspaces)
- [macOS sandbox](#macos-sandbox)
- [Admin UI](#admin-ui)
- [Self-update](#self-update)
- [Troubleshooting](#troubleshooting)

---

## Overview

CloudCode lets you drive `claude` (and other terminal AI tools) running on one host from anywhere. The remote terminal **is** the native claude TUI — slash commands, todos, diffs, permission prompts all work — because CloudCode just streams raw PTY bytes through a hub.

**One user, one subscription, one agent.** Sharing a single Claude Max / Pro account across users violates Anthropic's ToS. CloudCode is designed for **solo use**: your accounts on the hub are *your* devices, not other people.

---

## Architecture

```
[ your laptop ] ── client (raw PTY pump) ──┐
                                            │
[ your phone  ] ── webterm SPA (browser) ──┤
                                            │            ┌─────────┐
                                            │            │ tmux +  │
                                            └──► hub ◄───┤ claude  │  agent
                                                  │      │ in each │  (your login host)
                                                  │      │ workspace│
                                                  ▼      └─────────┘
                                            ┌─────────────────┐
                                            │ admin UI + ACL  │
                                            │ accounts, audit │
                                            └─────────────────┘
```

| Component | Where it runs | What it does |
|---|---|---|
| **hub** | A public-facing host (cloud VM, home server with port-forwarding, …) | The gateway. Accepts WS connections from agents (they dial out, NAT-friendly) and from clients (browser SPA + CLI). Multiplexes PTY bytes across agents on a per-session uuid. Ships the admin SPA + the user webterm SPA bundled in the binary. |
| **agent** | The host where you ran `claude /login` | Holds the OAuth credentials. Spawns `tmux + claude` per workspace, streams the PTY back. Workspaces persist across disconnects. |
| **client** (CLI) | Your laptop / any SSH terminal | Raw-mode PTY pump. Picks an agent + workspace via a TUI menu, then hands stdin/stdout straight through to the agent's PTY. |
| **webterm** (SPA) | Your browser | Same experience as the CLI client, plus multi-tool split panes, drag-select-to-clipboard, mouse scrollback, per-user defaults. Lives at `/` on the hub. |

The agent **dials out** to the hub over WSS. The hub never reaches in to agents — that's what makes the hub deployable on a public IP while agents stay on workstations behind NAT.

---

## Installation

Three binaries, three install commands. All three are also bundled as a single `cargo build --release --workspace` if you'd rather build from source.

### Hub (run-once, then runs as daemon)

```bash
curl -fsSL https://raw.githubusercontent.com/initialz/cloudcode/main/install.sh | sh -s -- hub
cloudcode-hub --init                              # writes hub.toml + prints two tokens
$EDITOR hub.toml                                  # set [server].listen, [admin].listen, TLS reverse-proxy, …
cloudcode-hub daemon start --config ./hub.toml
```

`--init` prints two tokens **once**:
- **Admin token** — log into the admin UI with this. Save it; if you lose it the only recovery is re-`--init` (which voids existing accounts in `hub.toml`).
- **Agent registration token** — every agent operator pastes this into their `agent.toml` to register. Same token for all agents.

For production deployments, put `cloudcode-hub` behind a TLS reverse proxy (caddy / nginx). Don't expose the `admin` listener on plain HTTP.

### Agent (one per login-host)

```bash
curl -fsSL https://raw.githubusercontent.com/initialz/cloudcode/main/install.sh | sh -s -- agent
cloudcode-agent --init                            # writes agent.toml template
$EDITOR agent.toml                                # set hub.url + paste registration_token
cloudcode-agent daemon start --config ./agent.toml
```

Must be the same OS user that ran `claude /login`. The OAuth credentials live in macOS Keychain / Linux keyring; the agent reads them from there, never copies them across hosts.

Required on the agent host: `tmux` (any 3.x). The agent fails fast if `tmux` isn't on PATH.

### CLI client (per device)

```bash
curl -fsSL https://raw.githubusercontent.com/initialz/cloudcode/main/install.sh | sh -s -- client
cloudcode --init                                  # writes ~/.config/cloudcode/config.toml
$EDITOR ~/.config/cloudcode/config.toml           # set hub URL + your account token
cloudcode
```

Your **account token** is per-person, issued by whoever runs the hub (admin UI → Accounts → Create, or `cloudcode-hub gen-token <name>`).

### From source

Needs Rust ≥ 1.74, Node ≥ 20 with `pnpm`, and `tmux` on the agent host.

```bash
git clone https://github.com/initialz/cloudcode.git
cd cloudcode
(cd admin-ui && pnpm install && pnpm build)       # admin SPA
(cd webterm   && pnpm install && pnpm build)      # user-facing SPA
cargo build --release --workspace
```

Binaries land in `target/release/{cloudcode-hub, cloudcode-agent, cloudcode}`. Both SPA bundles get baked into `cloudcode-hub` via `rust-embed`; rebuild the hub binary after editing webterm or admin-ui source to ship the changes.

---

## Configuration

Most settings have sane defaults. The fields below are the ones you actually touch.

### `hub.toml`

```toml
[server]
listen = "0.0.0.0:7100"                           # WS endpoint for agents + clients
audit_log = "./audit.jsonl"

[agents]
registration_token_hash = "$argon2id$..."         # written by --init

[admin]
listen = "0.0.0.0:7101"                           # admin SPA + REST API
db_path = "./cloudcode-hub.db"
token_hash = "$argon2id$..."                      # admin login token, written by --init

[[accounts]]
name = "alice"
token_hash = "$argon2id$..."                      # seed account; subsequent edits via admin UI
```

### `agent.toml`

```toml
[hub]
url = "wss://hub.example.com/v1/agent/ws"

[agent]
# name = "peter-mbp"                              # auto = "<hostname>-<user>"

[auth]
registration_token = "ag_..."                     # plaintext, paste from hub --init

[claude]
# workspace_root = "~/cloudcode-agent/workspaces" # per-workspace subdirs live here

[tools]
default = "claude"                                # what the first pane runs

[tools.claude]
executable     = "claude"
resume_command = "claude --continue"              # empty string = never resume, always fresh

[tools.codex]                                     # optional second tool
executable     = "codex"
resume_command = ""

[recording]
# dir       = "~/.local/state/cloudcode/agent/recordings"   # asciinema *.cast per session
# keep_days = 0                                              # 0 = forever
```

The `[sandbox]` block is deprecated — sandbox is now a per-account toggle in the admin UI.

### `~/.config/cloudcode/config.toml` (CLI)

```toml
[hub]
url = "wss://hub.example.com/v1/pty/ws"

[auth]
account_token = "cc_..."                          # your personal account token
```

---

## Web UI (webterm)

Open `https://<hub>/`, paste your account token, you're in.

### Layout

```
┌──────────────┬────────────────────────────────────────────────┐
│ Sidebar      │ Tab bar                                       │
│  Agents      │ [agent·workspace·tool] [agent·workspace·tool] │
│  Workspaces  ├────────────────────────────────────────────────┤
│              │                                                │
│              │ claude / codex pane(s)                         │
│              │                                                │
└──────────────┴────────────────────────────────────────────────┘
```

### Sidebar

- Click an agent to expand its workspaces. Each workspace has a status dot:
  - green = a session is currently live somewhere
  - yellow = saved (tmux still running, no client attached)
  - grey = blank (workspace exists on disk but no tmux)
- Right-click an agent row to create a new workspace under it.
- Hover a workspace row for the **reset** (clear tmux state, keep files) and **delete** (wipe everything) icons.

### Tabs + multi-tool

- Click a workspace to open it as a tab. Tab labels show `agent·workspace·tool`.
- The "+" / split icon on the active tab opens a dropdown of installed tools; hover one and a sub-menu offers two split directions:
  - **→ Right** (`tmux split-window -h`) — new pane to the right.
  - **↓ Down** (`tmux split-window -v`) — new pane below.
- Right of the split icon is the **layout** icon (two rectangles). It re-arranges every pane in the session into one of tmux's preset layouts:
  - **↦ Side by side** (`even-horizontal`)
  - **↧ Stacked** (`even-vertical`)

Each pane runs a separate tool (claude or codex) inside the same tmux session, sharing the workspace directory. The tools talk to each other through the filesystem — handy for "have codex generate a test file, then have claude review it" workflows.

### Selecting text & copying

The webterm enables tmux mouse mode so you can:

- **Scroll the wheel** to scroll back through tmux's per-pane scrollback (50 000 lines per pane).
- **Click-drag** to select. The selection stays visible after release.
- **Cmd+C** to copy to the system clipboard. Click anywhere else to cancel.

Behind the scenes:
- tmux emits the selection through an OSC 52 escape (`set-clipboard on`).
- The SPA registers an OSC 52 handler that base64-decodes (with proper UTF-8 reinterpretation, so Chinese / emoji round-trip cleanly) and calls `navigator.clipboard.writeText`.
- The Cmd+C → tmux bridge tracks drag state in the SPA so a stray Cmd+C while no selection exists doesn't inject a `y` into claude's input.

### Settings → Default args per tool

Stored server-side per account (sqlite blob, `PUT /api/preferences`). Edits commit on focus-out from each input.

```
Default args
  claude   --model claude-3-opus
  codex    --quiet
```

These args ride along on every `open_session` / `split_pane` for that tool. The same blob is used by the CLI client too — see [CLI client → Default args](#default-args-from-webterm-preferences) below.

### Theme

System / Light / Dark. The setting persists to `localStorage`.

---

## CLI client

`cloudcode` (alias for `cloudcode-client`) is a raw-mode PTY pump. Run it and you get a TUI menu:

1. **Agent picker** — list of agents you've been granted access to. ↑/↓ + Enter, or q to quit.
2. **Workspace picker** — for the chosen agent. Same keys, plus:
   - `c` create a new workspace
   - `r` reset (kill tmux + clear claude history; files untouched)
   - `d` delete (wipes everything)
   - Esc back to the agent picker
3. **Live session** — direct PTY relay. claude/codex's full TUI runs here.

### Persistent sessions

`/exit` claude → you land back in the workspace picker for the same agent. The tmux session stays alive on the agent. Reopen the workspace and tmux re-attaches you to wherever claude was. Close your laptop, lose Wi-Fi, jump to a phone — it's still running.

### Quit semantics

- Quitting at the **agent picker** clears `last_agent`. Next launch starts on the agent picker again. Use this when you want a clean restart.
- Quitting at the **workspace picker** keeps `last_agent`. Next launch lands on that agent's workspace picker.
- Returning from claude/code to the workspace picker keeps `last_agent` too.

### Default args from webterm preferences

The CLI doesn't have its own preferences UI. Instead, if you don't pass `cloudcode -- <args>`, the hub merges in the args you set under **webterm → Settings → Default args** for the relevant tool. Pass anything on the CLI (`cloudcode -- --model …`) and your CLI args win — server prefs are ignored for that run.

Looking up by tool:
- `cloudcode --tool codex` → uses webterm's `codex` args.
- `cloudcode` (no `--tool`) → uses webterm's `claude` args (CLI's default tool).

### State files

The CLI keeps two breadcrumbs under `~/.local/state/cloudcode/cli/`:
- `last_agent` — name of the most recent agent you opened a workspace on
- `<agent>/last_workspace` — most recent workspace per agent

Delete them to forget. The CLI re-creates them on the next successful open.

---

## Multi-tool: running claude + codex side-by-side

CloudCode v1.10 generalised the agent's spawn path to handle any number of CLI-shaped AI tools. Each is declared in `agent.toml`:

```toml
[tools]
default = "claude"

[tools.claude]
executable     = "claude"
resume_command = "claude --continue"
extra_args     = []

[tools.codex]
executable     = "codex"
resume_command = ""                                 # codex doesn't support resume
extra_args     = []
```

| Field | Meaning |
|---|---|
| `executable` | Argv-0 the wrapper execs. Absolute path or a PATH lookup. |
| `resume_command` | Shell snippet run on **re-attach** to a workspace whose tmux is still alive. Empty string = always relaunch fresh. The wrapper additionally gates `--continue` for claude on the presence of `~/.claude/projects/<encoded-cwd>/*.jsonl`, so a workspace that exited before claude wrote any history doesn't get stuck in an empty resume. |
| `extra_args` | Always-appended args. Layered like `[<extra_args> <per-session args>]`, where per-session args come from the CLI's `--` passthrough or webterm preferences. |

**Tool installation is your problem** — CloudCode just runs the binary. Make sure `claude` and `codex` are installed and logged-in as the same OS user that runs the agent.

The first pane of a workspace runs whichever tool the client asked for, falling back to `[tools].default`. Subsequent split panes can run any other declared tool. Splitting from webterm picks the tool via the dropdown; from the CLI you'd just open new tabs in the workspace picker.

---

## Workspaces

A workspace is a per-account, per-agent named slot. Each maps to:

- A directory under `[claude].workspace_root` (default `~/cloudcode-agent/workspaces/<account>/<workspace>`)
- A tmux session keyed `cloudcode-<account>-<workspace>`, run on a per-workspace tmux server (`-L cc-<account>-<workspace>`)
- One claude conversation history per workspace under `~/.claude/projects/<encoded-cwd>/*.jsonl`

### Lifecycle

| Action | What it does |
|---|---|
| Create | Just makes the directory. No tmux, no claude. |
| Open | Spawns the per-workspace tmux server if it isn't running. tmux runs the wrapper script which launches the configured tool. Re-attach if the tmux is already alive. |
| Reset (`r` in the picker, or webterm context menu) | Kills the tmux server, wipes the claude conversation history file. The workspace dir is untouched — files stay. Next open starts fresh. |
| Delete (`d` / context menu) | Kills the tmux server **and** removes the workspace directory + claude history. Irreversible. |

### Persistence across version upgrades

Per-workspace tmux servers survive agent upgrades. The agent's self-update kills the agent process; tmux servers (independent processes) keep running. When the new agent starts, opening a workspace re-attaches to the existing tmux.

### Recordings (asciinema)

Every session is teed to `~/.local/state/cloudcode/agent/recordings/<account>/<workspace>/<session-id>.cast`. Output-only — no keystrokes are recorded. Replay with `asciinema play <file>`. Set `[recording].dir = ""` to disable.

---

## macOS sandbox

Opt-in per account, configured in the admin UI (Accounts page → click the sandbox toggle). When **on**:

- Each spawned `tmux + claude` runs inside `sandbox-exec` with a Seatbelt profile that:
  - Allows read+write **only** within that workspace's directory and the system "world-readable" surface (`/usr`, `/Library`, …)
  - **Denies** access to `~/.ssh`, the macOS Keychain, sibling workspaces, other accounts' workspaces
  - Leaves the **network open** — claude / codex need it
- The sandbox is per-workspace, so a compromised tool in workspace A can't reach workspace B's files.

Caveat: claude needs to authenticate via `~/.claude/` (config + OAuth tokens). The profile allows the specific paths claude needs at startup; if a future claude version moves them, the agent log will show "denied" entries.

Linux sandbox support: not yet. The toggle is server-side, so flipping it on for a Linux agent surfaces a clear error on the next open instead of silently spawning unsandboxed.

---

## Admin UI

Lives at `https://<hub>:7101/admin/`. Log in with the admin token from `cloudcode-hub --init`.

### Accounts

- Create / disable / delete accounts. Each gets its own login token (regenerable).
- Per-account sandbox toggle. Edits apply on the next `OpenSession`.
- Account → Agents column shows the agent-allowlist (which agents this account can connect to).

### Agents

- List of agents that have ever registered. Online status, last-seen, host info.
- Per-agent account-allowlist (which accounts can use this agent).
- "Two-way strict whitelist" — both `account → agent` and `agent → account` must allow before a session can open.

### Workspaces

Cross-account inventory of every workspace on every agent. Filter by agent / account, see tmux-alive status, last-opened-at, who-opened.

### Sessions

Historical session log. Start time, end time, duration, end reason. Click into a session to see its asciinema recording (served straight from the agent's filesystem).

### Audit

Append-only event log: account creation, ACL changes, sandbox toggles, etc. Filterable by kind / account / agent / time range.

---

## Self-update

Agents check the hub's `/release.json` periodically. When a newer version is available and the operator has clicked "Update <agent>" in the admin UI's Agent detail page, the supervisor exits cleanly and re-execs the new binary. tmux servers + their claude sessions are independent and survive the swap.

`cloudcode-agent reset-binary` clears the `agent/current` symlink to undo a self-update — useful if a new release is broken; the supervisor will boot from the installed binary on PATH next time.

The hub itself does NOT self-update — it's deployed manually (you control the public-facing surface).

---

## Troubleshooting

| Symptom | Likely cause |
|---|---|
| Webterm hangs at "Loading…" | hub is running but the WS endpoint isn't reachable. Check reverse proxy WS upgrade headers. |
| `cloudcode` CLI shows "agent not allowed" | Admin UI → Accounts → your account → grant access to the agent. |
| Workspace open shows "sandbox not supported" | You toggled sandbox on for an account, but the agent platform (Linux) can't deliver it. Toggle it off for now or run the agent on macOS. |
| Mouse wheel doesn't scroll | The workspace's per-workspace tmux server was started before v1.10 and doesn't have `mouse on`. Reset the workspace (`r`). |
| Cmd+C in webterm doesn't copy | You're on plain HTTP and not localhost. `navigator.clipboard.writeText` requires a secure context — use HTTPS or localhost. |
| Webterm shows old chat content above the new one | Same root cause — pre-v1.10 wrapper script. Reset the workspace. |
| CLI shows previous claude session "behind" the new one | Same fix as above. v1.10's wrapper clears the pane before detach. |
| Default args set in webterm don't apply to CLI | The workspace's tmux is still alive from before you set the args — args are applied on **fresh boot only**. Reset the workspace. |
| Self-update never fires | The supervisor wraps the running agent. Without `cloudcode-agent supervise` (or `daemon start`, which auto-supervises), updates are no-ops. |

For more, open an issue at [github.com/initialz/cloudcode](https://github.com/initialz/cloudcode).
