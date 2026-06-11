//! UI state model — the testable core of the app.
//!
//! P6 reshaped the P3 `Screen{Connecting,Picker,Session}` enum into a
//! persistent-sidebar model: [`AppModel`] always carries the workspace
//! list (the sidebar), a connection [`Phase`], and an optional
//! [`ActiveSession`] (the content area). `apply_event` stays a PURE
//! reducer folding a `BackendEvent` into the model — no I/O, no
//! channels — so every transition is table-testable without a GUI.
//!
//! Behavior change vs P3 (documented per the plan): a mid-session
//! disconnect no longer lands the user on a picker screen. The model is
//! RETAINED (sidebar + dimmed session panels stay up) while
//! `phase = Connecting{reconnecting: true}`, and on the re-`Welcome`
//! the reducer asks the caller to auto-REOPEN the last active
//! workspace — the hub tore the session down on disconnect, but the
//! tmux session on the agent kept running, so reopening reattaches it.
//! That's the hero flow: a connection blip self-heals back into the
//! same running claude session.

use crate::backend::BackendEvent;
use cloudcode_proto::{AgentInfo, WorkspaceInfo};

/// Connection phase. Orthogonal to "is a session open" — a reconnect
/// keeps the session state around (dimmed) while the wire heals.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Phase {
    /// Waiting for the hub `Welcome`. `reconnecting = true` means the
    /// wire died after we were Ready (the UI shows the orange banner +
    /// dims the panels instead of a full-screen spinner); `false` is
    /// the initial launch (full-screen "connecting…" — there's nothing
    /// else to show yet).
    Connecting { reconnecting: bool },
    /// Hub `Welcome` received; the sidebar is live.
    Ready,
}

/// The open PTY session shown in the content area.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActiveSession {
    pub agent: String,
    pub workspace: String,
    /// Working directory the hub reported (top-bar display).
    pub cwd: String,
    /// Hub-minted PTY session id — needed to open the viewer ws
    /// (`/v1/viewer/ws?session=<id>`).
    pub session_id: String,
}

/// The whole UI model. `main.rs` owns one and renders from it; only
/// `apply_event` (and the explicit `begin_switch` user action) mutate it.
#[derive(Debug, Clone)]
pub struct AppModel {
    pub phase: Phase,
    /// Hub base URL — shown in connect/reconnect status lines.
    pub hub_url: String,
    /// Hub-confirmed account name (bottom status cell).
    pub account: String,
    /// The sidebar's workspace list (refreshed via ListWorkspaces).
    pub workspaces: Vec<WorkspaceInfo>,
    /// Online agents (new-workspace dropdown), via ListAgents.
    pub agents: Vec<AgentInfo>,
    pub active: Option<ActiveSession>,
    /// The workspace to auto-reopen after a reconnect: set on every
    /// successful open AND at switch time (the switch target), cleared
    /// when a session ends deliberately (SessionClosed) so a takeover
    /// from another client isn't silently stolen back. `(agent, workspace)`.
    pub last_active: Option<(String, String)>,
    /// Transient error toast (failed open/create/reopen) — cleared by
    /// the next successful action.
    pub error: Option<String>,
    /// Terminal failure (bad config, hub rejection at launch) — the UI
    /// shows a full-screen error with a Quit button.
    pub fatal: Option<String>,
}

impl AppModel {
    pub fn new(hub_url: impl Into<String>) -> AppModel {
        AppModel {
            phase: Phase::Connecting {
                reconnecting: false,
            },
            hub_url: hub_url.into(),
            account: String::new(),
            workspaces: Vec::new(),
            agents: Vec::new(),
            active: None,
            last_active: None,
            error: None,
            fatal: None,
        }
    }

    /// A model that starts dead (config failed before the backend even
    /// spawned).
    pub fn fatal(message: impl Into<String>) -> AppModel {
        let mut m = AppModel::new("");
        m.fatal = Some(message.into());
        m
    }

    /// User clicked a different workspace while a session is open: mark
    /// the target as the reopen-on-reconnect workspace and drop the
    /// (about-to-die) active session from the model. The caller then
    /// sends `UiCommand::SwitchWorkspace`, which cycles the connection;
    /// the reconnect path's auto-reopen does the rest — switching IS the
    /// hero flow with a deliberate trigger.
    pub fn begin_switch(&mut self, agent: impl Into<String>, workspace: impl Into<String>) {
        self.last_active = Some((agent.into(), workspace.into()));
        self.active = None;
        self.error = None;
    }
}

/// A command the reducer asks the caller to issue after a transition.
/// Keeps `apply_event` pure: it decides *what* should happen next, the
/// caller does the channel send.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FollowUp {
    ListWorkspaces,
    ListAgents,
    /// Auto-reopen this workspace (reconnect recovery / switch landing).
    OpenSession { agent: String, workspace: String },
}

/// Fold one backend event into the model. PURE — no I/O.
pub fn apply_event(model: &mut AppModel, event: BackendEvent) -> Vec<FollowUp> {
    match event {
        // The hub welcomed us (initial connect, reconnect, or the
        // welcome leg of a workspace switch). Refresh both sidebar
        // lists; if we were recovering from a drop/switch and know the
        // last active workspace, auto-reopen it (the hub tore the old
        // session down when the previous connection died, but its tmux
        // session is still running on the agent — reopening reattaches).
        BackendEvent::Connected { account } => {
            let was_reconnecting = matches!(
                model.phase,
                Phase::Connecting { reconnecting: true }
            );
            model.phase = Phase::Ready;
            model.account = account;
            // Whatever session we showed is gone server-side (hub
            // teardown on disconnect). The auto-reopen below brings a
            // fresh one back if we know what to reopen.
            model.active = None;
            let mut follow = vec![FollowUp::ListWorkspaces, FollowUp::ListAgents];
            if was_reconnecting {
                if let Some((agent, workspace)) = model.last_active.clone() {
                    follow.push(FollowUp::OpenSession { agent, workspace });
                }
            }
            follow
        }

        BackendEvent::Workspaces(items) => {
            model.workspaces = items;
            vec![]
        }

        BackendEvent::Agents(items) => {
            model.agents = items;
            vec![]
        }

        BackendEvent::SessionOpened {
            agent,
            workspace,
            cwd,
            session_id,
        } => {
            model.last_active = Some((agent.clone(), workspace.clone()));
            model.active = Some(ActiveSession {
                agent,
                workspace,
                cwd,
                session_id,
            });
            model.error = None;
            // Refresh the sidebar so the freshly-opened row shows its
            // tmux-alive dot without waiting for a manual refresh.
            vec![FollowUp::ListWorkspaces]
        }

        // The session ended deliberately (claude exited, takeover by
        // another client, reset). Clear the reopen target too: a
        // reconnect must not silently steal the workspace back from
        // whoever took it over.
        BackendEvent::SessionClosed(reason) => {
            model.active = None;
            model.last_active = None;
            model.error = reason;
            vec![FollowUp::ListWorkspaces]
        }

        // Failed open/create/reopen (or an in-session PtyError): show a
        // toast, keep the sidebar. EXCEPT during the initial connect,
        // where the backend signals non-retryable failures (bad config /
        // hub rejection) this way — that's fatal, there's no UI to fall
        // back to.
        BackendEvent::SessionError(message) => {
            if matches!(
                model.phase,
                Phase::Connecting {
                    reconnecting: false
                }
            ) {
                model.fatal = Some(message);
            } else {
                model.error = Some(message);
            }
            vec![]
        }

        // PTY bytes are intercepted by main.rs and fed to the terminal
        // panel directly; reaching here means no panel — ignore.
        BackendEvent::PtyBytes(_) => vec![],

        // The wire died. KEEP the model (sidebar list, active session
        // state — panels render dimmed) and flip into the reconnecting
        // phase; the backend's backoff loop will re-emit Connected. A
        // fatal model stays fatal.
        BackendEvent::Disconnected => {
            if model.fatal.is_none() {
                model.phase = Phase::Connecting { reconnecting: true };
            }
            vec![]
        }
    }
}

// ---------------------------------------------------------------------------
// Sidebar row badges + click decisions (pure, table-tested)
// ---------------------------------------------------------------------------

/// Status-dot color class for a sidebar workspace row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dot {
    /// Agent online + tmux session alive (green — "running").
    Running,
    /// Agent online, no tmux session (faint — "saved").
    Saved,
    /// Agent offline (dark — row not openable).
    Offline,
}

/// Everything the sidebar needs to paint one row, derived purely from
/// the `WorkspaceInfo` + whether the row is the active session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RowBadge {
    pub dot: Dot,
    /// Another cloudcode client is attached to this workspace — show
    /// the "attached elsewhere" hint. Suppressed on the ACTIVE row
    /// (that attached client is us).
    pub attached_elsewhere: bool,
    /// Clicking the row can open/switch (the agent is online and it's
    /// not already the active session).
    pub clickable: bool,
}

pub fn row_badge(w: &WorkspaceInfo, is_active: bool) -> RowBadge {
    let dot = if !w.agent_online {
        Dot::Offline
    } else if w.tmux_alive {
        Dot::Running
    } else {
        Dot::Saved
    };
    RowBadge {
        dot,
        attached_elsewhere: w.agent_online && w.has_client && !is_active,
        clickable: w.agent_online && !is_active,
    }
}

/// What clicking a workspace row should do. PURE — the render loop maps
/// this onto `UiCommand`s.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SwitchDecision {
    /// Not Ready / agent offline / already the active row: do nothing.
    Ignore,
    /// No session open — plain `OpenSession` on the live connection.
    Open,
    /// A session is open. The hub REJECTS a second OpenSession on the
    /// same connection ("session already open") and `ClientToHub::Close`
    /// closes the WHOLE connection, so the cheapest correct switch is a
    /// deliberate connection cycle: `begin_switch` (sets last_active to
    /// the target) + `UiCommand::SwitchWorkspace` (backend sends Close
    /// and treats it as WireLost → reconnect → re-Welcome), and the
    /// reducer's auto-reopen lands on the target. The OLD workspace's
    /// tmux keeps running on the agent — switching loses nothing.
    SwitchViaReconnect,
}

pub fn switch_decision(model: &AppModel, agent: &str, workspace: &str) -> SwitchDecision {
    if model.phase != Phase::Ready {
        return SwitchDecision::Ignore;
    }
    let target_online = model
        .workspaces
        .iter()
        .find(|w| w.agent == agent && w.name == workspace)
        .is_some_and(|w| w.agent_online);
    if !target_online {
        return SwitchDecision::Ignore;
    }
    match &model.active {
        Some(a) if a.agent == agent && a.workspace == workspace => SwitchDecision::Ignore,
        Some(_) => SwitchDecision::SwitchViaReconnect,
        None => SwitchDecision::Open,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::BackendEvent;

    fn ws(name: &str, online: bool, tmux: bool, client: bool) -> WorkspaceInfo {
        WorkspaceInfo {
            name: name.to_string(),
            agent: "agentA".to_string(),
            agent_online: online,
            tmux_alive: tmux,
            has_client: client,
        }
    }

    fn ready_model(workspaces: Vec<WorkspaceInfo>) -> AppModel {
        let mut m = AppModel::new("http://h");
        m.phase = Phase::Ready;
        m.account = "me".into();
        m.workspaces = workspaces;
        m
    }

    fn opened(model: &mut AppModel, agent: &str, workspace: &str) {
        apply_event(
            model,
            BackendEvent::SessionOpened {
                agent: agent.into(),
                workspace: workspace.into(),
                cwd: format!("/home/{workspace}"),
                session_id: "sess-1".into(),
            },
        );
    }

    // --- connect → ready ---

    #[test]
    fn initial_connect_goes_ready_and_requests_both_lists() {
        let mut m = AppModel::new("http://h");
        let follow = apply_event(
            &mut m,
            BackendEvent::Connected {
                account: "me".into(),
            },
        );
        assert_eq!(m.phase, Phase::Ready);
        assert_eq!(m.account, "me");
        assert!(m.active.is_none());
        // Initial connect: no auto-reopen (nothing was active).
        assert_eq!(
            follow,
            vec![FollowUp::ListWorkspaces, FollowUp::ListAgents]
        );
    }

    #[test]
    fn workspaces_event_refreshes_sidebar_list() {
        let mut m = ready_model(Vec::new());
        let follow = apply_event(
            &mut m,
            BackendEvent::Workspaces(vec![ws("proj", true, false, false)]),
        );
        assert_eq!(m.workspaces.len(), 1);
        assert_eq!(m.workspaces[0].name, "proj");
        assert!(follow.is_empty());
    }

    #[test]
    fn agents_event_fills_dropdown_source() {
        let mut m = ready_model(Vec::new());
        apply_event(
            &mut m,
            BackendEvent::Agents(vec![AgentInfo {
                name: "mac".into(),
                current: false,
            }]),
        );
        assert_eq!(m.agents.len(), 1);
        assert_eq!(m.agents[0].name, "mac");
    }

    // --- open → active ---

    #[test]
    fn session_opened_sets_active_and_remembers_last_active() {
        let mut m = ready_model(vec![ws("proj", true, false, false)]);
        m.error = Some("old toast".into());
        opened(&mut m, "agentA", "proj");
        let a = m.active.as_ref().expect("active session");
        assert_eq!(a.agent, "agentA");
        assert_eq!(a.workspace, "proj");
        assert_eq!(a.cwd, "/home/proj");
        assert_eq!(a.session_id, "sess-1");
        assert_eq!(
            m.last_active,
            Some(("agentA".to_string(), "proj".to_string()))
        );
        assert!(m.error.is_none(), "successful open clears the toast");
    }

    // --- closed → inactive ---

    #[test]
    fn session_closed_clears_active_and_reopen_target() {
        let mut m = ready_model(vec![ws("proj", true, true, false)]);
        opened(&mut m, "agentA", "proj");
        let follow = apply_event(
            &mut m,
            BackendEvent::SessionClosed(Some("taken over".into())),
        );
        assert!(m.active.is_none());
        assert!(
            m.last_active.is_none(),
            "deliberate close must not auto-reopen on the next reconnect"
        );
        assert_eq!(m.error.as_deref(), Some("taken over"));
        assert_eq!(follow, vec![FollowUp::ListWorkspaces]);
    }

    // --- disconnected → reconnecting, model retained ---

    #[test]
    fn disconnect_mid_session_keeps_model_and_enters_reconnecting() {
        let mut m = ready_model(vec![ws("proj", true, true, false)]);
        opened(&mut m, "agentA", "proj");
        let follow = apply_event(&mut m, BackendEvent::Disconnected);
        assert_eq!(m.phase, Phase::Connecting { reconnecting: true });
        // The model survives: sidebar list AND the (dimmed) session.
        assert_eq!(m.workspaces.len(), 1);
        assert!(m.active.is_some(), "panels stay up, dimmed — not cleared");
        assert!(follow.is_empty());
    }

    #[test]
    fn reconnect_auto_reopens_last_active_workspace() {
        // THE hero flow: blip mid-session → reconnect → auto-reattach.
        let mut m = ready_model(vec![ws("proj", true, true, false)]);
        opened(&mut m, "agentA", "proj");
        apply_event(&mut m, BackendEvent::Disconnected);
        let follow = apply_event(
            &mut m,
            BackendEvent::Connected {
                account: "me".into(),
            },
        );
        assert_eq!(m.phase, Phase::Ready);
        assert!(
            m.active.is_none(),
            "the old session died with the connection; reopen is in flight"
        );
        assert_eq!(
            follow,
            vec![
                FollowUp::ListWorkspaces,
                FollowUp::ListAgents,
                FollowUp::OpenSession {
                    agent: "agentA".into(),
                    workspace: "proj".into()
                }
            ]
        );
    }

    #[test]
    fn reconnect_without_prior_session_just_refreshes() {
        let mut m = ready_model(Vec::new());
        apply_event(&mut m, BackendEvent::Disconnected);
        let follow = apply_event(
            &mut m,
            BackendEvent::Connected {
                account: "me".into(),
            },
        );
        assert_eq!(
            follow,
            vec![FollowUp::ListWorkspaces, FollowUp::ListAgents]
        );
    }

    #[test]
    fn failed_auto_reopen_lands_on_sidebar_with_toast() {
        let mut m = ready_model(vec![ws("proj", true, true, false)]);
        opened(&mut m, "agentA", "proj");
        apply_event(&mut m, BackendEvent::Disconnected);
        apply_event(
            &mut m,
            BackendEvent::Connected {
                account: "me".into(),
            },
        );
        // The auto-reopen failed (e.g. agent went offline during the blip).
        apply_event(&mut m, BackendEvent::SessionError("agent offline".into()));
        assert_eq!(m.phase, Phase::Ready, "stay on the sidebar");
        assert!(m.active.is_none());
        assert_eq!(m.error.as_deref(), Some("agent offline"));
        assert!(m.fatal.is_none(), "a failed reopen is not fatal");
    }

    // --- errors ---

    #[test]
    fn session_error_during_initial_connect_is_fatal() {
        let mut m = AppModel::new("http://h");
        apply_event(&mut m, BackendEvent::SessionError("bad token".into()));
        assert_eq!(m.fatal.as_deref(), Some("bad token"));
    }

    #[test]
    fn session_error_while_ready_is_a_toast() {
        let mut m = ready_model(vec![ws("proj", true, false, false)]);
        apply_event(&mut m, BackendEvent::SessionError("agent offline".into()));
        assert!(m.fatal.is_none());
        assert_eq!(m.error.as_deref(), Some("agent offline"));
    }

    #[test]
    fn disconnect_on_fatal_stays_fatal() {
        let mut m = AppModel::fatal("config error");
        apply_event(&mut m, BackendEvent::Disconnected);
        assert!(m.fatal.is_some());
        assert_eq!(
            m.phase,
            Phase::Connecting {
                reconnecting: false
            },
            "fatal model never enters the reconnecting phase"
        );
    }

    // --- switch decisions (click-to-switch command sequence) ---

    #[test]
    fn click_with_no_session_opens_directly() {
        let m = ready_model(vec![ws("proj", true, false, false)]);
        assert_eq!(
            switch_decision(&m, "agentA", "proj"),
            SwitchDecision::Open
        );
    }

    #[test]
    fn click_other_workspace_mid_session_switches_via_reconnect() {
        // The hub rejects a second OpenSession on a live connection and
        // Close kills the whole connection — so switching cycles it.
        let mut m = ready_model(vec![
            ws("proj", true, true, false),
            ws("other", true, false, false),
        ]);
        opened(&mut m, "agentA", "proj");
        assert_eq!(
            switch_decision(&m, "agentA", "other"),
            SwitchDecision::SwitchViaReconnect
        );
    }

    #[test]
    fn click_active_row_or_offline_row_is_ignored() {
        let mut m = ready_model(vec![
            ws("proj", true, true, false),
            ws("dead", false, false, false),
        ]);
        opened(&mut m, "agentA", "proj");
        assert_eq!(
            switch_decision(&m, "agentA", "proj"),
            SwitchDecision::Ignore,
            "already active"
        );
        assert_eq!(
            switch_decision(&m, "agentA", "dead"),
            SwitchDecision::Ignore,
            "agent offline"
        );
        assert_eq!(
            switch_decision(&m, "agentA", "ghost"),
            SwitchDecision::Ignore,
            "unknown workspace"
        );
    }

    #[test]
    fn click_ignored_while_reconnecting() {
        let mut m = ready_model(vec![ws("proj", true, false, false)]);
        apply_event(&mut m, BackendEvent::Disconnected);
        assert_eq!(
            switch_decision(&m, "agentA", "proj"),
            SwitchDecision::Ignore
        );
    }

    #[test]
    fn begin_switch_retargets_reopen_and_drops_active() {
        let mut m = ready_model(vec![
            ws("proj", true, true, false),
            ws("other", true, false, false),
        ]);
        opened(&mut m, "agentA", "proj");
        m.begin_switch("agentA", "other");
        assert!(m.active.is_none());
        assert_eq!(
            m.last_active,
            Some(("agentA".to_string(), "other".to_string()))
        );
        // The full switch sequence: backend cycles the wire → Disconnected
        // → Connected → auto-reopen lands on the TARGET workspace.
        apply_event(&mut m, BackendEvent::Disconnected);
        let follow = apply_event(
            &mut m,
            BackendEvent::Connected {
                account: "me".into(),
            },
        );
        assert!(follow.contains(&FollowUp::OpenSession {
            agent: "agentA".into(),
            workspace: "other".into()
        }));
    }

    // --- sidebar row badge derivation (table test) ---

    #[test]
    fn row_badge_table() {
        // (online, tmux, has_client, is_active) → expected badge
        let cases = [
            // running: agent online + tmux alive
            (true, true, false, false, Dot::Running, false, true),
            // saved: online, no tmux
            (true, false, false, false, Dot::Saved, false, true),
            // offline agent: dark dot, not clickable
            (false, true, false, false, Dot::Offline, false, false),
            // attached elsewhere: hint shown on a non-active row
            (true, true, true, false, Dot::Running, true, true),
            // the ACTIVE row: attached client is us — no hint, no click
            (true, true, true, true, Dot::Running, false, false),
            // offline + has_client: stale flag, agent gone → no hint
            (false, false, true, false, Dot::Offline, false, false),
        ];
        for (online, tmux, client, active, dot, hint, click) in cases {
            let b = row_badge(&ws("w", online, tmux, client), active);
            assert_eq!(
                b,
                RowBadge {
                    dot,
                    attached_elsewhere: hint,
                    clickable: click
                },
                "case online={online} tmux={tmux} client={client} active={active}"
            );
        }
    }
}
