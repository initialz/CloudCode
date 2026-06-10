//! Session-screen view mode + the pure layout/lifecycle logic around it.
//!
//! The Session screen is a split layout: the terminal on the left and the
//! browser (screencast) panel on the right, with a draggable separator. The
//! user can collapse to a single panel. [`SessionView`] is that tri-state,
//! and the rest of this module is the PURE decision logic the `main.rs`
//! render arm leans on — kept here, side-effect-free, so it's unit-testable
//! without a GUI (egui rendering is smoke-only):
//!
//!   * [`SessionView::cycle`] — the toolbar/keyboard toggle order.
//!   * [`should_connect_viewer`] — whether the browser panel is visible in a
//!     given view, which drives the *lazy* viewer-ws connect/disconnect
//!     (connect when first shown, detach when hidden — honors P2 on-demand
//!     screencast).
//!   * [`clamp_split_ratio`] / [`split_width_range`] — keep the split
//!     separator within sane bounds so neither panel can be dragged away.

/// Which panel(s) the Session screen is showing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SessionView {
    /// Terminal left, browser right, draggable separator. The default —
    /// the spec's "left claude works, right watch the browser" layout.
    #[default]
    Split,
    /// Full-window terminal only (browser hidden → viewer ws detached).
    TerminalOnly,
    /// Full-window browser only.
    BrowserOnly,
}

impl SessionView {
    /// Whether the browser panel is visible in this view. Drives the lazy
    /// viewer-ws lifecycle (see [`should_connect_viewer`]).
    pub fn shows_browser(self) -> bool {
        matches!(self, SessionView::Split | SessionView::BrowserOnly)
    }

    /// Cycle to the next view, in toolbar order: Terminal → Split → Browser
    /// → (wrap) Terminal. Bound to Cmd/Ctrl+B so repeated presses walk the
    /// three states predictably.
    pub fn cycle(self) -> SessionView {
        match self {
            SessionView::TerminalOnly => SessionView::Split,
            SessionView::Split => SessionView::BrowserOnly,
            SessionView::BrowserOnly => SessionView::TerminalOnly,
        }
    }
}

/// Whether the viewer ws should be *connected* for a given view — i.e. the
/// browser panel is visible. The App connects the [`crate::viewer::
/// ViewerHandle`] lazily when this first becomes `true` and drops it (which
/// makes the hub send `ViewerDetach`, stopping the agent's screencast) when
/// it goes back to `false`. PURE — the App diffs old vs. new each frame.
pub fn should_connect_viewer(view: SessionView) -> bool {
    view.shows_browser()
}

/// What the per-frame viewer-ws reconciler should do, given whether the
/// browser panel is visible (`want`), whether a ws handle currently exists
/// (`connected`), and whether a prior drop has blocked auto-reconnect
/// (`retry_blocked`). PURE — drives `App::reconcile_viewer` so the lifecycle
/// decision is unit-testable without a GUI.
///
/// The load-bearing rule: while the panel stays visible we connect at most
/// once. After the ws drops (e.g. the agent runs `browser.enabled=false` so
/// the screencast never starts), `retry_blocked` is set and we do NOT
/// reconnect every frame — that would busy-loop the hub/agent. Hiding the
/// panel disconnects AND clears the block, so toggling the browser away and
/// back is the deliberate reconnect gesture.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ViewerAction {
    /// Open the viewer ws (rising edge: panel shown, no handle, not blocked).
    Connect,
    /// Drop the viewer ws (panel hidden while a handle is live).
    Disconnect,
    /// Do nothing this frame.
    Idle,
}

/// Decide the reconcile action. Also reports whether the retry-block latch
/// should be cleared (always cleared when the panel isn't wanted, so the next
/// show can reconnect). Returns `(action, clear_retry_block)`.
pub fn reconcile_viewer_action(
    want: bool,
    connected: bool,
    retry_blocked: bool,
) -> (ViewerAction, bool) {
    if !want {
        // Panel hidden / left session: drop any live ws and clear the latch.
        let action = if connected {
            ViewerAction::Disconnect
        } else {
            ViewerAction::Idle
        };
        return (action, true);
    }
    // Panel visible: connect once, unless a prior drop blocked retry.
    if !connected && !retry_blocked {
        (ViewerAction::Connect, false)
    } else {
        (ViewerAction::Idle, false)
    }
}

/// Lower/upper bounds for the browser panel's share of the split, as a
/// fraction of the total width. Keeps either panel from being dragged to
/// nothing.
pub const SPLIT_MIN: f32 = 0.2;
pub const SPLIT_MAX: f32 = 0.8;

/// Clamp a split ratio (browser panel's fraction of total width) to
/// `[SPLIT_MIN, SPLIT_MAX]`. PURE.
pub fn clamp_split_ratio(ratio: f32) -> f32 {
    ratio.clamp(SPLIT_MIN, SPLIT_MAX)
}

/// The browser SidePanel's allowed pixel-width range for a given total
/// window width, derived from [`SPLIT_MIN`]/[`SPLIT_MAX`]. egui's resizable
/// `SidePanel` persists the *actual* width itself (keyed by its Id); this
/// just bounds how far the user can drag the separator. PURE.
pub fn split_width_range(total_width: f32) -> std::ops::RangeInclusive<f32> {
    let w = total_width.max(0.0);
    (w * clamp_split_ratio(SPLIT_MIN))..=(w * clamp_split_ratio(SPLIT_MAX))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_split() {
        assert_eq!(SessionView::default(), SessionView::Split);
    }

    #[test]
    fn shows_browser_only_when_browser_visible() {
        assert!(SessionView::Split.shows_browser());
        assert!(SessionView::BrowserOnly.shows_browser());
        assert!(!SessionView::TerminalOnly.shows_browser());
    }

    #[test]
    fn cycle_walks_three_states_and_wraps() {
        let v = SessionView::TerminalOnly;
        let v = v.cycle();
        assert_eq!(v, SessionView::Split);
        let v = v.cycle();
        assert_eq!(v, SessionView::BrowserOnly);
        let v = v.cycle();
        assert_eq!(v, SessionView::TerminalOnly, "wraps back to start");
    }

    #[test]
    fn should_connect_matches_browser_visibility() {
        // The load-bearing lazy-connect decision: connect iff the browser
        // panel is on screen. Split/BrowserOnly → true, TerminalOnly → false.
        assert!(should_connect_viewer(SessionView::Split));
        assert!(should_connect_viewer(SessionView::BrowserOnly));
        assert!(!should_connect_viewer(SessionView::TerminalOnly));
    }

    #[test]
    fn reconcile_connects_on_rising_edge() {
        // Panel shown, no ws, not blocked → connect; latch untouched.
        assert_eq!(
            reconcile_viewer_action(true, false, false),
            (ViewerAction::Connect, false)
        );
    }

    #[test]
    fn reconcile_idle_when_already_connected() {
        assert_eq!(
            reconcile_viewer_action(true, true, false),
            (ViewerAction::Idle, false)
        );
    }

    #[test]
    fn reconcile_blocked_does_not_reconnect() {
        // The load-bearing fix: after a drop (blocked) with the panel still
        // visible, we must NOT reconnect — no busy-loop against a
        // never-streaming agent.
        assert_eq!(
            reconcile_viewer_action(true, false, true),
            (ViewerAction::Idle, false)
        );
    }

    #[test]
    fn reconcile_hiding_panel_disconnects_and_clears_block() {
        // Panel hidden with a live ws → disconnect AND clear the latch so the
        // next show reconnects (toggle-away-and-back is the reconnect gesture).
        assert_eq!(
            reconcile_viewer_action(false, true, true),
            (ViewerAction::Disconnect, true)
        );
    }

    #[test]
    fn reconcile_hidden_idle_still_clears_block() {
        // Panel hidden with no ws: nothing to drop, but still clear the latch.
        assert_eq!(
            reconcile_viewer_action(false, false, true),
            (ViewerAction::Idle, true)
        );
    }

    #[test]
    fn clamp_split_ratio_bounds() {
        assert_eq!(clamp_split_ratio(0.5), 0.5);
        assert_eq!(clamp_split_ratio(0.0), SPLIT_MIN);
        assert_eq!(clamp_split_ratio(1.0), SPLIT_MAX);
        assert_eq!(clamp_split_ratio(-3.0), SPLIT_MIN);
        assert_eq!(clamp_split_ratio(SPLIT_MIN), SPLIT_MIN);
        assert_eq!(clamp_split_ratio(SPLIT_MAX), SPLIT_MAX);
    }

    #[test]
    fn split_width_range_scales_with_total() {
        let r = split_width_range(1000.0);
        assert_eq!(*r.start(), 200.0);
        assert_eq!(*r.end(), 800.0);
        // Degenerate total clamps to a non-negative empty-ish range.
        let r = split_width_range(-5.0);
        assert_eq!(*r.start(), 0.0);
        assert_eq!(*r.end(), 0.0);
    }
}
