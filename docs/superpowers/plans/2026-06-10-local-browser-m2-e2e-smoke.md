# M2 Local-Browser Auth Gate — Manual E2E Smoke

Validates the M2 auth gate end to end with real hub + agent + client processes and a real
`claude`, using the pinned `@playwright/mcp@0.0.76` headless driver on the client side.

> **Gotchas to read before running:**
> 1. **Fresh session required after agent restart.** Tokens live in agent memory; a
>    workspace token is stable across client reattach, but an agent restart clears the
>    in-memory token map. Open a new workspace session after restarting the agent.
> 2. **First browser action may take up to ~2 minutes.** Tool calls get a 120s
>    endpoint timeout (vs 25s for handshake/metadata), covering the lazy browser launch
>    plus the time the user takes to answer the consent prompt. If the user is slower
>    than that, claude sees a timeout error on the very first call — the pill stays up,
>    and a retry succeeds once approved. Pre-warming npx (see Prereqs) still helps make
>    the first call snappy.
> 3. **Must use the Rust CLI client** — webterm sessions have no browser channel in M2;
>    browser tool calls from a webterm session will time out.
> 4. **`CC_BROWSER_MCP` env overrides the whole command.** Set
>    `export CC_BROWSER_MCP="node /abs/path/to/echo-mcp.mjs"` to swap in the echo stub
>    for pipe-debugging without a real browser. Unset it to use the playwright default.

## Prereqs

- `node` (and `npx`) installed on the CLIENT machine. The client probes for `node` on
  PATH (`cc_browser::which_node`); node presence flips `browser_capable` on in the
  handshake.
- **Pre-warm the playwright MCP package once manually** on the client machine before the
  first session:
  ```
  npx -y @playwright/mcp@0.0.76 --version
  ```
  This downloads and caches the npm package. The command itself may fail with
  "unknown option" — that is fine; the package will be cached regardless.
- **Browser binary:** on a machine with Chrome already installed, nothing extra is needed
  (the playwright default channel is `chrome`). On a machine without Chrome, install the
  playwright-bundled Chromium:
  ```
  npx -y @playwright/mcp@0.0.76 install-browser
  ```
- Built binaries from this branch (`feature/local-browser-m2`):
  `cargo build --release` (hub, agent, client).
- All three processes (hub, agent, client) must be from this branch.

## Steps

### Happy path — first call prompts, second call is silent

1. Start the hub (note its address/token). Start the agent. The resident MCP endpoint
   listens on `127.0.0.1:7110` by default. Confirm it is up:
   ```
   curl http://127.0.0.1:7110/healthz
   ```
   Expected body: `ok`.
2. Start the client (with `node` on PATH so it announces `browser_capable=true`). No
   extra env vars are needed — playwright is the M2 default command.
3. Open a **fresh** workspace and enter claude. (Reattaching to an existing workspace
   after an agent restart is fine if the agent has not restarted.)
   **No consent pill appears at claude boot** — the MCP handshake (initialize,
   tools/list, notifications) flows without consent. The playwright subprocess is
   lazily spawned by the handshake, warming up npx before the first action.
4. In claude, ask it to navigate to `https://example.com` using the browser tools.
   For example: _"Please use the browser tools to navigate to https://example.com and
   give me the page title."_
5. **The consent pill appears now — on the FIRST BROWSER ACTION (tools/call), not at
   claude boot.** The client terminal rings (BEL) and displays a yellow consent line
   on row 2:
   ```
   云端任务请求操作你的浏览器 — 允许? [y]允许 / [n]拒绝
   ```
   Claude is suspended at this point (the relay's select! is parked on the modal).
6. Press **`y`** to approve. The consent line clears. Claude receives the playwright
   page snapshot and reports the page title.
   The first action may take up to ~2 minutes (browser launch + consent); if you are
   slower than the 120s tool-call timeout, claude reports a timeout error on this
   first call — the pill stays up, and asking claude to retry succeeds.
7. Ask claude for a second browser action on the same session — for example: _"Take a
   snapshot of the current page."_
   **No prompt should appear.** The grant is live and the idle window (600 seconds
   default, or `CC_BROWSER_IDLE_TIMEOUT_SECS`) has not elapsed. Claude gets the
   response directly.

### Deny path

8. Start a **fresh** workspace session (or wait for the idle window to expire), then
   trigger a browser action. The consent pill appears again (grant was never issued or
   has expired).
9. Press **`n`** (or Esc). The pill clears. Claude receives a JSON-RPC error response:
   ```json
   {"error": {"code": -32002, "message": "denied by user"}}
   ```
   Claude reports that the browser action was denied.

### Reattach survival check

10. While the workspace is open and the browser grant is live, **detach the client**
    (close the terminal or disconnect). Reopen the same workspace with the client.
11. Ask claude for a browser action. **No agent restart occurred**, so the workspace
    token is still valid. The browser action succeeds without requiring claude to be
    restarted. (No consent prompt if the idle window has not expired; otherwise the
    consent pill appears and a fresh `y` issues a new grant.)

## Where outputs land

Screenshots and saved files go to the **CLIENT machine**, under
`<data_local>/cloudcode/browser-output` (e.g. `~/Library/Application Support/cloudcode/browser-output`
on macOS, `~/.local/share/cloudcode/browser-output` on Linux) — the browser runs
client-side, so its outputs are client-side files. They are NOT visible in the agent
workspace; auto-upload into the agent workspace is a planned M3 feature. playwright
creates the directory on first write.

## What this proves

- **Auth gate (ask-once, method-aware):** the first browser ACTION (tools/call)
  triggers the inline ANSI consent modal on the client; handshake/metadata frames
  (initialize, tools/list, ping, notifications) flow without consent, and subsequent
  actions within the idle window pass silently.
- **Idle window and expiry:** `CC_BROWSER_IDLE_TIMEOUT_SECS` (default 600s) controls
  the sliding grant window; after expiry the next frame asks again.
- **Deny propagation:** pressing `n` sends `ClientToHub::BrowserClosed` → hub forwards
  `ServerMsg::BrowserClosed` → agent calls `fail_pending(session_id, "denied by user")`
  → claude receives a clean JSON-RPC `-32002` error instead of hanging out the
  endpoint timeout.
- **Real playwright pipe:** `@playwright/mcp@0.0.76` subprocess is launched headless via
  `npx`, communicating over stdio JSON-RPC. The `BrowserChannel` line-framing used in
  M1 for the echo stub is binary-compatible with 0.0.76's single-line compact JSON
  output.
- **Reattach survival:** the workspace token is stable per `(account, workspace)` pair
  (`PtyManager.workspace_tokens`), re-registered against each new `session_id` on every
  `open_session`. A client reattach does not invalidate the token; only an agent restart
  does.

## Known limitations (M2 scope, not bugs)

- **Headless only.** Headed mode (handoff / `request_handoff` heuristic injection) is
  M3. The playwright subprocess always runs `--headless`.
- **Webterm sessions have no browser channel.** Sessions opened via the browser SPA will
  time out on MCP calls. This is intentional for M2.
- **Agent restart invalidates tokens.** A claude process that survives an agent restart
  will find its token dead. Open a fresh session after restarting the agent.
- **Consent pill renders at row 2** and may briefly overlay claude's UI output line
  until the terminal redraws after the modal closes. This is cosmetic.
- **npx cold start.** The subprocess is now lazily spawned by claude's MCP handshake
  (initialize), which carries the short 25s endpoint timeout. On the very first
  `npx @playwright/mcp@0.0.76` invocation without a cached package, the download may
  exceed that and the handshake fails. Pre-warm once as described in Prereqs to
  avoid this. (Tool calls themselves get a 120s timeout.)
- **No explicit revocation UI.** To revoke a live grant: deny the next browser action,
  wait out the idle window, or close the session. An explicit "revoke browser access"
  command is deferred to a later milestone.
