# Per-workspace env vars & launch args config — Design

Date: 2026-06-03
Branch: `dev`
Status: Approved, ready for implementation

## Goal

Let users configure, before each tool (claude/codex) launches:

1. **Environment variables** — currently impossible to set custom env; some
   features are gated behind env vars.
2. **Launch args** — already configurable globally per-tool; extend to
   per-workspace.

Both must be configurable at **two levels**:

- **Global (account-level)** — in the existing webterm Settings dialog.
- **Per-workspace** — each opened workspace can be customised independently.

## Decisions (locked during brainstorming)

| Decision | Choice |
|----------|--------|
| Who controls env / where stored | Users set freely; stored in hub db `user_preferences` (same channel as launch args). No operator allowlist. |
| Global vs per-workspace layering | Per-workspace **inherits from global**, but once customised it **forks a snapshot** and becomes independent (can add/change/**delete** inherited entries). Later global edits do NOT flow into already-customised workspaces. |
| env per-tool? | **No** — env is shared across all tools (one map per level). |
| args per-tool? | **Yes** — keep existing `toolArgs[tool]` shape, at both levels. |
| How changes take effect | **Next tool launch.** Running sessions are untouched. UI offers an explicit **"restart this workspace to apply"** button; no auto-restart. |
| Restart mechanism | **Reuse the existing `ResetWorkspace` path** (`tmux kill-server`; conversation survives via `claude --continue` on next open). No new restart protocol message. |

## Current state (reference)

- Launch args: stored per-account in hub db `user_preferences` (opaque JSON
  blob), schema owned by webterm `webterm/src/lib/preferences.ts`. Wire shape
  today: `{ tool_args: { claude: [...], codex: [...] } }`. Edited in
  `webterm/src/components/SettingsDialog.tsx`. Read/written via
  `api/preferences` GET/PUT.
- On open-session, webterm sends `claude_args`; hub merges with the account's
  stored `tool_args` in `crates/hub/src/pty_session.rs` (OpenSession handler,
  ~646-800; `args_from_user_preferences`, blob parse ~1229-1257) and sends
  `ServerMsg::PtyOpen { session_id, account, workspace, cols, rows,
  claude_args, sandbox, sandbox_mode, tool }`.
- Agent receives `PtyOpen` in `crates/agent/src/tunnel.rs` (~302-326), runs
  `open_session` in `crates/agent/src/pty.rs` (~505-895). Tool is spawned via
  a bash `TOOL_WRAPPER`; agent sets a few fixed env vars at `pty.rs:767-799`
  (`cmd.env("CLOUDCODE_TOOL", …)` etc.). **No custom env injected anywhere.**
- Workspace = `<workspace_root>/<account>/<workspace>/` dir + a persistent
  per-workspace tmux server (`-L cc-<account>-<workspace>`). Hub table
  `workspaces(account, agent, name, created_at)`.
- `workspace_reset` (`pty.rs:1241`): `tmux kill-server` for that workspace;
  keeps files and `~/.claude/projects/<cwd>/` so `--continue` resumes.
- `agent.toml` is read once at startup; no hot reload. (We do NOT touch
  `agent.toml` for this feature — all user config lives in the hub blob.)

## Data model

Extend the per-account `user_preferences` JSON blob. webterm owns the schema
(`preferences.ts`) and parses defensively (bad data → defaults).

Wire shape (snake_case on the wire, camelCase in TS):

```jsonc
{
  "tool_args": { "claude": ["--model", "..."], "codex": [] },  // global, existing
  "env": { "ANTHROPIC_BASE_URL": "https://…", "FEATURE_X": "1" }, // global, NEW
  "workspaces": {                                                // NEW
    "<agent>/<workspace>": {
      "env": { "...": "..." },
      "tool_args": { "claude": [...], "codex": [...] }
    }
    // key present  => workspace has been customised (forked snapshot)
    // key absent   => workspace inherits global live
  }
}
```

- **Workspace key** = `"<agent>/<workspace>"` because the `workspaces` table
  PK is `(account, agent, name)` — the same workspace name can exist on
  different agents.
- **Snapshot-fork on first edit**: when the user first edits a workspace's
  config, the client copies the *current effective global* `{ env, tool_args }`
  into `workspaces[key]`, then edits proceed against that copy. Afterwards the
  workspace is independent.
- **Deletion of inherited entries** is naturally expressible: the forked copy
  is the full set, so removing a key from it removes it for that workspace
  regardless of what global has.
- No new DB table. No new hub API route — reuse `api/preferences` GET/PUT.

## Effective-config resolution (hub, at OpenSession)

Given an `OpenSession { workspace, agent, tool, claude_args? }` for `account`:

```
blob   = load user_preferences(account)
wsKey  = `${agent}/${workspace}`
ws     = blob.workspaces?[wsKey]            // may be undefined

effectiveEnv  = ws ? ws.env             : blob.env             // map (no merge needed: ws is a full snapshot)
effectiveArgs = ws ? ws.tool_args[tool] : blob.tool_args[tool] // per-tool array
```

- If the client explicitly passes non-empty `claude_args` on OpenSession,
  that still wins for args (preserve current behaviour); otherwise use
  `effectiveArgs`.
- `effectiveEnv` always comes from stored config (clients don't pass env).
- Defaults: missing `env` → `{}`; missing `tool_args[tool]` → `[]`.

Because per-workspace is a full snapshot, the hub does NOT merge global+ws —
it picks one or the other. This matches the "fork, then independent" decision
and keeps resolution trivial.

## Protocol change

Add one field to the hub→agent open message:

- `ServerMsg::PtyOpen { …, env: HashMap<String, String> }`
  - hub side: `crates/hub/src/pty_proto.rs` (PtyOpen definition) + populate in
    `pty_session.rs` OpenSession handler with `effectiveEnv`.
  - agent side: `crates/agent/src/tunnel.rs` PtyOpen variant gains `env`,
    threaded into `open_session`.
- Serde default `#[serde(default)]` on `env` so an older hub talking to a newer
  agent (or vice-versa) degrades to "no extra env" instead of failing to parse.
- No change to client↔hub OpenSession for env (clients never send env). The
  webterm OpenSession message is unchanged except it continues to send
  `claude_args` as today.

## Agent-side env injection

In `crates/agent/src/pty.rs` `open_session`, alongside the existing
`cmd.env("CLOUDCODE_TOOL", …)` block (~767-799), iterate the `env` map and
`cmd.env(k, v)` for each entry.

- Inject into the process that ultimately runs the tool, i.e. it must take
  effect **inside** the sandbox wrapper (env applied to the wrapped tool
  process, not just the outer tmux). Verify against the sandbox-exec path
  (~689-716) — set env on the command that exec's the tool wrapper.
- Validate keys defensively agent-side too (skip keys failing
  `^[A-Za-z_][A-Za-z0-9_]*$`); values pass through verbatim.
- Order: apply user env first, then the fixed `CLOUDCODE_*` vars, so internal
  vars cannot be silently clobbered by user config.

## Take-effect / restart

- Saving config never disturbs a running session.
- New env/args apply on the **next** tool spawn for that workspace.
- UI shows "Saved — applies next launch" and a **"Restart workspace to apply
  now"** button that sends the existing `reset_workspace { name, agent }`.
  After reset the next reattach relaunches the tool with new env/args and
  `--continue` restores the conversation.
- UI copy must distinguish this from the destructive framing of the existing
  right-click "Reset" (which users think of as "clear history"). Functionally
  identical; wording differs.

## UI

### Global — `SettingsDialog.tsx`

Add an **Environment variables** section below the existing per-tool
"Default args". A key/value row editor (add/remove rows). Launch args section
stays as-is.

### Per-workspace — new Config panel

- Add a **"Config"** item to the workspace right-click context menu in
  `webterm/src/components/AgentTree.tsx` (next to Files / Reset / Delete).
- Opens a dialog scoped to that `(agent, workspace)`, reusing the same form
  components as the global dialog: **Environment variables** block + **Launch
  args (per tool)** block.
- Title makes inheritance explicit, e.g. *"workspace: <name> — inherits global
  until you edit, then independent."*
- First edit triggers the snapshot fork (copy effective global into
  `workspaces[key]`).
- Footer: Save + "Restart workspace to apply now".
- Optional nicety (YAGNI unless cheap): a "Reset to global" action that deletes
  `workspaces[key]` so the workspace re-inherits global. Include only if it
  falls out naturally.

### Validation (shared form helpers)

- env key: `^[A-Za-z_][A-Za-z0-9_]*$`; empty key rows dropped on save.
- env value: any string.
- args: existing whitespace-split text input (`textToArgs`), unchanged.

## Out of scope (YAGNI)

- Operator allowlist / env gating in `agent.toml`.
- Per-tool env.
- Live env mutation of a running process (physically impossible).
- agent.toml hot reload.
- Quoted-arg parsing (existing limitation, unchanged).
- Secret masking/encryption at rest (values stored plaintext in the blob, same
  trust model as today's args). Note in UI that values are stored as entered.

## Implementation breakdown (for parallel subagents)

Shared contracts (define first, everyone codes against these):

- **Wire shape**: the JSON above (`env`, `workspaces[key].{env,tool_args}`),
  `key = "<agent>/<workspace>"`.
- **Protocol**: `PtyOpen` gains `env: HashMap<String,String>` with
  `#[serde(default)]` on both hub and agent sides.

Layers (mostly independent once contracts are fixed):

- **L1 — Agent (Rust)**: add `env` to `tunnel.rs` PtyOpen; thread into
  `open_session`; inject via `cmd.env` at pty.rs ~767-799 inside the sandbox
  path; key validation; ordering (user env before CLOUDCODE_*).
- **L2 — Hub (Rust)**: add `env` to `pty_proto.rs` PtyOpen; in
  `pty_session.rs` OpenSession, compute effective env/args via the resolution
  rule (workspace snapshot else global) and populate `PtyOpen`. Blob parsing
  stays opaque pass-through (hub doesn't need the schema beyond reading
  `env`/`tool_args`/`workspaces`).
- **L3 — webterm (TS)**: extend `preferences.ts` (types, parse, serialize for
  `env` + `workspaces`); snapshot-fork helper; env section in
  `SettingsDialog.tsx`; new per-workspace Config dialog + AgentTree context
  menu item; wire "Restart to apply" to existing `reset_workspace`.

Dependency notes: L1 and L2 share only the `PtyOpen.env` field name/type —
fix that first. L3 shares the wire shape with L2's blob reads. L1 can proceed
fully in parallel once the field is agreed.

## Verification

- `cargo test` (agent + hub).
- `cd webterm && npx tsc -b && npx vite build`.
- `cd admin-ui && npm run build` (only if touched — likely not).
- Manual (Pete validates on `dev`): set a global env var, open a workspace,
  confirm it reaches the tool (e.g. `env | grep` inside the session); customise
  a workspace's env/args, confirm fork independence (changing global doesn't
  change it); delete an inherited key in a workspace and confirm it's gone;
  hit "Restart to apply" and confirm new env present + conversation resumed.

## Release

Per project flow: after Pete validates on `dev`, merge to `main`, bump
`Cargo.toml` (MINOR — new feature), commit, tag `vX.Y.0`, push tag (CI
publishes). Do NOT merge/tag until Pete confirms.
