# Release Smoke Test

A repeatable checklist to walk before publishing a release. Everything in **Phase A** is run automatically by `cargo test` + `cargo clippy` + the per-frontend builds and should be clean on the release branch. **Phase B** is the manual smoke pass; budget ~20 minutes.

## Phase A — automated (must pass on release branch)

```bash
# from repo root
cargo build --release --workspace
cargo test  --workspace
cargo clippy --workspace --all-targets -- -D warnings

(cd admin-ui && npm install && npm run build)
(cd webterm  && pnpm install && pnpm build)

# A v1.11+ release MUST also rebuild the hub after frontend builds so
# the new SPA bundles land in the binary via rust-embed.
cargo build --release -p cloudcode-hub

# Sanity-check version + new subcommands surface.
./target/release/cloudcode-hub  --version
./target/release/cloudcode-hub  supervise --help
./target/release/cloudcode-hub  daemon --help
./target/release/cloudcode-agent --version
./target/release/cloudcode-agent supervise --help
./target/release/cloudcode       --version
```

If `cargo clippy ... -D warnings` was already clean on the previous release, treat any new warning as a release blocker — it points at a real regression.

## Phase B — manual smoke (per release)

Assumes you have a running v(N-1) hub + agent + macOS client + a webterm tab somewhere. Bring the new binaries on a side host, **don't replace the live hub yet** — we want to exercise backward compat first.

### 1. Wire-protocol backward compat (CRITICAL — agents are remote)

The release MUST cross-version with the previous release. If you touched any wire file (`tunnel.rs`, `pty_proto.rs`, `proto.rs`, `wire.ts`), bump `PROTOCOL_VERSION` and document it.

- [ ] **old client → new hub** Run a v(N-1) `cloudcode` CLI against the new hub. Open a workspace, type into claude, `/exit`. No protocol errors in either log.
- [ ] **new client → old hub** Inverse direction. Connect new CLI to old hub. Open + use + exit.
- [ ] **old agent → new hub** Leave a v(N-1) agent connected when upgrading the hub. After hub restart, agent reconnects within a few seconds. Open a workspace from the new client.
- [ ] **old webterm SPA cached in a browser → new hub** Use a stale tab without hard-refresh. Hello frame is rejected with a friendly version-mismatch banner (not a silent hang).

### 2. CLI client

- [ ] **First launch on a clean host** `cloudcode --init` writes config, then `cloudcode` shows the agent picker (no `last_agent` yet).
- [ ] **Quit at agent picker** Hit `q` at the agent picker → next `cloudcode` invocation still lands on the agent picker (v1.10.1+ behaviour).
- [ ] **Quit at workspace picker** Pick an agent, then `q` at the workspace picker → next launch lands on the workspace picker for that agent.
- [ ] **Open + chat + /exit + reopen** Open workspace, talk to claude, `/exit`. Reopen the same workspace — only ONE copy of the chat history is visible (no v1.10.0-era flash / duplicate).
- [ ] **Per-session `--` args win** `cloudcode -- --version` → claude prints version and exits. Webterm-side preferences are ignored when CLI args are non-empty.
- [ ] **Webterm-side preferences fallback** Set claude args in webterm Settings, reset the workspace, then plain `cloudcode` (no `--`) → the args ride along.

### 3. Webterm SPA

- [ ] **Multi-tab open / close** Open two different workspaces in two tabs. Switch between them; each retains its scrollback.
- [ ] **Split + layout** Open a workspace, Split → "Right" with codex. Both panes render. Layout button → "Stacked"; panes flip orientation. Close one pane (`Ctrl+B x` in tmux); session stays alive.
- [ ] **Mouse scroll** Wheel up scrolls into chat history (tmux scrollback). Wheel down past current returns to live.
- [ ] **Drag-select + Cmd+C** Drag a chunk of text; selection highlight stays after release. `Cmd+C` copies. Paste somewhere — full text including CJK round-trips with no mojibake.
- [ ] **Click cancels selection** After a drag, click anywhere in the pane → selection clears, copy mode exits.
- [ ] **Settings → default args** Open Settings, set claude args, blur the input. `GET /app/api/preferences` returns the saved blob. Reset the workspace, open — claude starts with the new args (verify via `--version` for an instant exit).
- [ ] **Cookie auth survives reload** Hard-refresh the page; webterm goes straight to the workspace tree, not the login screen.

### 4. Agent

- [ ] **Sandbox toggle on/off** Flip the per-account sandbox in admin UI. Open a workspace; check `ps -o command= -p <claude pid>` on the agent host for `sandbox-exec` wrapping (on) or its absence (off).
- [ ] **Reset workspace** From CLI or webterm, `r` a workspace. tmux server is killed; next open is a fresh boot with no chat history.
- [ ] **Self-update via admin UI** With a newer release available upstream, click "Update to vX.Y.Z" in the Agents page. Button animates `Updating.` / `..` / `...` for ~30 s. Agent disconnects, then reconnects with the new version. The row's "version" column shows the new tag. Test with the agent running BOTH directly and as `daemon start`.
- [ ] **Self-update rollback path** Manually corrupt `~/.local/state/cloudcode/agent/current` to point at a non-existent binary, restart the supervisor (`cloudcode-agent daemon restart`), expect: after ~10 failed spawns the supervisor flips to `agent/previous` and the agent comes back online.

### 5. Hub

- [ ] **Hub supervise mode** First boot of v1.11+: `cloudcode-hub daemon restart` should re-exec into supervisor. `ps -ef | grep cloudcode-hub` shows two processes — the supervisor and the hub child.
- [ ] **Hub self-update** Click the "Update → vX.Y.Z" badge next to the hub version. Animation runs, page auto-reloads after the hub restarts. New version visible in the header.
- [ ] **Hub crash recovery** `kill -9` the hub child PID (not the supervisor). Within ~1 s the supervisor respawns the child and the admin UI's WS reconnects.
- [ ] **Hub graceful shutdown** `cloudcode-hub daemon stop` → both supervisor and child exit cleanly within the 5 s grace window.
- [ ] **Admin login + new account creation** Log in, create a new account, copy the token, hand it to a fresh `cloudcode --init` flow. Account works end-to-end.

### 6. Admin UI regressions

- [ ] Dashboard renders all charts (sessions, leaderboards, token/message distributions) without console errors.
- [ ] Accounts page: rotate token, disable/enable, sandbox toggle.
- [ ] Agents page: edit allowed accounts, delete an offline agent.
- [ ] Sessions page: row → SessionDetail → timeline of messages renders.
- [ ] Audit page: filter by kind / account; pagination works.

## When something fails

1. Capture: hub log, agent log, client / browser console.
2. File against the PR / release in GitHub Issues.
3. Decide hold-vs-ship: if the regression is in a code path the release explicitly touched, hold. Otherwise note it and ship.

## Notes specific to v1.11.0

This release adds hub self-update. The first upgrade from v1.10.x → v1.11.0 must be done **manually** (the still-running v1.10.x daemon isn't yet a supervisor). Subsequent hub upgrades go through the admin UI button. Confirm this caveat is in the release notes.

The hub's supervisor mirrors the agent's. Phase B item 5 (Hub supervise + crash recovery + graceful shutdown) was new in this release; treat it as the highest-priority regression target.
