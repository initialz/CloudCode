//! UI state machine — the testable core of the app.
//!
//! `Screen` is the app's current view; `apply_event` is a PURE function
//! that folds a `BackendEvent` into the next `Screen`. Keeping the
//! transition logic side-effect-free lets us table-test it without a
//! GUI (egui rendering is smoke-only).

use crate::backend::BackendEvent;
use cloudcode_proto::WorkspaceInfo;

/// Which screen the app is currently showing.
#[derive(Debug, Clone)]
pub enum Screen {
    /// Waiting for the hub `Welcome` — either the initial connect or a
    /// reconnect after the wire died. `reconnecting` flips the status
    /// line from "connecting to …" to "reconnecting …" so a mid-session
    /// drop reads as a transient blip, not a fresh launch.
    Connecting {
        hub_url: String,
        reconnecting: bool,
    },
    /// Workspace picker. `account` is the hub-confirmed account name
    /// (shown in the header). `error` is a transient banner (e.g. a
    /// failed open or create) shown until the next successful action.
    Picker {
        account: String,
        workspaces: Vec<WorkspaceInfo>,
        error: Option<String>,
    },
    /// A PTY session is open. Task 3 replaces `output` with a real
    /// `TerminalPanel`; for now it's a lossy-utf8 scrollback buffer so
    /// the skeleton visibly works.
    Session {
        agent: String,
        workspace: String,
        /// Working directory the hub reported for the session. Carried
        /// for display now; Task 3's TerminalPanel can use it too.
        cwd: String,
        output: String,
    },
    /// Terminal failure — the connection is gone or the hub rejected us.
    Error { message: String },
}

impl Screen {
    pub fn connecting(hub_url: impl Into<String>) -> Self {
        Screen::Connecting {
            hub_url: hub_url.into(),
            reconnecting: false,
        }
    }

    /// The "reconnecting after a mid-session drop" variant of the
    /// connecting screen (greys the terminal, shows a reconnecting line).
    pub fn reconnecting(hub_url: impl Into<String>) -> Self {
        Screen::Connecting {
            hub_url: hub_url.into(),
            reconnecting: true,
        }
    }
}

/// Cap the placeholder scrollback so a chatty session can't grow the
/// buffer without bound before Task 3's real terminal lands.
const SESSION_BUF_CAP: usize = 256 * 1024;

/// Fold one backend event into the next screen. PURE — no I/O, no
/// channels. This is the unit-tested heart of the app.
///
/// Returns the next screen plus an optional follow-up: when we land on
/// the picker after connecting we want to auto-request the workspace
/// list, but issuing that command is the caller's job (keeps this pure).
pub fn apply_event(screen: Screen, event: BackendEvent) -> (Screen, Vec<FollowUp>) {
    // Best-effort account name carried across transitions so the picker
    // header keeps showing it even after a list refresh / error.
    let account = screen_account(&screen);
    // The hub URL we last knew, carried into the reconnecting screen so
    // its status line can name the host.
    let hub_url = screen_hub_url(&screen);

    match (screen, event) {
        // Connected: leave the connecting screen for an empty picker and
        // ask the caller to fetch the workspace list.
        (Screen::Connecting { .. }, BackendEvent::Connected { account }) => (
            Screen::Picker {
                account,
                workspaces: Vec::new(),
                error: None,
            },
            vec![FollowUp::ListWorkspaces],
        ),

        // Workspace list arrived (could land while on the picker, or
        // right after connecting if events interleave) — refresh the
        // list and keep us on the picker.
        (_, BackendEvent::Workspaces(items)) => (
            Screen::Picker {
                account,
                workspaces: items,
                error: None,
            },
            vec![],
        ),

        // Session opened — switch to the (placeholder) terminal screen.
        (
            _,
            BackendEvent::SessionOpened {
                agent,
                workspace,
                cwd,
            },
        ) => (
            Screen::Session {
                agent,
                workspace,
                cwd,
                output: String::new(),
            },
            vec![],
        ),

        // A failed open/create while picking: show the error banner on
        // the picker rather than tearing the whole UI down.
        (Screen::Picker { account, workspaces, .. }, BackendEvent::SessionError(message)) => (
            Screen::Picker {
                account,
                workspaces,
                error: Some(message),
            },
            vec![],
        ),
        // Session error anywhere else is terminal enough to surface.
        (_, BackendEvent::SessionError(message)) => (Screen::Error { message }, vec![]),

        // PTY bytes only matter while a session is open; append lossily.
        (
            Screen::Session {
                agent,
                workspace,
                cwd,
                mut output,
            },
            BackendEvent::PtyBytes(bytes),
        ) => {
            output.push_str(&String::from_utf8_lossy(&bytes));
            if output.len() > SESSION_BUF_CAP {
                // Drop the oldest half on overflow.
                let start = output.len() - SESSION_BUF_CAP / 2;
                // Snap to a char boundary so we don't split a UTF-8 seq.
                let start = (start..output.len())
                    .find(|&i| output.is_char_boundary(i))
                    .unwrap_or(output.len());
                output = output.split_off(start);
            }
            (
                Screen::Session {
                    agent,
                    workspace,
                    cwd,
                    output,
                },
                vec![],
            )
        }
        // PTY bytes with no session — ignore, stay put.
        (screen, BackendEvent::PtyBytes(_)) => (screen, vec![]),

        // Connected event when not connecting (e.g. a stray reconnect) —
        // ignore.
        (screen, BackendEvent::Connected { .. }) => (screen, vec![]),

        // The wire died mid-session (or on the picker): drop into the
        // reconnecting screen rather than a terminal Error, so the
        // backend's backoff loop can recover us. The UI greys the
        // terminal and shows a "reconnecting…" line; when the backend
        // re-emits `Connected` we land back on the picker (the live tmux
        // session, if any, is reattached by reopening the workspace).
        //
        // A terminal `Error` stays put — that's a fatal, user-dismissed
        // state, not a recoverable drop.
        (Screen::Error { message }, BackendEvent::Disconnected) => {
            (Screen::Error { message }, vec![])
        }
        (_, BackendEvent::Disconnected) => (Screen::reconnecting(hub_url), vec![]),
    }
}

/// The hub URL a screen knows about, if any. Only the connecting screen
/// carries it today; everything else yields an empty string (the
/// reconnecting status line then omits the host, which is fine).
fn screen_hub_url(screen: &Screen) -> String {
    match screen {
        Screen::Connecting { hub_url, .. } => hub_url.clone(),
        _ => String::new(),
    }
}

/// The account name a screen carries, if any. Used to preserve the
/// picker header across list refreshes / error banners.
fn screen_account(screen: &Screen) -> String {
    match screen {
        Screen::Picker { account, .. } => account.clone(),
        _ => String::new(),
    }
}

/// A command the reducer asks the caller to issue after a transition.
/// Keeps `apply_event` pure: it decides *what* should happen next, the
/// caller does the channel send.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FollowUp {
    ListWorkspaces,
}

/// One-word status badge for a workspace row. Mirrors the CLI client's
/// badge priority (offline > active/takeover > saved > online).
pub fn badge(w: &WorkspaceInfo) -> &'static str {
    if !w.agent_online {
        "offline"
    } else if w.has_client {
        "takeover"
    } else if w.tmux_alive {
        "reattach"
    } else {
        "online"
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

    fn picker(workspaces: Vec<WorkspaceInfo>, error: Option<&str>) -> Screen {
        Screen::Picker {
            account: "me".into(),
            workspaces,
            error: error.map(String::from),
        }
    }

    fn session(output: &str) -> Screen {
        Screen::Session {
            agent: "a".into(),
            workspace: "w".into(),
            cwd: "/w".into(),
            output: output.into(),
        }
    }

    #[test]
    fn connecting_plus_connected_goes_to_empty_picker_and_requests_list() {
        let (next, follow) = apply_event(
            Screen::connecting("http://h"),
            BackendEvent::Connected {
                account: "me".into(),
            },
        );
        match next {
            Screen::Picker {
                account,
                workspaces,
                error,
            } => {
                assert_eq!(account, "me");
                assert!(workspaces.is_empty());
                assert!(error.is_none());
            }
            other => panic!("expected picker, got {:?}", other),
        }
        assert_eq!(follow, vec![FollowUp::ListWorkspaces]);
    }

    #[test]
    fn picker_plus_workspaces_updates_list() {
        let start = picker(Vec::new(), Some("old error"));
        let (next, follow) =
            apply_event(start, BackendEvent::Workspaces(vec![ws("proj", true, false, false)]));
        match next {
            Screen::Picker {
                account,
                workspaces,
                error,
            } => {
                assert_eq!(account, "me", "account survives a list refresh");
                assert_eq!(workspaces.len(), 1);
                assert_eq!(workspaces[0].name, "proj");
                assert!(error.is_none(), "list refresh clears the error banner");
            }
            other => panic!("expected picker, got {:?}", other),
        }
        assert!(follow.is_empty());
    }

    #[test]
    fn picker_plus_session_opened_goes_to_session() {
        let start = picker(vec![ws("proj", true, false, false)], None);
        let (next, _) = apply_event(
            start,
            BackendEvent::SessionOpened {
                agent: "agentA".into(),
                workspace: "proj".into(),
                cwd: "/home/proj".into(),
            },
        );
        match next {
            Screen::Session {
                agent,
                workspace,
                cwd,
                output,
            } => {
                assert_eq!(agent, "agentA");
                assert_eq!(workspace, "proj");
                assert_eq!(cwd, "/home/proj");
                assert!(output.is_empty());
            }
            other => panic!("expected session, got {:?}", other),
        }
    }

    #[test]
    fn picker_session_error_shows_banner_not_error_screen() {
        let start = picker(vec![ws("proj", true, false, false)], None);
        let (next, _) = apply_event(start, BackendEvent::SessionError("agent offline".into()));
        match next {
            Screen::Picker { error, .. } => assert_eq!(error.as_deref(), Some("agent offline")),
            other => panic!("expected picker with banner, got {:?}", other),
        }
    }

    #[test]
    fn session_error_outside_picker_is_terminal() {
        let (next, _) = apply_event(
            Screen::connecting("http://h"),
            BackendEvent::SessionError("boom".into()),
        );
        assert!(matches!(next, Screen::Error { .. }));
    }

    #[test]
    fn session_appends_pty_bytes_lossily() {
        let start = session("hi ");
        let (next, _) = apply_event(start, BackendEvent::PtyBytes(b"there".to_vec()));
        match next {
            Screen::Session { output, .. } => assert_eq!(output, "hi there"),
            other => panic!("expected session, got {:?}", other),
        }
    }

    #[test]
    fn disconnected_from_any_live_screen_goes_to_reconnecting() {
        // A mid-session / mid-picker drop must NOT strand the user on a
        // terminal Error — it routes to the reconnecting screen so the
        // backend's backoff loop can recover them to the picker.
        for screen in [
            Screen::connecting("http://h"),
            picker(Vec::new(), None),
            session(""),
        ] {
            let (next, follow) = apply_event(screen, BackendEvent::Disconnected);
            match next {
                Screen::Connecting { reconnecting, .. } => assert!(reconnecting),
                other => panic!("expected reconnecting screen, got {:?}", other),
            }
            assert!(follow.is_empty());
        }
    }

    #[test]
    fn disconnected_carries_hub_url_when_known() {
        let (next, _) = apply_event(Screen::connecting("http://h"), BackendEvent::Disconnected);
        match next {
            Screen::Connecting { hub_url, reconnecting } => {
                assert_eq!(hub_url, "http://h");
                assert!(reconnecting);
            }
            other => panic!("expected reconnecting screen, got {:?}", other),
        }
    }

    #[test]
    fn disconnected_on_error_stays_error() {
        let (next, _) = apply_event(
            Screen::Error { message: "boom".into() },
            BackendEvent::Disconnected,
        );
        assert!(matches!(next, Screen::Error { .. }));
    }

    #[test]
    fn reconnecting_then_connected_returns_to_picker() {
        // The full recovery path: a drop puts us in reconnecting, and the
        // backend's re-`Welcome` (Connected) lands us back on the picker
        // and re-requests the workspace list.
        let (dropped, _) = apply_event(session(""), BackendEvent::Disconnected);
        let (next, follow) = apply_event(
            dropped,
            BackendEvent::Connected { account: "me".into() },
        );
        assert!(matches!(next, Screen::Picker { .. }));
        assert_eq!(follow, vec![FollowUp::ListWorkspaces]);
    }

    #[test]
    fn badge_priority() {
        assert_eq!(badge(&ws("w", false, true, true)), "offline");
        assert_eq!(badge(&ws("w", true, false, true)), "takeover");
        assert_eq!(badge(&ws("w", true, true, false)), "reattach");
        assert_eq!(badge(&ws("w", true, false, false)), "online");
    }
}
