# M1 Local-Browser Pipe — Manual E2E Smoke

Validates the reverse browser-RPC channel end to end with real hub + agent + client
processes and a real `claude`, using the echo MCP stub on the client side.

> **✅ VALIDATED 2026-06-10** by petez on PeteMacBookPro (all three processes local,
> branch `feature/local-browser-m1` @ c95be51): claude connected to `cc-browser`,
> `tools/call echo text="hello"` returned `echo: hello` through the full chain.
>
> **Gotchas hit during validation (read before re-running):**
> 1. **Fresh session required after agent restart.** Tokens live in agent memory;
>    a tmux-reattached claude keeps its original (now-dead) token. Reset the
>    workspace / open a new one after restarting the agent.
> 2. **Launch the client with the stub command pinned** if in doubt:
>    `export CC_BROWSER_MCP="node /abs/path/to/test-fixtures/echo-mcp.mjs"` before
>    starting `cloudcode`. (Default resolution walks up from the binary, but the
>    env var removes all ambiguity.)
> 3. **Must use the Rust CLI client** — webterm has no browser channel in M1;
>    sessions opened via the browser SPA will time out on MCP calls.
> 4. claude treats any non-2xx on the MCP POST as "auth required" and goes into a
>    misleading OAuth-discovery failure — that's why the endpoint returns transport
>    errors as JSON-RPC errors at HTTP 200 (fixed in 535113b/c95be51).

## Prereqs
- `node` installed on the CLIENT machine (the laptop running `cloudcode`). The client
  advertises `browser_capable` by probing for `node` on PATH (`cc_browser::which_node`),
  so node presence is what flips capability on.
- Built binaries: `cargo build --release` (hub, agent, client).
- The echo stub at `test-fixtures/echo-mcp.mjs` present in the client's working dir
  (the M1 default command is `node test-fixtures/echo-mcp.mjs`, resolved relative to the
  client's CWD), OR set `CC_BROWSER_MCP="node /abs/path/to/test-fixtures/echo-mcp.mjs"` in
  the client env to override the whole command.

## Steps
1. Start the hub (note its address/token). Start the agent. The resident MCP endpoint is
   spawned on `127.0.0.1:7110` by default (`config.mcp_port`, falls back to 7110). NOTE:
   in M1 there is no explicit "listening" log line — the serve task only logs
   `mcp endpoint exited` (at `error` level) if it fails to bind/serve. To confirm it is up,
   curl `http://127.0.0.1:7110/healthz` and expect the body `ok`. Then start the client
   (with node on PATH so it announces `browser_capable=true`).
2. From the client, open a workspace and enter claude.
3. In claude, ask it to list available MCP tools / MCP servers. Expect a `cc-browser`
   server exposing an `echo` tool.
4. Ask claude to call the `echo` tool with text "hello". Expect the result `echo: hello`.
5. Negotiation check: on a client machine WITHOUT node (or with node removed from PATH),
   repeat steps 2-3. Expect NO `cc-browser` server / no `echo` tool (browser_capable=false
   ⇒ agent skips MCP injection, see `pty.rs` `if browser_capable { … inject --mcp-config }`).

## What this proves
- claude's MCP HTTP client connects to the agent's resident endpoint
  (`http://127.0.0.1:7110/mcp/<token>`, Streamable-HTTP POST-blocking).
- A `tools/list` and `tools/call` round-trip survives: agent endpoint → hub → client →
  echo subprocess → back, correlated by JSON-RPC id.
- Capability negotiation gates tool exposure (node ⇒ `Hello.browser_capable` ⇒
  `PtyOpen.browser_capable` ⇒ `--mcp-config` injection).

## Known M1 limitations (validated as out-of-scope, not bugs)
- Browser tools are injected only on first claude boot for a session; after a tmux
  reattach (CLOUDCODE_RESUME_CMD path) the `--mcp-config` is not re-applied, so a resumed
  session won't have cc-browser until a fresh session. (Revisit in M2.)
  **FIXED in M2:** the wrapper actually does pass `--mcp-config` on resume too (`$@` is
  forwarded to `eval "$RESUME_CMD"`), so idle-reattach works via re-reading the rewritten
  `mcp-browser.json` (new token). Busy-reattach (claude still running in tmux) is fixed by
  token REBIND on the open_session swap path: the prior handle's tokens are rebound to the
  new session_id instead of unregistered, so the live claude keeps working; both tokens are
  carried on the new handle (`PtyHandle.mcp_tokens`) and all unregistered on true close.
- No auth gate / handoff / real browser yet — those are M2/M3. The client-side MCP
  subprocess is the echo stub (`test-fixtures/echo-mcp.mjs`), not a real browser driver.
- Streamable-HTTP header nuances (Mcp-Session-Id, Accept negotiation) are not implemented;
  the endpoint is a dumb relay (routes by token→session_id, correlates by JSON-RPC id,
  tunnels opaque JSON). If real claude requires those headers, adjust the endpoint here.
- The endpoint rejects `GET /mcp/:token` with 405 (no server-initiated SSE in M1); only
  POST-blocking is supported.

## Automated coverage (for reference)
The real-HTTP path (axum routing over a live TCP socket) is covered by
`mcp_endpoint::tests::real_http_post_roundtrips_via_endpoint` in
`crates/agent/src/mcp_endpoint.rs`, which starts the actual `serve()` on a free localhost
port, drives it with a reqwest POST, and simulates the hub/client via the `to_hub` channel
+ `resolve_response`. This manual smoke complements it by exercising the real-claude ↔
endpoint interop that automated tests can't reach.
