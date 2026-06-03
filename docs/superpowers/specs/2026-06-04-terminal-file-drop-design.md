# Drag/paste files into the terminal — Design (Phase 1: webterm)

Date: 2026-06-04
Branch: `dev`
Status: Approved, ready for implementation

## Goal

Let users get a local file in front of the remote `claude` by dragging it onto
the terminal (or pasting an image), the way native Claude Code accepts dragged
files. Because claude runs on the remote **agent** while the file lives on the
user's machine, "drop a file" must mean **upload the file into the workspace,
then insert a reference to its agent-side path** — not just paste a local path
(which wouldn't exist on the agent).

Research (claude-code-guide) confirmed: claude loads BOTH text files and images
from a **filesystem path** (text via Read, images as visual content). So once a
file — image or not — is in the workspace and referenced by path, claude handles
it correctly. Our upload+reference path is therefore uniform across file types.

## Phasing

- **Phase 1 (this spec): webterm** — drag files onto the terminal, and paste
  images (Ctrl/Cmd+V). Shared upload→insert pipeline.
- **Phase 2 (separate spec, NOT built here): CLI drag** — intercept the
  bracketed-paste local path in stdin, read the file locally, upload over a new
  client→hub channel, rewrite to the agent path. Different architecture; outline
  in the appendix.

## Decisions (locked during brainstorming)

| Decision | Choice |
|----------|--------|
| What a dropped file becomes | Upload to the workspace, then insert a reference to the agent-side path. |
| Reference form inserted | **`@<workspace-relative-path>`** mention (claude's native "include this file" signal; works for text and images). |
| Upload destination | A dedicated subfolder **`.cloudcode/uploads/`** in the workspace. |
| Naming / conflicts | Keep the original filename; on conflict append ` (n)` (e.g. `foo (1).png`). Pasted clipboard images (nameless) → `pasted-<timestamp>.png`. |
| Conflict resolution owner | Agent-side (it knows the filesystem); the final chosen name is reported back so webterm references the right path. |

## Current state (reference)

- Upload: webterm `uploadFiles` (`webterm/src/lib/api.ts`) POSTs multipart to
  `/api/files/upload?agent&workspace&path`. Hub `files_upload`
  (`crates/hub/src/app/api.rs`) streams chunks to the agent as
  `FsWriteInit` + `FsWriteChunk` frames and waits for `FsWriteResult`. Agent
  `crates/agent/src/fs.rs` `write_init` (creates file; `resolve_safe_parent`
  already does `create_dir_all` on the parent — so `.cloudcode/uploads/` is
  auto-created) and `write_chunk` (writes; the v1.25.1 byte-count integrity
  check lives hub-side).
- The HTTP response is `{ results: [{ name, bytes_written, error }] }`.
- Terminal input: xterm `term.onData` and the Shift+Enter handler send bytes
  straight to the per-tab PTY WebSocket via `tab.ws.sendBinary(bytes)`
  (`webterm/src/pages/Workbench.tsx`). Injecting text into claude's input ==
  sending those bytes.
- `FilesModal` already has drag-drop upload UI (to the browsed dir) and an
  upload progress/state pattern to mirror.

## Shared contract (both layers code against this)

- webterm uploads dropped/pasted files via the existing endpoint with
  `path=.cloudcode/uploads`.
- The HTTP response keeps shape `{ results: [{ name, bytes_written, error }] }`,
  but **`name` is the FINAL name the agent wrote** (after any conflict suffix).
  webterm inserts `@.cloudcode/uploads/<name>` using exactly `results[].name`.
- A successful upload has `error == null` and `bytes_written == file size`
  (existing integrity check).

## Backend (Rust) — conflict-safe naming + report final name

- `crates/agent/src/fs.rs` `write_init`: if the resolved target already exists,
  pick the first free name by appending ` (n)` before the extension
  (`foo.png` → `foo (1).png` → `foo (2).png` …). Create that file instead.
  Store the final filename on the `WriteSession`.
- Protocol: add `final_name: Option<String>` (or `String`, `#[serde(default)]`)
  to `ClientMsg::FsWriteResult` on both the agent (`crates/agent/src/tunnel.rs`)
  and hub (`crates/hub/src/tunnel.rs`) definitions; the agent fills it with the
  final filename on the eof result.
- `crates/hub/src/app/api.rs` `files_upload`: put the agent-reported final name
  into `results[].name` (fall back to the requested filename if absent, for
  back-compat). Keep the existing integrity check.
- Tests: agent unit test for the conflict-suffix naming (existing file →
  ` (1)` etc.); keep existing upload tests green.

## Frontend (webterm) — drop, paste, upload-and-insert

- New helper `uploadAndInsertFiles(tab, files: File[])`:
  1. Upload all `files` via `uploadFiles(agent, workspace, '.cloudcode/uploads', items, onProgress)`.
  2. For each successful result, build `@.cloudcode/uploads/<results[].name>`.
  3. Join the references with spaces, append a trailing space, and send to the
     PTY: `tab.ws.sendBinary(new TextEncoder().encode(refs))` (same channel as
     typed input). Only insert references for files that uploaded cleanly.
  4. Surface a lightweight progress indicator while uploading and a toast/error
     on failure (mirror FilesModal's upload state; do NOT block typing).
- **Drag-and-drop**: on the terminal pane container (the per-tab xterm host
  div in `Workbench.tsx`), add `dragover` (preventDefault + show a drop
  highlight) and `drop` (preventDefault, read `e.dataTransfer.files`, call
  `uploadAndInsertFiles`). Ignore drops with no files.
- **Paste image**: a `paste` handler (Ctrl/Cmd+V) that inspects
  `e.clipboardData.items` for image types; for each image item, get the
  `File`/Blob, synthesise a name `pasted-<Date.now()>.<ext>` (ext from MIME),
  preventDefault, and call `uploadAndInsertFiles`. Non-image pastes fall
  through to xterm's normal paste (unchanged).
- Multiple files in one drop → all uploaded → references inserted space-joined.
- The drop/paste handlers attach when the tab's terminal is opened (next to the
  existing `term.open(el)` wiring) and target the active tab.

## Error handling

- Upload failure (network, integrity mismatch, agent error) → toast with the
  error; insert nothing for that file. Other files in the same batch still
  proceed.
- No active/connected session → ignore the drop (or toast "open a session
  first"); never send to a closed PTY.

## Out of scope (Phase 1 / YAGNI)

- CLI drag (Phase 2).
- CLI/remote clipboard image paste (infeasible: remote claude can't reach the
  local clipboard).
- A conflict dialog (FilesModal's Skip/Overwrite/Keep-Both) — drag-to-terminal
  always auto-keeps-both via the ` (n)` suffix.
- Configurable upload destination — fixed at `.cloudcode/uploads/`.
- Progress for tiny files beyond a basic indicator.

## Verification

- `cargo test` (agent conflict-naming test + existing upload tests).
- `cd webterm && npx tsc -b && npx vite build`.
- Manual (Pete, on `dev`, after rebuilding the hub which embeds webterm):
  drag a text file → it appears under `.cloudcode/uploads/`, `@…` inserted,
  claude can read it; drag two same-named files → second becomes ` (1)`;
  paste a screenshot → `pasted-<ts>.png` uploaded + referenced, claude sees the
  image; drop with no session → no crash.

## Release

Per project flow: after Pete validates on `dev`, merge to `main`, bump
`Cargo.toml` (MINOR — new feature), commit, tag `vX.Y.0`, push tag. Webterm is
embedded in the hub binary, so the hub must be rebuilt+redeployed for users to
get it.

## Appendix — Phase 2 (CLI drag) outline (not implemented here)

- Detect bracketed paste (`ESC[200~ … ESC[201~`) in the CLI stdin reader
  (`crates/client/src/input.rs` / `relay.rs`); if the pasted content is a single
  existing local file path (`std::fs::metadata`), intercept it.
- Read the local file; upload to the agent workspace. The CLI speaks the WS
  wire protocol, not the hub's HTTP API, so this needs either (a) new
  client→hub upload frames, or (b) the client calling the hub's
  `/api/files/upload` with its own auth. To be decided in the Phase 2 spec.
- Replace the path in the forwarded stream with `@.cloudcode/uploads/<name>`.
- Risks: heuristic path detection, quoting/escaping, the new upload channel.
