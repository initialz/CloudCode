# P4 Task 1 — per-session page mapping investigation (CDP context/target correlation)

Date: 2026-06-10 · Branch: `feature/desktop-app` · Task 1 of the Desktop App P4 plan
(security item: close the P2 cross-page screencast leak). Empirical — a real
experiment on this machine plus CDP docs (context7).

## Question

P2's screencast picks "the active non-blank page" of the single shared resident
Chrome, ignoring `session_id`. With a multi-account-shared agent, account A's
viewer could see account B's page. `ViewerAttach` carries `session_id`. Can the
agent use it to screencast the page belonging to **that** session's
playwright-mcp?

The hinge: when a playwright-mcp connects to an external Chrome via
`--cdp-endpoint` and navigates, does its page target carry a **distinct
`browserContextId`** (vs another playwright-mcp on the same Chrome), so the agent
can map `session_id -> browserContextId/target_id`?

## Setup

- Chrome **149.0.7827.54**, `--headless=new --remote-debugging-port=19233
  --user-data-dir=…`.
- `@playwright/mcp@0.0.76` via `npx … --cdp-endpoint http://127.0.0.1:19233`
  (the agent's exact production argv — see `mcp_endpoint::mcp_argv`).
- Two mcp subprocesses (= two "sessions"), each driven over stdio JSON-RPC,
  navigating to distinct `data:` URLs (`MARKER_A` / `MARKER_B`).
- Target inspection via **both** `GET /json` (HTTP) and CDP
  `Target.getTargets {filter:[{}]}` over the browser websocket (the only place
  `browserContextId` is exposed).

## Evidence

### 1. DEFAULT config (the agent's shipping argv — NO `--isolated`)

Two mcp instances, clean profile, each navigates to its own marker:

```
=== Target.getTargets page targets ===
{id: 8D711C2F…, url: data:…MARKER_B, browserContextId: CF6E0D5D…}
=== distinct browserContextIds among page targets ===
[ 'CF6E0D5D4DDAD5BFA1C03713D8780E9F' ]   // ONE context
```

MARKER_A is **gone** — the second instance drove the **same single page target**
in the **same default browser context**. Even forcing a new tab per instance
(`browser_tabs action:new` then navigate):

```
[default+newtab] 921C44:TA  8207D6:about  E8F5C4:TB   // all ctx B40290 — ONE context
A snapshot sees MARKER_A: true  MARKER_B: true        // A can read B's tab
B snapshot sees MARKER_A: true  MARKER_B: true        // B can read A's tab
```

→ In default mode every playwright-mcp shares the one **default browser
context**, sees every tab, and there is **no `browserContextId` or `targetId`
difference** to key a session on. The cross-page visibility is intrinsic to the
shared-default-context model, on BOTH the CDP side and playwright's own
`browser_snapshot`.

### 2. `--isolated` config (NOT what the agent ships today)

Same two instances, each launched with `--isolated`:

```
=== after navA ===  F040E9C4 url:MARKER_A ctx:18894C19
=== after navB ===  1861626D url:MARKER_B ctx:F5DFBF42   (+ leftover blank)
mcpA snapshot has MARKER_A: true   MARKER_B: false
mcpB snapshot has MARKER_A: false  MARKER_B: true
```

→ With `--isolated` each playwright-mcp creates its **own distinct
`browserContextId`** and its `browser_snapshot` sees **only its own page**. Here
strategy A (context correlation) WOULD work.

### 3. `/json` (HTTP) does NOT expose `browserContextId`

The screencast picks its target via `GET <cdp>/json`. Inspected keys:

```
description, devtoolsFrontendUrl, id, title, type, url, webSocketDebuggerUrl
```

No `browserContextId`. The only per-target correlation field present in `/json`
is `id` (= the CDP `targetId`). To match a context id you must additionally call
`Target.getTargets` over the browser ws and join on `targetId`.

## Verdict

| Mode | Distinct `browserContextId` per session? | Distinct page target per session? | Can correlate `session_id → page`? |
|------|------|------|------|
| **default (shipping)** | **No** — one shared default context | **No** — one shared tab; both see all tabs | **No** |
| `--isolated` (not shipped) | **Yes** | Yes | Yes (strategy A) |

**For the agent's current configuration, strategy A (browserContextId
correlation) and strategy B (target-id tracking) are both IMPOSSIBLE** — all
sessions share one default context and one set of tabs, visible to each other
even through playwright's own snapshot. There is no CDP signal to distinguish
"account A's page" from "account B's page".

## Decision — strategy C, with the plumbing wired for A

Implement **strategy C** (active-page selection) honestly, but refactor the page
picker so the hint pathway exists for the day the agent opts into `--isolated`:

- `pick_page_target(json)` → **`pick_page_for_session(targets_json, target_hint:
  Option<&str>)`** (pure). With a hint it returns the `webSocketDebuggerUrl` of
  the page whose `/json` `id` equals the hint; with `None` it falls back to the
  existing active-non-blank-page logic. The hint is a **target id** (not context
  id) because that is the only correlation field `/json` carries (evidence #3);
  an isolated-mode mapping can resolve `session → targetId` by joining
  `Target.getTargets`' `browserContextId` back to `/json`'s `id`.
- `EndpointState::page_hint_for(session_id) -> Option<String>` is the hint
  source ViewerManager queries. **Today it returns `None`** (default mode has no
  per-session target), so behaviour is the documented active-page fallback. The
  map (`session_id -> target_id`) and the recording hook are in place so flipping
  to `--isolated` (a future, larger change) lights up real per-session targeting
  with no further screencast/viewer plumbing.

### Residual risk (honest, carried forward)

With the shipping default config the screencast still shows the **active page of
the shared Chrome**. In a multi-account-shared agent this means account A's
viewer can still see whatever page is active — **the P2 cross-page leak is
MITIGATED-BY-PLUMBING but NOT CLOSED in default mode.** It is genuinely closed
only once the agent runs playwright-mcp `--isolated` (distinct context per
session) and populates `page_hint_for`. This matches the P2 **solo-use
acceptance**: the resident agent is intended for a single account; multi-account
sharing is against the supported model. The refactor makes the real fix a
config+mapping change rather than a screencast rewrite.

### Why not just flip `--isolated` now?

`--isolated` drops playwright-mcp's persisted profile/session state. P1 spike
notes (Q3, Q4.6) validated and rely on the *shared, persistent* default-context
model (page state survives mcp restarts; Chrome-crash recovery). Switching the
whole browser stack to per-session isolated contexts is an architectural change
with its own login/persistence tradeoffs — out of scope for this security-plumbing
task, and called out here as the follow-up that actually closes the leak.

## Cleanup

Killed spike Chrome, `rm -rf /tmp/cc-p4-spike`.

## Implementation outcome (post-investigation)

Implemented strategy C with the A-pathway wired:

- `screencast::pick_page_for_session(targets_json, target_hint: Option<&str>)`
  (pure, table-tested): hint → the `/json` page whose `id` matches (fail-closed
  if absent, so a hinted viewer never gets another session's tab); `None` →
  legacy active-page fallback.
- `EndpointState { page_hints: session_id -> target_id }` with
  `record_page_hint` / `page_hint_for`; the hint is cleared on `end_session`.
  `page_hint_for` returns `None` today (default config records nothing).
- `ViewerManager::attach(viewer, session_id)` resolves `mcp.page_hint_for(
  session_id)` and passes it into `ScreencastSession::start`.

Integration test `per_session_page_targeting_picks_the_owning_sessions_page`
(`#[ignore]`, real Chrome + 2 real playwright-mcp under `--isolated`) **PASSED**:
A and B got distinct target ids, and `pick_page_for_session` selected
A→`P4MARKER_AAA` and B→`P4MARKER_BBB` (different pages). Confirms the targeting
math closes the leak *when the per-session isolation prerequisite holds*.

**P2 cross-page leak status: MITIGATED, not CLOSED, in the shipping default
config** — sessions still share one context there, so `page_hint_for` is empty
and the viewer falls back to the active page. The leak is genuinely closed only
under `--isolated` + populated hints (a follow-up config change). Honest
solo-use acceptance stands.
