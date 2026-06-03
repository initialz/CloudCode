# Drag files into the terminal — Design (Phase 2: CLI)

Date: 2026-06-04
Branch: `dev`
Status: Approved, ready for implementation
Follows: `2026-06-04-terminal-file-drop-design.md` (Phase 1, webterm — shipped v1.26.0)

## Goal

Let a user of the `cloudcode` CLI client drag a local file onto their terminal
and have the remote `claude` be able to read it — the CLI analogue of Phase 1.
Target topology (per brainstorming): **remote agent** — the CLI runs on the
user's machine, the agent (with the workspace + claude) runs elsewhere. So a
dragged local path doesn't exist on the agent and **must be uploaded** to the
workspace, then referenced by its agent-side path.

## Decisions (locked during brainstorming)

| Decision | Choice |
|----------|--------|
| Upload channel | **Approach A — new client↔hub upload frames.** The CLI sends new `ClientToHub` frames; the hub relays them to the agent's existing `FsWriteInit`/`FsWriteChunk` write path (reusing conflict-safe naming + the v1.25.1 byte-count integrity check). No HTTP, no new auth — the WS session is already authenticated. |
| Upload destination | `.cloudcode/uploads/` (same as Phase 1). |
| Reference inserted | `@.cloudcode/uploads/<final-name>` (same as Phase 1; `final-name` is what the agent actually wrote after any ` (n)` conflict suffix). |
| Multiple files | Supported: if a single bracketed paste parses to one-or-more existing local files, upload all and inject space-joined `@`-mentions. |
| Feedback during upload | Silent upload; on success inject the `@`-mention(s); on per-file failure inject an inline ` [upload failed: <name>] ` note. The CLI can't safely draw a progress overlay over claude's TUI — documented rough edge. |

## Architecture & data flow

```
drag file → terminal inserts a bracketed-paste local path into the CLI's stdin
  → CLI relay detects bracketed paste, parses path(s), checks they exist locally
    → reads file bytes, uploads via new ClientToHub::FsWrite* frames
      → hub forwards to agent FsWriteInit/FsWriteChunk (conflict-safe, integrity-checked)
        → agent writes to <workspace>/.cloudcode/uploads/<final>, returns final name
      → hub returns HubToClient::FsWriteResult{final_name}
    → CLI injects "@.cloudcode/uploads/<final> " as normal input bytes to claude
  (the original local path is NOT forwarded)
```

### Client (`crates/client/`)

- **Bracketed-paste detection** (new, testable module, e.g. `paste_detect.rs`):
  the relay's stdin stream may wrap pasted/dragged text in bracketed paste
  (`ESC[200~ … ESC[201~`) — claude enables bracketed-paste mode
  (`ESC[?2004h`), which reaches the user's terminal through the relay output,
  so the terminal wraps drags. A small state machine buffers bytes between
  `ESC[200~` and `ESC[201~` (with a sane size cap; over the cap → treat as a
  normal paste and forward verbatim). Pure function over byte chunks — unit
  tested.
- **Path parsing** (pure, tested): split the bracketed-paste content into
  tokens, handling the two common terminal drag encodings — backslash-escaped
  spaces (`/a/b\ c.png`) and surrounding single/double quotes (`'/a/b c.png'`).
  Trim whitespace/newlines.
- **Decision rule**: if EVERY token is an existing local file
  (`std::fs::metadata(t).is_file()`), treat the paste as a file drop and
  intercept it (do NOT forward). Otherwise forward the original bytes verbatim
  (it was a normal paste).
- **Upload + inject** (in/near `relay.rs`): for each detected file, read it and
  upload via the new frames (64 KiB base64 chunks, matching the HTTP path).
  After all uploads finish, send `@.cloudcode/uploads/<final> …` (space-joined,
  trailing space) to the agent as **raw input bytes** (not bracketed) via the
  relay's existing `OutFrame::Binary` out channel — same channel typed input
  uses. The relay loop must not block its output arm while uploading: run the
  upload as a task and feed the resulting inject-bytes / error back into the
  select loop via a channel.
- The client already knows `agent`, `workspace`, and is authenticated — pass
  these in the upload frames.

### Protocol (both copies must match — wire-compatible serde)

Add to BOTH `crates/client/src/proto.rs` and `crates/hub/src/pty_proto.rs`:
- `ClientToHub::FsWriteInit { request_id: Uuid, agent: String, workspace: String, path: String }`
- `ClientToHub::FsWriteChunk { request_id: Uuid, #[serde(default)] data_b64: String, #[serde(default)] eof: bool }`
- `HubToClient::FsWriteResult { request_id: Uuid, #[serde(default)] final_name: Option<String>, #[serde(default)] error: Option<String> }`

(`path` is the destination dir, e.g. `.cloudcode/uploads`, + filename — keep it
consistent with how the HTTP path builds `target_path`.)

### Hub (`crates/hub/`)

- Handle the new `ClientToHub::FsWrite*` frames in `pty_session.rs` (the same
  match that handles `OpenSession` etc.). Authorize via the existing
  `authorize_workspace(account, agent, workspace)`; resolve the agent
  connection from the registry.
- **Refactor the upload orchestration** out of `app/api.rs` `files_upload` (the
  loop body that does `FsWriteInit` → stream `FsWriteChunk` → await
  `FsWriteResult`, with the byte-count integrity check and final-name) into a
  reusable async helper, and call it from BOTH the HTTP handler and the new WS
  frame handler. This keeps conflict-naming + integrity in one place.
- Return `HubToClient::FsWriteResult { final_name, error }` to the client.

## Error handling & edge cases

- Content isn't all existing files → forward verbatim (normal paste; never
  hijacked).
- Paste exceeds the buffer cap → forward verbatim.
- ws/agent error or integrity mismatch → inject ` [upload failed: <name>] `
  inline so the user sees it; other files in the batch still proceed.
- Binary files → base64-chunked, fine.
- Size cap: reuse the agent's 1 GiB write cap.

## Out of scope (Phase 2 / YAGNI)

- Clipboard image paste in the CLI (infeasible: remote claude can't reach the
  local clipboard).
- A progress UI / overlay in the CLI (can't safely draw over claude's TUI).
- Same-machine optimization (skip path rewrite when agent shares the
  filesystem) — we always upload, per the chosen remote-agent scope.

## Testing

- Client unit tests (pure, hermetic): the bracketed-paste state machine
  (single chunk, split across chunks, oversized, no-paste passthrough) and the
  path parser (plain, backslash-escaped spaces, quoted, multiple files,
  non-file token → reject).
- Reuse existing agent write tests (conflict-naming/integrity already covered).
- Manual (Pete): with a **remote** agent, drag a file into the CLI → it lands
  in `.cloudcode/uploads/`, `@…` is injected, claude reads it; drag a
  non-file selection / normal paste → forwarded unchanged; drag two files →
  both uploaded + referenced.

## Release

Per project flow: after Pete validates on `dev`, merge to `main`, bump
`Cargo.toml` (MINOR — new feature), tag, push. (Affects the `cloudcode` client
binary and the hub — both must be rebuilt/redeployed.)

## Implementation note (why one subagent, not parallel)

This is tightly-coupled Rust spanning client + hub + a shared wire protocol,
all in one cargo workspace (shared `target/`). Parallel agents would contend on
the build and risk drifting the two protocol copies. One agent owns the whole
vertical slice so the contract stays consistent end-to-end.
