# `feature/hub-managed-workspace` — Pause Notes

**Status**: shelved on 2026-05-20 with the heavy-load push reliability
issue still unresolved. The architecture and most UX work landed. The
remaining unknown is why a `git clone` of a multi-thousand-file repo
(reproducer: <https://github.com/angr/angr.git>) still triggers
intermittent WS connection resets even after the spawn_blocking +
ws-tx-slot fixes.

Use this doc as the starting point when picking the branch back up.

---

## What v1.13 was supposed to do

Move canonical workspace ownership from agents to hub:

- Hub holds the source of truth for every account's workspaces on disk
  (`<config_dir>/hub/workspaces/<account>/<workspace>/`) + a
  `workspaces` row per (account, name).
- Agent opens a session → hub streams the canonical files to the
  agent's local working copy (pull stream).
- Agent edits files (via claude / user) → notify watcher → push worker
  → `WorkspacePushFile` frames → hub writes canonical → ack.
- Exactly one agent holds the workspace lock at any time. Another
  agent can `force=true` to take it; hub queues a `WorkspaceCleanup`
  so the old holder rm-rfs its stale local copy.
- UI flow: pick workspace → pick agent (was: pick agent → pick
  workspace). `codex` removed from supported tool list; `claude` only.

## What works (committed)

### Rounds 1–4 (commits 30d8b62 → 115c10c)

- **Hub storage**: `crates/hub/src/workspaces.rs::WorkspaceStorage` —
  per-account/workspace dir, atomic write_file (tmp + rename), strict
  path-segment validation, list/delete with size aggregation.
- **DB schema**: `workspaces` + `pending_workspace_cleanups` tables,
  per-(account, name) lock column, queued cleanup on force-take.
- **OpenSession orchestrator** (`crates/hub/src/pty_session.rs::open_session`):
  workspace-exists → agent-online + ACL → lock inspection → set lock
  → register session → stream pull (`WorkspacePullStart` + N
  `WorkspaceFile` + ack wait) → `PtyOpen` → wait for `PtyOpened` →
  forward `SessionOpened` to client. Every failure path releases
  whatever it acquired.
- **Force-takeover**: queues a pending cleanup for the displaced
  agent, AND pushes a live `WorkspaceCleanup` if the agent is still
  online. Drained on next `Welcome` for offline-then-reconnect case.
- **Wire frames**: `ServerMsg::{WorkspacePullStart, WorkspaceFile,
  WorkspaceFileAck, WorkspaceCleanup}` + `ClientMsg::{WorkspacePullAck,
  WorkspacePushFile, WorkspaceDeleteFile}`. `serde_bytes` for binary
  content (JSON-encoded — see "Known performance footgun" below).
- **Agent sync engine** (`crates/agent/src/sync/`): notify watcher +
  gitignore-style filter + persistent push queue (sqlite) +
  per-session push worker that scans the queue, sends frames, and
  drains acks. Watcher canonicalizes the workspace root (macOS
  `/var → /private/var`).
- **Smoke harness**: `scripts/v1.13-smoke.sh` drives the 5 main
  scenarios (workspace creation → pull → push → live force-take →
  offline force-take + Welcome drain) using the
  `cloudcode-smoke-ws` helper crate.
- **Integration tests**: `crates/hub/tests/workspace_sync.rs` spawns
  a real `cloudcode-hub` subprocess and drives both the agent and
  client wires with `tokio-tungstenite`. 7 cases.
- **Config**:
  - `[workspaces].root` in `hub.toml`, defaults to
    `./hub/workspaces` (anchored to config dir).
  - `[claude].workspace_root` in `agent.toml`, defaults to
    `./agent/workspaces` (anchored to config dir).
  - `cloudcode-daemon::config_sync` — startup-time auto-append of
    commented-out doc blocks for unset optional keys (so users
    discover new knobs without grep-source-tree). Per-binary
    `config_schema.rs` defines the SCHEMA slice.
- **CLI menu UX**: workspace-first picker, agent-second picker,
  decide_initial_selection helper (with 7 unit tests pinning the
  arrow-key behaviour), force-take confirm dialog with proper text
  wrapping + horizontal padding, green dot badge on the agent
  currently holding the lock.
- **TUI error surfacing**: when hub returns `SessionError`, the
  client stashes the message and the next menu run displays it via
  `show_message` instead of `eprintln!`-ing to a torn-down terminal.

### Round 5 (uncommitted; bundled into next commit)

- **Login UX** (admin-ui + webterm + hub): both surfaces now take
  username + token, validated together with a generic
  `invalid_credentials` reply (no probing). Admin UI's username
  field is read-only and fetched from
  `GET /admin/api/login-info`. `[admin].username` defaults to
  `"admin"`. constant-time username comparison.
- **Force-take UX**: hub's `SessionError` got optional `code` +
  `workspace_lock_holder` fields. Client recognises
  `code = "workspace_locked"` and pops a confirm dialog; on
  confirm, retries `OpenSession { force: true }`.
- **Config path anchoring**: relative `workspace_root` in either
  config is resolved against the config file's parent dir at
  `Config::load` time. Survives `daemon restart` from a different
  cwd. Unit tests cover relative-default, relative-explicit, and
  absolute-passthrough cases.
- **Delete dir support**: hub's `workspaces.delete_file` does
  `symlink_metadata` first and dispatches to `remove_dir_all` for
  directories. (The agent watcher emits a single `Remove(Folder)`
  for an `rm -rf` and we used to fail with `EISDIR`.) Regression
  test in `workspaces.rs` covers this.
- **Subtree fan-out on dir Changed**: macOS FSEvents coalesces
  "many files created at once" (e.g. `git clone`) into a single
  parent-directory event. Agent now walks the subtree in
  `tokio::task::spawn_blocking` and enqueues every regular file,
  honouring `IgnoreFilter` and (briefly) a per-file size cap.
  The size cap was removed per user request after they wanted
  full repo sync. Unit test
  `directory_change_fans_out_to_subtree_files` covers it.
- **`.git/` synced by default**: removed from `DEFAULT_IGNORES`. Users
  who don't want it add `.git/` to their per-workspace
  `.cloudcodeignore`. `node_modules/`, `target/`, `dist/`, `build/`
  still ignored.
- **WS frame/message limit**: hub + agent client both raised to
  256 MiB (was 16 MiB default). Bigger frames for large pack files
  / vendored binaries. JSON-encoded content goes through fine.
- **Session survives WS reconnect** (agent side): introduced
  `PtyManager.ws_tx` — an `Arc<RwLock<Option<Sender<OutFrame>>>>`.
  `ws.rs::run_once` calls `bind_ws_tx` on connect, `clear_ws_tx`
  on exit. PTY reader thread + push worker snapshot the slot on
  every send instead of holding the per-WS sender by value. A
  transient WS reset no longer kills the PTY session. Unit test
  `push_survives_ws_reconnect` proves it.
- **peek_oldest scoping** (`push_queue.rs`): the queue is shared
  across all sessions on this agent, so peek_oldest now filters
  by `(account, workspace)` in SQL. The previous global
  ORDER-BY-id behaviour let an orphan row at the head silently
  starve every subsequent scan (the case we hit at one point —
  2000+ enqueues, 0 sends). Regression test
  `peek_oldest_skips_other_sessions_rows` covers it.
- **biased select! + force-scan after watch**
  (`sync/runtime.rs::run_push_worker`): `tick.tick()` is polled
  first, and any successful `handle_watch_event` triggers an
  immediate `scan_and_send`. Without this, a heavy burst of
  watch events kept `watch_rx` ready and starved the periodic
  tick out of ever firing.
- **Hub FS ops in spawn_blocking**: `handle_workspace_push` /
  `handle_workspace_delete` in `ws_handler.rs` now wrap the
  `WorkspaceStorage::write_file` / `delete_file` calls in
  `tokio::task::spawn_blocking`. Originally these ran on axum's
  async runtime threads; under a 5000-frame burst they starved
  the writer task and no acks came back. `WorkspaceStorage`
  derives `Clone` so cloning into the blocking closure is cheap.
- **Diagnostic logs**: `sync::runtime` and `pty::start_session_sync`
  now log `starting workspace sync engine`,
  `workspace watcher running`, `watch event received` (DEBUG),
  `sync: enqueue push`, `sync: sent push frame to hub`,
  `sync: fan out directory event`, `sync: fan out complete` (with
  full counters), `ack reported failure`.

## Known UNRESOLVED issue (the reason we're shelving)

**Under heavy push load (e.g. `git clone` of angr, ~10k files,
including .git pack files)** the agent's WS connection to hub
intermittently resets with `IO error: Connection reset by peer (os
error 54)`. After each reset the agent reconnects within ~1 s
(supervisor handles it), but the user experience is that the session
appears unstable / claude appears to crash.

What we ruled out:

- Frame-size limit (raised to 256 MiB on both sides).
- `peek_oldest` orphan starvation.
- `select!` tick starvation.
- Hub sync FS writes on the async runtime (moved to spawn_blocking).

What I suspect is still wrong (educated guesses, none verified):

1. **Hub-side stale `selected_agent` Arc after agent WS reconnect.**
   `crates/hub/src/pty_session.rs::ConnCtx::selected_agent` is set
   once in `OpenSession` (line ~745). It's an `Arc<AgentConn>`
   pointing at the agent connection that was alive at the time.
   When that agent's WS resets, hub's `handle_socket` for the agent
   side exits and `registry.unregister(&conn)` runs — but the
   client-side `ConnCtx` still holds the original Arc. Subsequent
   PtyInput / Resize from the client go to the dead conn. Even
   after the agent reconnects under the same name, the registry has
   a NEW `AgentConn`, and the client's pointer is dangling.
   Fix idea: look up the current agent connection by name from
   `state.registry` on each forward instead of caching the Arc.
2. **Mirror of the agent's slot trick on hub side.** Hub holds a
   per-agent `AgentConn` with its own outbound `tx`. When the agent
   side's WS hiccups, hub's `AgentConn.send(...)` calls fail
   silently. A mirror of `PtyManager.ws_tx` for hub's per-agent
   connection (so the agent's reconnect rebinds the channel) would
   make client→agent routing resilient too.
3. **JSON-encoded `serde_bytes` is wasteful.** A 1 MiB binary file
   becomes 3-8 MiB on the wire as a JSON array of integers. With
   thousands of frames in flight, this is real bandwidth + memory
   pressure. Switching to a binary frame format (or actual
   `Message::Binary` instead of `Text(serde_json)`) would cut
   network load by 3-8x and reduce the chance of TCP-level
   backpressure-induced RST.
4. **Bundle small files**: for a freshly-cloned repo, the agent
   sees thousands of `Create(File)` events. Each becomes one WS
   frame. The hub processes each independently. Aggregating into
   a tarball / multipart frame would slash overhead.

The last log I saw (`/private/tmp/agent.log`) had 5097 sends, 0
`Connection reset` lines after the spawn_blocking fix, but the user
still reported "agent crashes" — meaning either:

- they didn't fully restart hub + agent with the freshly-built
  binaries (`pkill -f cloudcode-{hub,agent}` + relaunch each time
  is what we kept asking for), OR
- the failure mode changed to a CLIENT-side disconnect (hub→client
  WS dying), which we never instrumented.

**Next session's first action should be: capture both `hub` and
`agent` logs at INFO level during a real `git clone angr` run, and
specifically check whether the reset originates from the agent side
or the hub side, and whether anything on the hub log shows the
connection ending cleanly vs being torn at the TCP layer.**

## Tests at pause

```
86 PASS / 0 FAIL / 1 ignored

  36 cloudcode-agent unit  (pty, sync/{runtime,push_queue,watcher,ignore_filter})
  28 cloudcode-hub unit    (workspaces, config_sync, db, config)
   8 cloudcode-daemon unit (config_sync)
   7 cloudcode-hub integration  (crates/hub/tests/workspace_sync.rs)
   7 cloudcode-client unit (menu::decide_initial_selection)
```

`cargo clippy --workspace --all-targets -- -D warnings` clean.

## Where files moved / appeared

```
NEW:
  crates/agent/src/config_schema.rs
  crates/agent/src/sync/{ignore_filter,push_queue,watcher,runtime}.rs
  crates/client/src/config_schema.rs
  crates/daemon/src/config_sync.rs
  crates/hub/src/config_schema.rs
  crates/hub/tests/workspace_sync.rs
  crates/smoke-ws/                              (test-only WS helper)
  scripts/v1.13-smoke.sh
  docs/test-reports/2026-05-19-v1.13.0-hub-managed-workspace.html
  docs/BRANCH_NOTES_hub_managed_workspace.md    (this file)

DELETED on this branch:
  Anything `codex`-related in agent / hub / webterm / admin-ui.
  Split-pane UI in webterm.
```

## Resuming the branch

```sh
git checkout feature/hub-managed-workspace

# Sync any drift from main if needed:
git fetch origin
git rebase origin/main   # or merge — pick what feels right at the time

# Rebuild from scratch (no incremental cache surprises):
cargo clean -p cloudcode-hub -p cloudcode-agent
cargo build --release --workspace

# Then start by reproducing the failure mode on a fresh machine.
# Run hub + agent in two terminals (RUST_LOG=info on both) and clone
# angr to see where the RST originates this time. Don't go down a
# fix path without that log in hand — every theory we tried without
# both logs ended up being only partly right.
```

## What to NOT lose

The `crates/hub/tests/workspace_sync.rs` integration tests are the
best regression net for the wire protocol. The `scripts/v1.13-smoke.sh`
+ `crates/smoke-ws/` pair is the best end-to-end harness for "does the
whole stack actually work together". Both of these were painful to get
right; don't throw them out if you redesign the protocol.

The agent's `PtyManager.ws_tx` slot pattern is reusable for hub's
agent-side connections (see suspect #2 above).
