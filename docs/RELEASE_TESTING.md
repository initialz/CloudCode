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
- [ ] **Settings → default args** Open Settings, set claude args, blur the input. `GET /api/preferences` returns the saved blob. Reset the workspace, open — claude starts with the new args (verify via `--version` for an instant exit).
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

## Test reports

For releases that change behaviour visibly (MAJOR / MINOR, or any
PATCH that touches a self-update / rollback / migration path), drop
a self-contained HTML report under `docs/test-reports/`:

```
docs/test-reports/<YYYY-MM-DD>-<tag>-smoke.html
```

The report is **inline CSS only, no external resources** so it
emails / archives / attaches to a PR cleanly. Structure:

1. **Header**: tag, commit, host, summary verdict (`N/N passed`).
2. **Environment + isolation**: state dir override, ports, "what
   on the host was NOT touched" guarantee.
3. **One section per test**, each with sub-blocks:
   - *Setup* — config knobs / process layout
   - *Action* — exact commands run (in a `<pre class="log">`)
   - *Observation* — log excerpts + artefact tables
   - *Verdict* — coloured banner: PASS / FAIL / WARN
4. **Cross-cutting checks** — invariant list with ✓ / ✗.
5. **Out of scope** — flag what wasn't covered (so the reviewer
   sees the gap explicitly instead of guessing).

Example: [`docs/test-reports/2026-05-17-v1.11.1-smoke.html`](test-reports/2026-05-17-v1.11.1-smoke.html).

**When to generate, when to skip**

- MAJOR / MINOR releases: report is **required**, generated before
  the tag push so it can ride the release commit.
- PATCH releases that touch update / rollback / supervisor /
  migration code: report is **required** for the touched path.
- Pure docs / dependency bumps / UI polish PATCH: skip unless the
  releaser explicitly asks for one.

**Push gate**: after generating, hand the report URL to the
operator. Only push the release tag once they sign off.

## When something fails

1. Capture: hub log, agent log, client / browser console.
2. File against the PR / release in GitHub Issues.
3. Decide hold-vs-ship: if the regression is in a code path the release explicitly touched, hold. Otherwise note it and ship.

## v1.13 hub-managed workspace smoke

v1.13 moves canonical workspace bytes from the agent to the hub
(see the architecture note in `crates/hub/src/workspaces.rs`). The
new pull / push / lock-takeover paths are tricky to walk by hand —
they live across hub + two agents + the agent sync engine — so the
release ships a scripted end-to-end smoke that exercises all five
scenarios in one shot:

```bash
scripts/v1.13-smoke.sh                # build release + run all 5 CASEs
scripts/v1.13-smoke.sh --no-build     # reuse existing release artifacts
scripts/v1.13-smoke.sh --keep-temp    # leave the temp $CLOUDCODE_STATE_DIR for triage
scripts/v1.13-smoke.sh --rebuild-ui   # also rebuild webterm + admin-ui (rare)
```

What it covers:

1. **CASE 1 — create workspace + seed**: `CreateWorkspace` over `/v1/pty/ws`, then a direct write of `README.md` under `<hub-state>/hub/workspaces/alice/demo/`. Confirms `ListWorkspaces` sees it.
2. **CASE 2 — initial pull**: agent-A `OpenSession`. Asserts that `<agent-A>/workspaces/alice/demo/README.md` materialises with the canonical content.
3. **CASE 3 — real-time push**: agent-side append + new file + delete. Asserts the hub canonical copy mirrors each change within ~2 s (`COALESCE_WINDOW = 100ms` + `SCAN_INTERVAL = 500ms` backstop).
4. **CASE 4 — live force-take**: agent-B `OpenSession force=true` against agent-A's lock. Asserts (a) lock moves to B in `workspaces.locked_by_agent`, (b) agent-A's local copy gets rm-rf'd via the live `WorkspaceCleanup` the hub pushes (not just the queued one).
5. **CASE 5 — offline force-take + Welcome drain**: SIGSTOP agent-B (so the hub still sees it as the lock holder), agent-C force-takes (queues a pending cleanup in `pending_workspace_cleanups`), kill agent-B, restart agent-B with a pre-seeded stale local copy. Asserts the agent drains the cleanup on its Welcome frame and the row is gone afterwards.

When to run:

- **MAJOR / MINOR releases** that touch any file under `crates/hub/src/{workspaces,pty_session,ws_handler}.rs`, `crates/agent/src/sync/`, or the v1.13 wire variants in `tunnel.rs` — required.
- **PATCH releases** whose surface area doesn't touch sync — skip.
- After bumping `tokio-tungstenite`, `notify`, or the sqlx feature set — run, since these underpin the pull / watcher / push-queue plumbing.

The script writes its HTML report to `docs/test-reports/<date>-v<tag>-hub-managed-workspace.html`. Push the tag only after that report has a PASS banner.

Implementation notes worth knowing:

- The smoke uses `cloudcode-smoke-ws` (new in v1.13 — `crates/smoke-ws/`), a single-shot WS helper that speaks the user-facing PTY protocol. It's intentionally not on the release manifest; it's a test artifact only.
- macOS's `/var → /private/var` symlink trips up `notify::RecommendedWatcher` (the watcher canonicalises the root and then `strip_prefix` mis-matches against `/var/...` event paths). The script canonicalises `$TMP` via `cd … && pwd -P` so agent.toml's `workspace_root` is in the same form FSEvents emits.
- The script SIGSTOPs an agent in CASE 5 rather than SIGKILLing it, because the hub auto-releases workspace locks on TCP disconnect (`release_all_workspace_locks_for_agent` in `ws_handler.rs`). Without the freeze the force-take path would see the workspace as unlocked and skip the pending-cleanup queue.

## Notes specific to v1.11.0

This release adds hub self-update. The first upgrade from v1.10.x → v1.11.0 must be done **manually** (the still-running v1.10.x daemon isn't yet a supervisor). Subsequent hub upgrades go through the admin UI button. Confirm this caveat is in the release notes.

The hub's supervisor mirrors the agent's. Phase B item 5 (Hub supervise + crash recovery + graceful shutdown) was new in this release; treat it as the highest-priority regression target.
