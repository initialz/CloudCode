# Idle workspace reaper — Design

Date: 2026-06-08
Branch: `dev`
Status: Approved, ready for implementation

## Goal

Reclaim agent resources from abandoned workspaces. Each open workspace keeps a
`claude` process tree alive (~0.4–0.7 GB RSS each — measured; the per-workspace
tmux server is negligible at ~1–9 MB). When nobody is using a workspace, that
claude RAM is wasted. Reap (kill) the idle session's tmux server — which takes
claude down with it — and rely on the existing smart-reattach + `claude
--continue` to restore the conversation when the user comes back.

## Decisions (locked during brainstorming)

| Decision | Choice |
|----------|--------|
| Reap trigger | No client attached for **≥ 30 min** **AND** claude is idle (`is_claude_idle` — claude at `end_turn`, not thinking/tool-calling). |
| Idle signal | Reuse the existing `is_claude_idle(claude_proj_dir)` (reads claude's jsonl `stop_reason`). Non-claude tools (no jsonl idle signal) are **conservatively NOT reaped**. |
| Threshold | Fixed **30 min** (`const`, not configurable). Reaper tick: **60 s**. |
| How reaped | `tmux -L cc-<account>-<workspace> kill-server` (same mechanism as `workspace_reset`). Conversation jsonl is preserved → `--continue` resumes. |
| Recovery | Existing: next `open_session` finds no server → fresh boot → `resume_command` (`claude --continue`). No change needed. |
| UI | None. The webterm sidebar dot already reflects tmux liveness via `has-session`; it goes grey after a reap and the session resumes on reopen. |

## Architecture

A periodic background task spawned in the agent's `serve()` (alongside
`ws::run`), holding an `Arc<PtyManager>`. Every 60 s it:

1. **Enumerate live per-workspace tmux servers** — reuse the enumeration that
   `workspace_list_all` already does (walk `<workspace_root>/<account>/<workspace>`
   dirs and probe `tmux -L cc-<account>-<workspace> has-session`).
2. **Compute the attached set** — iterate `PtyManager.sessions` (each
   `PtyHandle` carries `account` + `workspace`); these are workspaces with a
   live client session right now.
3. **detached = live servers − attached.** Maintain a
   `detached_since: HashMap<(account, workspace), Instant>` on the manager:
   - First time a workspace is observed detached → record `now`.
   - When a workspace is attached (or its server is gone) → remove its entry.
4. For each detached workspace with `now − detached_since ≥ 30 min`:
   - Compute `claude_proj_dir = jsonl::project_dir(home, <workspace cwd>)` and
     check `is_claude_idle(&claude_proj_dir)`. **Only reap if idle.**
   - Re-check it is still detached (not attached in the moment between scan and
     kill) as a final guard.
   - `tmux -L cc-<account>-<workspace> kill-server`; log it
     (`tracing::info!`). Drop its `detached_since` entry (server is gone).

The decision logic (compute detached set from servers+attached, and the
"due for reaping" predicate over `detached_since` + threshold) is factored into
**pure functions** so they can be unit-tested without tmux/processes.

## Safety

- Attached workspaces are never reaped (excluded from the detached set, plus the
  final re-check).
- Busy claude (thinking / tool-calling) is never reaped — the `is_claude_idle`
  gate. A long autonomous run that finishes and then sits ≥30 min idle WILL be
  reaped (intended); while it's working it won't be.
- Non-claude tools (codex etc.) have no jsonl idle signal → `is_claude_idle`
  returns false → never reaped (conservative; documented gap).
- Agent restart: `detached_since` starts empty, so any pre-existing detached
  server is first observed "now" and gets a full 30-min grace — no reaping
  storm on startup.

## Out of scope (YAGNI)

- Configurable threshold.
- A dedicated "suspended" UI state or user notification (sidebar dot suffices).
- Reaping non-claude tool sessions.
- Manual "suspend now" button (could be a small follow-up; not in this spec).

## Testing

- Unit tests (pure, hermetic) for the decision logic:
  - detached-set computation: servers minus attached.
  - `detached_since` bookkeeping: new detached records a time; re-attach clears.
  - due-for-reaping predicate: under-threshold → no; ≥threshold → yes.
- The tmux/`is_claude_idle`/enumeration parts reuse already-exercised code; the
  reaper loop itself (timer + side effects) is verified by `cargo build` +
  manual check (logs show reaps; reopen resumes via `--continue`).
- Manual (Pete): open a workspace, detach, wait (or temporarily lower the const
  to test), confirm tmux server gets killed only after 30 min idle, confirm
  reopen resumes the conversation, confirm a busy session is NOT reaped.

## Release

Per project flow: after Pete validates on `dev`, merge to `main`, bump
`Cargo.toml` (MINOR — new feature), tag, push. (Agent-only change; the agent
binary must be rebuilt/redeployed — happens via self-update.)
