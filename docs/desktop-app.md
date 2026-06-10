# Desktop app — feature overview

> Maintainer entry point for the CloudCode desktop app feature line. End-user
> install/config lives in the [README](../README.md#desktop-app); this page is the
> map of the whole thing — architecture, milestones, crates, and what's left.

## What it is

A native, pure-Rust (eframe/egui) single-binary desktop app — a **third client**
alongside the CLI and the webterm. One window, split screen:

- **left** — the claude terminal (PTY byte stream, same native TUI as everywhere
  else), rendered by an `alacritty_terminal` VTE state machine into a self-drawn
  egui grid with CJK + IME support;
- **right** — a **live mirror of the agent's browser**, streamed over CDP
  (`Page.startScreencast`, JPEG frames) and controllable (mouse / key / IME →
  `Input.dispatchMouseEvent` / `insertText`).

The browser is **one resident instance on the agent** (same trust domain as
claude, one persistent login). When someone is watching, the agent screencasts
frames up through the hub to the app and routes the app's input back down. When
nobody is attached, the screencast stops — zero overhead.

### Why this replaced the `feature/local-browser` (M1-M3) line

An earlier line put the browser on the **user's client machine** and tunnelled
MCP frames back through the hub. It worked but exposed structural problems:
headless⇄headed switching meant restarting the browser (losing in-page state),
login state was pinned to one client machine, and webterm users couldn't be
supported at all. The new architecture **inverts** it — browser resident on the
agent, only pixels + input cross the network — which kills the cross-network MCP
frames, the client dependency, and the per-machine login pinning, and gives one
persistent login. The `feature/local-browser` branch is **archived, not merged
and not deleted**; its engineering knowledge (MCP handshake replay, timeout
tiers, playwright-mcp argv) was the direct input to this design and partly reused.
See the [design spec](superpowers/specs/2026-06-10-desktop-app-design.md) for the
full decision record.

## Milestones

Five milestones, each independently verifiable, each with an implementation plan
and a hand-run e2e smoke manual.

| # | Milestone | Plan | Smoke | Key files |
|---|-----------|------|-------|-----------|
| **P1** | Agent browser foundation — resident Chrome (localhost CDP), playwright-mcp via `--cdp-endpoint` on the same instance, `mcp_endpoint` short-circuited to a local subprocess; `[browser] enabled` toggle | [plan](superpowers/plans/2026-06-10-desktop-app-p1-agent-browser.md) · [spike notes](superpowers/plans/2026-06-10-p1-spike-notes.md) | [smoke](superpowers/plans/2026-06-10-desktop-app-p1-e2e-smoke.md) | `crates/agent/src/browser/{chrome,subprocess,mcp_endpoint}.rs` |
| **P2** | Screencast pipe — shared `cloudcode-proto` crate (collapses the hand-mirrored client↔hub PTY wire into one source of truth), viewer/screencast frames on the agent↔hub tunnel, agent screencast module, hub frame relay | [plan](superpowers/plans/2026-06-10-desktop-app-p2-screencast.md) | [smoke](superpowers/plans/2026-06-10-desktop-app-p2-e2e-smoke.md) | `crates/proto/src/lib.rs`, `crates/agent/src/tunnel.rs`, `crates/agent/src/browser/screencast.rs`, `crates/hub/src/viewer_session.rs` |
| **P3** | App skeleton + terminal panel — `crates/app`: egui shell, connection core, workspace picker, full terminal panel (CJK / IME / selection / scroll / resize) | [plan](superpowers/plans/2026-06-10-desktop-app-p3-app-terminal.md) | [smoke](superpowers/plans/2026-06-10-desktop-app-p3-e2e-smoke.md) | `crates/app/src/{main,backend,session_view}.rs`, `crates/app/src/terminal/*` |
| **P4** | Browser panel + split — viewer panel, adjustable split / fullscreen, focus routing; per-session page-mapping security investigation | [plan](superpowers/plans/2026-06-10-desktop-app-p4-browser-panel.md) · [page-mapping notes](superpowers/plans/2026-06-10-p4-page-mapping-notes.md) | [smoke](superpowers/plans/2026-06-10-desktop-app-p4-e2e-smoke.md) | `crates/app/src/viewer/*`, `crates/agent/src/browser/viewer.rs` |
| **P5** | Packaging — release CI hardening (GUI app excluded from the musl server release), macOS `.app`/`.dmg` bundle, this documentation | [plan](superpowers/plans/2026-06-10-desktop-app-p5-packaging.md) | — | `.github/workflows/release.yml`, `crates/app/Cargo.toml` (`[package.metadata.bundle]`), `crates/app/assets/` |

## Crate map

| Crate / path | Role |
|--------------|------|
| `crates/proto` (`cloudcode-proto`) | Shared wire crate for the **client↔hub** protocol (`ClientToHub` / `HubToClient`), referenced by hub / app so the desktop app reuses the CLI's connection core instead of a hand-mirrored copy. |
| `crates/{agent,hub}/src/tunnel.rs` | The **agent↔hub** tunnel protocol where the screencast/viewer messages live: `ViewerInputEvent` (mouse / key / wheel / insert-text), the `ViewerAttach` / `ViewerDetach` / `ViewerInput` control variants, and the binary `TAG_SCREENCAST_FRAME` JPEG frame. |
| `crates/agent/src/browser/*` | Agent browser foundation + screencast. `chrome.rs` (resident Chrome supervision), `subprocess.rs` / `mcp_endpoint.rs` (playwright-mcp short-circuit + handshake replay), `screencast.rs` (CDP screencast pull + input inject), `viewer.rs` (attach/detach + per-session page hint plumbing). |
| `crates/hub/src/viewer_session.rs` | Hub-side viewer frame relay — reuses the PTY-stream session routing; viewer auth goes through the existing account/ACL. (`crates/hub/src/app/viewer.rs` is the webterm-facing ws handler / placeholder viewer page.) |
| `crates/app/*` | The desktop app itself. `terminal/` (grid render, fonts, IME input, geometry), `viewer/` (CDP frame decode → egui texture, input panel, proto), `backend.rs` / `wire.rs` (hub ws), `config.rs` (shared `config.toml`), `session_view.rs` / `state.rs` (egui shell + split layout). |

## Self-update status

The agent and client both self-update from GitHub releases
(`crates/agent/src/update.rs`, `crates/hub/src/update.rs` — match a release asset
by name, download, swap in place). **The desktop app does not self-update yet.**
V1 is a manual flow: download a newer `CloudCode-<tag>-macos-aarch64.dmg` and
replace the app. Wiring the app onto the same GitHub-release check is listed as
future work below (deliberately not implemented — YAGNI for V1).

## Known limitations / future work

- **Per-session browser page isolation** — mitigated, not closed, in the shipping
  default config: all playwright-mcp sessions share one browser context, so a
  multi-account-shared agent could leak account A's active page to account B's
  viewer. The plumbing (a `session_id → target_id` hint path) is wired but returns
  nothing today; running playwright-mcp `--isolated` + populating the hint closes
  it. Fine under the documented solo-use model. Full analysis:
  [`p4-page-mapping-notes.md`](superpowers/plans/2026-06-10-p4-page-mapping-notes.md).
- **No code signing / notarization** — no Apple Developer cert, so the `.dmg` is
  unsigned and the first launch needs a Gatekeeper bypass (right-click → Open, or
  `xattr -dr com.apple.quarantine`). Sign + notarize once a cert is available.
- **Viewer is fixed-viewport** — screencast runs at a fixed size; live viewport
  reflow on app-window resize (`ViewerResize` is wired in the protocol but the
  agent doesn't yet re-drive the browser viewport) is future work.
- **macOS-only packaging** — only the Apple Silicon `.dmg` is produced by CI.
  Linux desktop-app packaging (AppImage / tarball with the eframe/winit runtime
  deps documented) is not shipped; Linux users `cargo run -p cloudcode-app`.
- **No app self-update** — see above; future work is the GitHub-release check the
  agent/client already use.
- **No webterm viewer panel** — the screencast protocol is display-端 agnostic and
  P2's web verification page is its seed, but a real in-webterm browser panel is
  not built yet.

## Related docs

- [Design spec](superpowers/specs/2026-06-10-desktop-app-design.md) — the full
  decision record and architecture.
- Plans: [P1](superpowers/plans/2026-06-10-desktop-app-p1-agent-browser.md) ·
  [P2](superpowers/plans/2026-06-10-desktop-app-p2-screencast.md) ·
  [P3](superpowers/plans/2026-06-10-desktop-app-p3-app-terminal.md) ·
  [P4](superpowers/plans/2026-06-10-desktop-app-p4-browser-panel.md) ·
  [P5](superpowers/plans/2026-06-10-desktop-app-p5-packaging.md)
- Smoke manuals: [P1](superpowers/plans/2026-06-10-desktop-app-p1-e2e-smoke.md) ·
  [P2](superpowers/plans/2026-06-10-desktop-app-p2-e2e-smoke.md) ·
  [P3](superpowers/plans/2026-06-10-desktop-app-p3-e2e-smoke.md) ·
  [P4](superpowers/plans/2026-06-10-desktop-app-p4-e2e-smoke.md)
- Investigation notes:
  [P1 spike](superpowers/plans/2026-06-10-p1-spike-notes.md) ·
  [P4 page-mapping](superpowers/plans/2026-06-10-p4-page-mapping-notes.md)
</content>
</invoke>
