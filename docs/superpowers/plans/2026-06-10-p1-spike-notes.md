# P1 Spike Notes — playwright-mcp `--cdp-endpoint` vs externally-managed Chrome

Date: 2026-06-10 · Branch: `feature/desktop-app` · Task 1 of the Desktop App P1 plan (risk-validation spike, empirical, no product code).

## Setup

- **Chrome binary**: system Chrome at `/Applications/Google Chrome.app/Contents/MacOS/Google Chrome` (Chrome **149.0.7827.54**). Playwright-cached Chromium (`~/Library/Caches/ms-playwright/chromium-1223`) also exists as a backup but was not needed.
- **playwright-mcp**: `@playwright/mcp@0.0.76` via npx (verified: `npx -y @playwright/mcp@0.0.76 --version` → `Version 0.0.76`).
- Node v26.0.0, macOS (darwin 25.4.0).

### 1. Launch Chrome resident (we own this process)

```bash
"/Applications/Google Chrome.app/Contents/MacOS/Google Chrome" \
  --headless=new --remote-debugging-port=19222 \
  --user-data-dir=/tmp/cc-p1-spike \
  --no-first-run --no-default-browser-check about:blank &
```

CDP answered within ~1s:

```json
// curl -s http://127.0.0.1:19222/json/version
{
   "Browser": "Chrome/149.0.7827.54",
   "Protocol-Version": "1.3",
   "webSocketDebuggerUrl": "ws://127.0.0.1:19222/devtools/browser/48acf5fc-…"
}
```

### 2. Drive playwright-mcp over stdio

Helper script (`/tmp/cc-p1-spike-driver.mjs`, Node) spawns:

```bash
npx -y @playwright/mcp@0.0.76 --cdp-endpoint http://127.0.0.1:19222
```

…with piped stdio and speaks newline-delimited JSON-RPC:
`initialize` (protocolVersion `2025-06-18`) → `notifications/initialized` → `tools/call browser_navigate {url:"https://example.com"}` → `tools/call browser_snapshot {}`.

## Evidence (trimmed)

**initialize** succeeded:

```json
{"protocolVersion":"2025-06-18","capabilities":{"tools":{}},
 "serverInfo":{"name":"Playwright","version":"1.61.0-alpha-1781023400000"}}
```

**browser_navigate** + **browser_snapshot** through the external Chrome:

```
### Page
- Page URL: https://example.com/
- Page Title: Example Domain
### Snapshot
- heading "Example Domain" [level=1] [ref=e3]
- paragraph [ref=e4]: This domain is for use in documentation examples …
```

**Proof it drove OUR Chrome** (not a self-launched one) — our debug port's target list gained the page:

```bash
$ curl -s http://127.0.0.1:19222/json   # → type/url/title
page https://example.com/ Example Domain
```

**Restart test**: killed the playwright-mcp subprocess (SIGTERM; Chrome left running), spawned a fresh `npx -y @playwright/mcp@0.0.76 --cdp-endpoint http://127.0.0.1:19222`, replayed `initialize` + `initialized`, then called `browser_snapshot` **without navigating**. It reattached and returned the SAME page:

```
### Page
- Page URL: https://example.com/
- Page Title: Example Domain
### Snapshot
- heading "Example Domain" [level=1] [ref=e3]   ← same content, fresh refs
```

## Verdicts

| Q | Question | Verdict |
|---|----------|---------|
| Q1 | `--cdp-endpoint` attach to externally-launched Chrome | **WORKS** — attached to our `--remote-debugging-port=19222` Chrome 149, no extra flags |
| Q2 | navigate + snapshot through it | **WORKS** — real network navigation to example.com; accessibility snapshot returned; target visible in our `/json` list |
| Q3 | playwright-mcp restart → reattach same Chrome, same page state | **WORKS** — fresh process + initialize + `browser_snapshot` (no navigate) sees the same example.com page; page state survives MCP restarts |
| Q4 | Surprises | See below |

### Q4 — surprises / notes

1. **`serverInfo.version` is the Playwright lib version**, not the package version: `0.0.76` reports `1.61.0-alpha-1781023400000`. Don't use serverInfo to pin the mcp package version.
2. **Snapshot side-effect on disk**: `browser_navigate` writes a snapshot YAML to `.playwright-mcp/page-<timestamp>.yml` relative to the MCP server's cwd. In the product, set the server's cwd (or `--output-dir`) deliberately, or this litters the repo (it littered ours during the spike; removed).
3. **No version-mismatch warnings** between Playwright 1.61-alpha and Chrome 149 over CDP; `--headless=new` behaved normally (no quirks observed; targets, navigation, a11y snapshot all fine).
4. **Fallback would be painful** (not needed since Q1 works, but observed empirically): when playwright-mcp self-manages its browser, it launches Chrome with `--remote-debugging-pipe` — i.e. **no TCP CDP port exists to discover** via `ps`. Several unrelated self-managed instances on this machine all show `--remote-debugging-pipe` + a private `ms-playwright-mcp/mcp-chrome-*` profile. So external-Chrome-with-`--cdp-endpoint` is not just nicer, it's the only way to share the browser with other CDP clients.
5. **Lazy attach (verified)**: playwright-mcp does **not** connect to the CDP endpoint at startup. With Chrome already killed, `initialize` still succeeded normally; the failure only surfaced on the first tool call, as a tool-level error (`isError: true`), not a protocol/process failure:
   ```
   Error: async initializeServer: connect ECONNREFUSED 127.0.0.1:19222
     - <ws preparing> retrieving websocket url from http://127.0.0.1:19222
   ```
   So a bad/dead endpoint won't fail MCP startup — the product must health-check the CDP port itself.
6. **Chrome-crash recovery without mcp restart (verified)**: with ONE long-lived mcp process — navigate OK → `pkill` Chrome → `browser_snapshot` returns tool error `browserBackend.callTool: Target page, context or browser has been closed` (process stays alive) → relaunch Chrome on the same port → next `browser_navigate` on the SAME mcp process succeeds and snapshot works:
   ```
   [snapshot while chrome dead] ### Error … Target page, context or browser has been closed
   [navigate #2 same mcp process] … Page URL: data:text/html,<h1>spike-after-recover</h1>
   [snapshot #2] … heading "spike-after-recover" [level=1] [ref=e2]
   ```
   Recovery from a Chrome crash needs only a Chrome relaunch on the same port — neither side ever needs to restart the other.

## Cleanup performed

Killed spike Chrome, `rm -rf /tmp/cc-p1-spike`, removed stray `.playwright-mcp/` from the repo.

## Implication for Task 3/4

The "we own the Chrome process, playwright-mcp is a disposable stateless attachment" architecture is validated end-to-end: Chrome lifecycle (and page/session state) is fully decoupled from MCP server lifecycle, in BOTH directions — mcp restart preserves page state (Q3), and Chrome restart is survived by a live mcp (Q4.6).
