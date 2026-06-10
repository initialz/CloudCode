# M3 Local-Browser Handoff — Manual E2E Smoke

Validates the M3 human-handoff feature end to end: claude calls `request_handoff` →
client terminal rings + confirmation pill → visible Chrome window opens → human
works → presses Enter → window closes (headless restart) → claude re-navigates with
persisted cookies. Also covers: decline path, 600s timeout, login-page hint, agent-restart
self-heal, and install.sh preflight.

> **Gotchas to read before running:**
> 1. **M2 smoke must pass first.** M3 builds on M2; if the basic consent gate or
>    headless browser channel does not work, nothing in M3 will either.
> 2. **Headed switch loses in-page state by design.** When the browser restarts headed
>    (or back to headless), any unsaved form inputs are gone. Cookies and login sessions
>    persist — that is the point of handoff. This is a documented spec deviation (restart
>    vs. always-headed); see the plan for rationale.
> 3. **Must use the Rust CLI client** — webterm sessions have no browser channel and
>    claude has no browser tools at all in a webterm session. This is permanent
>    (by capability negotiation, not a bug).
> 4. **Concurrent browser calls during a handoff time out.** While the handoff pills are
>    up, all other browser requests buffer but receive no subprocess response (the
>    subprocess was restarted). They hit the 120s `CALL_TIMEOUT` on the agent side.
>    claude should not issue concurrent browser calls during a handoff; if it does, the
>    timeouts are the expected signal.

## Prereqs

- **M2 smoke passed** on this machine + these binaries.
- `node` (and `npx`) installed on the CLIENT machine. The client probes for `node` on
  PATH (`cc_browser::which_node`); node presence flips `browser_capable` on in the
  handshake.
- **Chrome recommended.** playwright-mcp defaults to the `chrome` channel for its headed
  window. On a machine without Chrome, install the playwright-bundled Chromium first:
  ```
  npx -y @playwright/mcp@0.0.76 install-browser
  ```
  On a fresh install from this branch's `install.sh`, the script pre-warms the package
  automatically when node is found:
  ```
  curl -fsSL .../install.sh | sh -s -- client
  # → prints "pre-warming @playwright/mcp (browser channel)..." if node is present
  ```
- Built binaries from `feature/local-browser-m3`:
  `cargo build --release` (hub, agent, client).
- All three processes must be from this branch.

## Steps

### Happy path — request_handoff flow

1. Start the hub. Start the agent. Confirm the MCP endpoint is up:
   ```
   curl http://127.0.0.1:7110/healthz
   ```
   Expected body: `ok`.

2. Start the client (with `node` on PATH). No extra env vars are needed.

3. Open a **fresh** workspace and enter claude. No pill appears at boot.

4. Ask claude to navigate to a site that requires login — for example:
   _"Please use the browser tools to navigate to https://github.com/login and try to
   find the username field."_

   The first browser action (tools/call) triggers the M2 consent pill:
   ```
   云端任务请求操作你的浏览器 — 允许? [y]允许 / [n]拒绝
   ```
   Press **`y`**. Claude navigates; the page snapshot returns.

5. Now instruct claude to log in:
   _"I need to log in. Please call request_handoff with reason 'login required'."_

   Claude calls `request_handoff`. The client terminal rings (BEL) and paints a pill:
   ```
   云端请求人工接管浏览器: login required — [y]打开窗口 / [n]拒绝
   ```

6. Press **`y`**. The client:
   - kills the headless subprocess (awaits its exit so the `--user-data-dir` profile
     lock is released),
   - starts a new headed subprocess (same command minus `--headless`),
   - replays the cached MCP handshake (`initialize` + `notifications/initialized`) into
     the new subprocess, swallowing the duplicate initialize response so claude never
     sees it.

   A **visible Chrome window** opens on the client machine's screen. The pill changes to:
   ```
   浏览器已切到可见窗口,完成人工操作后按回车交还
   ```

7. Log in manually in the visible Chrome window. Once done, press **Enter** in the client
   terminal.

8. The client:
   - kills the headed subprocess (awaits exit, releases profile lock),
   - starts a new headless subprocess (original command with `--headless`),
   - replays the handshake again.

   The pill clears. Claude receives the `request_handoff` success result:
   ```
   Human finished. Browser is headless again; cookies persisted. Re-navigate to continue.
   ```

9. Claude re-navigates to the authenticated page. It should now be in the logged-in
   state — the cookie/session was persisted in `--user-data-dir` across the
   headed/headless restarts.

### Decline path

10. Trigger `request_handoff` again (or start fresh). When the pill appears:
    ```
    云端请求人工接管浏览器: <reason> — [y]打开窗口 / [n]拒绝
    ```
    Press **`n`** (or Esc). The pill clears immediately. Claude receives a JSON-RPC error:
    ```json
    {"error": {"code": -32003, "message": "user declined handoff"}}
    ```
    The M2 browser consent grant is **not** revoked — declining a handoff does not
    revoke ongoing browser access.

### Timeout path

11. Trigger `request_handoff` and press **`y`**. The headed window opens and the pill:
    ```
    浏览器已切到可见窗口,完成人工操作后按回车交还
    ```
    Do **not** press Enter. After 600 seconds the agent's `HANDOFF_TIMEOUT` fires on the
    blocking POST and claude receives:
    ```json
    {"error": {"code": -32000, "message": "browser MCP request timed out ..."}}
    ```
    The headed window remains open on the client (the client has no timer of its own;
    the 600s bound lives entirely on the agent endpoint).

### Login-page hint (notify-only heuristic)

12. Ask claude to navigate to a login page (without calling `request_handoff`):
    _"Navigate to https://example.com/login and take a snapshot."_

    When the playwright page snapshot comes back and contains a password-field indicator
    (`"type":"password"` or the escaped form `\"type\":\"password\"`), the client
    terminal rings once and a hint pill appears:
    ```
    页面似乎需要登录 — 可让 claude 调 request_handoff 交给你操作
    ```
    This is **notify-only**: the pill clears on the next outbound browser frame. Claude's
    flow is not interrupted; no automatic handoff is triggered. The hint fires at most
    once per gate grant (`hint_shown` flag) and resets when the gate re-prompts or the
    browser channel tears down.

### Agent-restart self-heal

13. While a workspace session is open and claude is alive in tmux, **restart the agent**
    (kill + re-start the agent process). The agent's in-memory token map is wiped.

14. Reopen the same workspace from the client (reattach). The agent's `open_session`
    path:
    - detects the existing `<cwd>/.cloudcode/mcp-browser.json`,
    - calls `extract_token_from_config` to pull the token out of the URL,
    - re-registers that token against the new session_id.

15. Ask claude for a browser action (without restarting claude in tmux). Browser tools
    work — the token is live again. **No claude restart required.**

### install.sh preflight

16. On a machine with node installed:
    ```
    bash install.sh client --version <tag>
    ```
    After `install_bin cloudcode`, the script prints:
    ```
    pre-warming @playwright/mcp (browser channel)...
    note: if this machine has no Chrome, run later:
      npx -y @playwright/mcp@0.0.76 install-browser
    ```
    The `npx -y @playwright/mcp@0.0.76 --version` invocation may exit non-zero (unknown
    option) — that is fine; `|| true` swallows it and the package is cached regardless.

17. On a machine **without** node:
    ```
    bash install.sh client --version <tag>
    ```
    Prints:
    ```
    note: browser channel needs Node.js on this machine (optional).
    ```
    No error; install continues.

## What this proves

- **Handoff gate (client-side tool):** `request_handoff` is injected by the client into
  every `tools/list` response claude sees (client rewrites the outbound JSON); claude's
  call is intercepted at the client and never forwarded to playwright-mcp.
- **Headed/headless switch via restart + handshake replay:** the browser subprocess is
  killed (kill + wait, releasing the `--user-data-dir` profile lock) and re-spawned
  headed/headless; the cached MCP handshake (`initialize` + `notifications/initialized`)
  is replayed into the fresh subprocess; the replayed initialize response is swallowed so
  claude never sees a duplicate.
- **Cookie/session persistence:** cookies survive the restart because both the headed and
  headless subprocesses share the same `--user-data-dir` profile directory. The in-page
  state (form inputs) does not survive — documented spec deviation.
- **600s handoff window:** `timeout_for` in `mcp_endpoint.rs` returns 600s for
  `request_handoff` tool calls (vs. 120s for other tool calls, 25s for
  handshake/metadata). The client imposes no timer of its own.
- **Decline propagation:** pressing `n` sends a client-constructed JSON-RPC error
  (`-32003`) directly back to claude. The M2 consent grant is untouched.
- **Login-page hint:** a string sniff on outbound playwright frames detects
  `"type":"password"` patterns and fires a one-shot advisory pill. This is a heuristic
  — false positives are possible; the hint is purely advisory and never interrupts
  claude's flow.
- **Agent-restart self-heal:** `open_session` in `pty.rs` reads back the persisted
  `mcp-browser.json` via `extract_token_from_config` and re-registers the existing token
  instead of minting a new one. A claude process in tmux survives the agent restart
  with its token intact.
- **install.sh preflight:** the client branch pre-warms `@playwright/mcp@0.0.76` when
  node is present, so the first in-session browser action does not pay the npx
  cold-start penalty.

## Known limitations (M3 scope, not bugs)

- **Headed/headless switch loses in-page state.** Form inputs typed in the headless
  session before the handoff are gone. Cookies and login sessions persist via
  `--user-data-dir` — that is the primary use case and it works. Spec originally called
  for always-headed + off-screen window (zero state loss), but that requires owning the
  Playwright driver directly; M2 chose playwright-mcp (black-box). Documented deviation.
- **Heuristic is notify-only.** The login-page hint does not auto-trigger handoff. The
  spec's "heuristic automatic popup" is deferred until the project has its own Playwright
  driver (no window management API in playwright-mcp).
- **Webterm sessions have no browser tools.** Sessions opened via the browser SPA never
  set `browser_capable=true` in the handshake; the agent therefore never injects the MCP
  config; claude in a webterm session has no browser tools at all. This is permanent
  by capability negotiation, not a bug.
- **Concurrent browser calls during a handoff time out.** While the handoff pills are
  active and the subprocess is being restarted, any other in-flight browser requests
  from claude get no subprocess response and eventually hit the 120s `CALL_TIMEOUT` on
  the agent side.
- **No explicit handoff revocation UI.** There is no "cancel handoff" keystroke after
  pressing `y`. Closing the terminal or letting the 600s timer fire are the only escapes
  short of restarting the client.
