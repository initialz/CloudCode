# Session Handoff — 2026-06-01

Pete is moving to another machine. This doc captures the in-flight work
so the next Claude session can pick up without re-tracing history.

## 1. Where the repo stands

- Branch: `main`
- Latest tag: **v1.24.1** (released 2026-05-30)
- Latest commit: `22fb87f` — `install.sh: POSIX sh compatible (fix Ubuntu dash failure)`
  - Post-release patch (no tag, lives on main)

Latest releases (newest first):

| Tag      | What it shipped                                                              |
|----------|------------------------------------------------------------------------------|
| v1.24.1  | Windows `install.ps1` (WSL2 bootstrap); POSIX `install.sh` fix               |
| v1.24.0  | First-time tutorial in webterm + Settings → "Show tutorial again"           |
| v1.23.1  | Sidebar tweaks: "Cloudcode" → "CloudCode"; drop Open from context menu      |
| v1.23.0  | **Invite links** + **three-mode sandbox** (strict/permissive/off) + much more|
| v1.22.0  | Folder upload (picker + drag&drop)                                          |
| v1.21.0  | CLI native scroll; workspace reset preserves history                        |
| v1.20.x  | Smart reattach (jsonl stop_reason), CLI alt-screen fix, sandbox relaxations |

## 2. Active investigation — Windows webterm scroll issue

**Status: IN PROGRESS, awaiting screenshot from user.**

### User setup

- Host: Windows 10/11, prompt: `DESKTOP-9ITPMFN`
- WSL2 (Ubuntu) running cloudcode-agent v1.24.1
- Hub somewhere (also v1.24.1)
- Webterm in Windows browser (Edge or Chrome, not confirmed)
- claude installed inside WSL via npm (we walked them through this)

### Symptoms (verbatim from user)

1. "没有向上滚动功能" — wheel doesn't scroll up
2. "生成的内容没有实际输出，只有过程说明" — only sees claude's status / tool-use lines, not the actual response body

### Already ruled out

- ✅ Agent v1.24.1 (matches latest)
- ✅ Hub v1.24.1 (so webterm bundle baked into the binary is current)
- ✅ tmux.conf written by agent has correct settings:
  ```
  set -g mouse off
  set-window-option -g alternate-screen off
  set -g terminal-overrides '*:smcup@:rmcup@'
  ...
  ```
- ✅ Live tmux session (`cc-petez-wintest`) shows `mouse off` and
  `alternate-screen off` via `show-options -gw`
- ✅ `terminal-overrides[0] *:smcup@:rmcup@` IS at server scope on
  tmux 3.2a (`set -g` got promoted to server scope automatically).
  An extra `-sa` patch I asked them to apply was no-op since the value
  was already there.
- ✅ User's tmux version: **3.2a** (Ubuntu in WSL2)

### What this means

Both halves of our fix are in place:
- tmux is configured to **not** send alt-screen escapes
- webterm has the escape filter that strips 47/1047/1049 as a backstop

So in theory xterm.js should stay in main screen, scrollback should work,
and the response should render. But it doesn't. Need to actually see what's
happening on the browser side.

### Open asks I sent the user (waiting for reply)

1. **Screenshot** of the broken webterm window (most important).
2. **Test with non-claude output** — `/exit` claude, then run from bash:
   ```bash
   echo "hello world"
   ls -la
   seq 1 50          # does this scroll back?
   tput cols && tput lines
   ```
   This isolates "is it claude-specific?" vs "is xterm broken?"
3. **DevTools → Network → WS → Messages** screenshot during a claude
   response, so we can see what bytes actually arrive in the browser.

### Hypotheses to keep in mind

- Browser-side quirk on Windows (Edge especially can be weird with WS
  binary frames + xterm.js DOM renderer)
- claude TUI uses some control sequence the WSL→Windows pipe drops or
  mangles
- xterm.js DOM renderer has a known issue with certain ConPTY-adjacent
  byte patterns
- Their hub is `1.24.1` per user message, but they didn't say WHERE
  the hub runs — could it be an older one on a different machine? Worth
  re-confirming if other diagnostics dead-end.

### Pick up here

1. If user has now sent the screenshot / diagnostics, look at those first.
2. If still no screenshot, the most useful next step is to send them a
   browser DevTools console snippet that hooks xterm.js's `term.write`
   to log incoming bytes (hex) to the console for a few seconds, so
   we can see the actual stream after the filter ran. We currently
   don't have a clean way to do this because the Terminal instance
   isn't on `window`. Could add a small dev-mode flag to expose it.

## 3. Other in-flight context

### Branch hygiene

- `try/dom-renderer` and `feature/invite-links` branches are merged + deleted.
- Only `main` is active.

### Active session memory notes

The memory store at
`/Users/petez/.claude/projects/-Users-petez-data-vtech-src-vibe-cloudcode/memory/`
already captures the durable things:

- `user_profile.md` — Pete Zha, 中文优先, macOS Apple Silicon, Claude Max
- `feedback_workflow.md` — concise README, confirm before public actions, options+tradeoffs
- `feedback_versioning.md` — MAJOR = architectural, MINOR = features, PATCH = bugfix
- `feedback_release_flow.md` — tag push triggers GH release automatically, no `gh` CLI needed
- `project_cloudcode.md` — architecture overview (now stale on version — needs bump to v1.24.x)
- `project_multitool_backend.md` — older context
- `reference_cloudcode.md` — repo URL, SSH deploy key, claude credentials location

If you do material new work, update these.

### Known small TODOs that came up but weren't done

- `project_cloudcode.md` says v1.13.11; should be refreshed to v1.24.x and
  mention invite links + three-mode sandbox.
- The webterm tutorial only fires once per browser (localStorage `cc_tutorial_seen_v1`);
  no telemetry to know if people skip vs finish. Fine for now.
- `install.ps1` was tested logically but not run end-to-end on a real
  Windows machine (yet). User started installing v1.24.1 and ran into
  the dash/pipefail bug, which we patched on main but didn't re-tag.

## 4. Reference commands

```bash
# Verify everything builds locally:
cargo test
cd webterm && npx tsc -b && npx vite build
cd admin-ui && npm run build

# Release flow (we do this a lot):
# 1. bump version in Cargo.toml
# 2. git add Cargo.toml && git commit -m "Release vX.Y.Z — short reason"
# 3. git tag -a vX.Y.Z -m "Release vX.Y.Z — short reason"
# 4. git push && git push origin vX.Y.Z
# CI auto-builds + publishes the GH release.

# Useful diagnostics:
curl -fsSL "https://api.github.com/repos/initialz/cloudcode/releases/latest" \
  | python3 -c "import sys,json; d=json.load(sys.stdin); print(d['tag_name'], len(d.get('assets',[])))"
```

## 5. How Pete likes to work

- Ship fast, iterate small. Patch releases for tiny tweaks are fine.
- Confirm before public/destructive actions (git push to others' repos,
  bulk delete, etc.) — but normal commits/tags/pushes to our own repo
  are auto-OK.
- Prefer options + tradeoffs over a single "best" answer.
- Use subagents in parallel for independent layers (DB/Hub/Admin UI/Webterm).
- Chinese for chat, English in code/commits.

---

When you take over, read this top-to-bottom, then check `git status` and
`git log --oneline -10` to make sure nothing landed after this doc was
written.
