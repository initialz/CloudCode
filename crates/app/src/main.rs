//! cloudcode-app — native egui desktop client.
//!
//! Architecture (see `backend.rs`): eframe owns the winit main thread;
//! all hub I/O runs on a tokio runtime in a separate std::thread. The
//! two halves talk over `UiCommand` / `BackendEvent` channels, and the
//! backend wakes the UI with `egui::Context::request_repaint()` on every
//! event.
//!
//! P6 layout (cmux-style): a persistent left sidebar (workspace list,
//! new/delete/reset, hub status) + a content area (the active session's
//! terminal/browser split, or a placeholder). The view state is the
//! [`state::AppModel`] folded by the pure `state::apply_event` reducer
//! (unit-tested in `state.rs`); all chrome colors come from `theme.rs`.

mod backend;
mod config;
mod session_view;
mod state;
mod terminal;
mod theme;
mod viewer;
mod wire;

use backend::{spawn, BackendEvent, BackendHandle, UiCommand};
use config::HubConfig;
use session_view::{
    reconcile_viewer_action, should_connect_viewer, split_width_range, SessionView, ViewerAction,
};
use state::{apply_event, row_badge, switch_decision, AppModel, Dot, FollowUp, Phase, SwitchDecision};
use std::path::PathBuf;
use terminal::{install_cjk_font, TerminalPanel};
use viewer::{BrowserPanel, ViewerCommand, ViewerEvent, ViewerHandle};
// `terminal::UiOutput` is referenced fully-qualified in the session arm.

fn main() -> eframe::Result {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .init();

    let args = Args::parse(std::env::args().skip(1));

    // Load config up front so a misconfigured client shows the fatal
    // screen rather than spinning forever on Connecting.
    let cfg_result = config::load_config(args.config.as_deref());

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default().with_inner_size([1100.0, 680.0]),
        ..Default::default()
    };

    eframe::run_native(
        "cloudcode",
        options,
        Box::new(move |cc| Ok(Box::new(App::new(cc, cfg_result)))),
    )
}

/// Minimal CLI: only `--config <path>` (no clap — the desktop app has
/// one knob and pulling clap in for it isn't worth it).
struct Args {
    config: Option<PathBuf>,
}

impl Args {
    fn parse(mut argv: impl Iterator<Item = String>) -> Args {
        let mut config = None;
        while let Some(a) = argv.next() {
            match a.as_str() {
                "--config" => config = argv.next().map(PathBuf::from),
                other if other.starts_with("--config=") => {
                    config = Some(PathBuf::from(&other["--config=".len()..]));
                }
                _ => {}
            }
        }
        Args { config }
    }
}

/// A destructive sidebar action awaiting the user's confirmation popup.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Confirm {
    Delete { name: String, agent: String },
    Reset { name: String, agent: String },
}

struct App {
    /// The reducer-owned view model (sidebar list, phase, active session).
    model: AppModel,
    /// `None` until config loads cleanly and the backend spawns.
    backend: Option<BackendHandle>,
    /// Hub base URL + account token from config — threaded into the App so
    /// the session content can open the *second* (viewer) ws lazily. The
    /// backend already has its own copy for the PTY ws.
    hub_url: String,
    token: String,
    /// "+ new workspace" inline form state (sidebar).
    new_open: bool,
    new_name: String,
    new_agent: String,
    /// Pending destructive action (delete/reset) awaiting confirmation.
    confirm: Option<Confirm>,
    /// The live terminal. `Some` only while a session is open; created on
    /// `SessionOpened`, fed PTY bytes each frame. Lives here (not in the
    /// model) so the `state` reducer stays pure and unit-testable. NOT
    /// dropped on a disconnect — the grid stays up (dimmed) through the
    /// reconnect so the blip doesn't blank the screen.
    terminal: Option<TerminalPanel>,
    /// Which panel(s) the session content shows (terminal / split / browser).
    session_view: SessionView,
    /// The browser (screencast) panel — same lifetime as `terminal`.
    browser: Option<BrowserPanel>,
    /// The *second* ws to the hub (the viewer/screencast). Opened LAZILY:
    /// `None` until the browser panel is first shown, dropped (→ hub
    /// `ViewerDetach` → agent stops screencast) when it's hidden again.
    viewer: Option<ViewerHandle>,
    /// Latch: once the viewer ws drops (e.g. the agent has `browser.enabled=
    /// false` so the screencast never starts, or a network blip), don't keep
    /// auto-reconnecting it every frame while the panel stays visible — that
    /// would busy-loop the hub/agent. The client intentionally does NOT
    /// auto-reconnect (see `viewer/client.rs`); re-establishing is a deliberate
    /// user action (toggle the browser panel away and back, which clears this).
    /// Cleared when the browser panel is hidden or a new session opens.
    viewer_retry_blocked: bool,
    /// The terminal bell rang this frame (attention freshly set in
    /// `drain_events`): `update()` nudges the OS (dock bounce / taskbar
    /// flash) if the window is unfocused. One-shot, best-effort.
    attention_nudge: bool,
}

impl App {
    fn new(cc: &eframe::CreationContext<'_>, cfg: anyhow::Result<HubConfig>) -> App {
        // The ONE place the theme is installed — every panel and popup
        // rendered afterwards inherits it (cmux lesson: no split themes).
        theme::apply(&cc.egui_ctx);
        // Register a system CJK font as a fallback so Chinese renders
        // instead of tofu. Runtime-loaded (not embedded) — see fonts.rs.
        install_cjk_font(&cc.egui_ctx);

        let (model, backend, hub_url, token) = match cfg {
            Ok(cfg) => {
                let hub_url = cfg.hub_url.clone();
                let token = cfg.token.clone();
                // Hand the egui context to the backend so it can wake us
                // on incoming events from its own thread.
                let backend = spawn(cfg, cc.egui_ctx.clone());
                (
                    AppModel::new(hub_url.clone()),
                    Some(backend),
                    hub_url,
                    token,
                )
            }
            Err(e) => (
                AppModel::fatal(format!("config error: {e:#}")),
                None,
                String::new(),
                String::new(),
            ),
        };
        App {
            model,
            backend,
            hub_url,
            token,
            new_open: false,
            new_name: String::new(),
            new_agent: String::new(),
            confirm: None,
            terminal: None,
            session_view: SessionView::default(),
            browser: None,
            viewer: None,
            viewer_retry_blocked: false,
            attention_nudge: false,
        }
    }

    /// Drop the viewer ws (best-effort `Close` → hub `ViewerDetach` → agent
    /// stops the screencast), if one is open. Idempotent. Called when the
    /// browser panel is hidden, the session ends, or the app quits.
    fn disconnect_viewer(&mut self) {
        if let Some(handle) = self.viewer.take() {
            let _ = handle.cmd_tx.send(ViewerCommand::Close);
            // Dropping `handle` also drops the command sender; the client
            // thread sees the channel close and tears the ws down even if
            // the explicit Close above didn't make it.
        }
        if let Some(panel) = &mut self.browser {
            panel.mark_disconnected();
        }
    }

    /// Open the viewer ws for the current session, if not already open. Needs
    /// the session id (from `SessionOpened`); a no-op without one. Called when
    /// the browser panel is first shown.
    fn connect_viewer(&mut self, ctx: &egui::Context) {
        if self.viewer.is_some() {
            return; // already connected
        }
        let Some(active) = &self.model.active else {
            return; // not in a session — nothing to watch
        };
        if active.session_id.is_empty() {
            // No session id (older hub that didn't send one) — can't open the
            // viewer ws. Leave the panel on its placeholder.
            tracing::warn!("viewer: no session_id; browser panel unavailable");
            return;
        }
        self.viewer = Some(ViewerHandle::connect(
            self.hub_url.clone(),
            self.token.clone(),
            active.session_id.clone(),
            ctx.clone(),
        ));
    }

    /// Reconcile the viewer ws with the current view: connect when the
    /// browser panel is visible (and the wire is up), disconnect when it's
    /// hidden or we're mid-reconnect. Idempotent — called every frame; only
    /// acts on a change. See `session_view::reconcile_viewer_action` for the
    /// retry-latch contract.
    fn reconcile_viewer(&mut self, ctx: &egui::Context) {
        let want = self.model.phase == Phase::Ready
            && self.model.active.is_some()
            && should_connect_viewer(self.session_view);
        let (action, clear_block) =
            reconcile_viewer_action(want, self.viewer.is_some(), self.viewer_retry_blocked);
        if clear_block {
            self.viewer_retry_blocked = false;
        }
        match action {
            ViewerAction::Connect => self.connect_viewer(ctx),
            ViewerAction::Disconnect => self.disconnect_viewer(),
            ViewerAction::Idle => {}
        }
    }

    /// Drain queued viewer-ws events into the browser panel each frame:
    /// frames become textures, a disconnect flips the placeholder on.
    fn drain_viewer_events(&mut self, ctx: &egui::Context) {
        let events: Vec<ViewerEvent> = match &self.viewer {
            Some(h) => h.event_rx.try_iter().collect(),
            None => return,
        };
        let Some(panel) = &mut self.browser else {
            return;
        };
        for ev in events {
            match ev {
                ViewerEvent::Connected => panel.mark_connected(),
                ViewerEvent::Frame(jpeg) => panel.set_frame(ctx, &jpeg),
                // The agent's tab list (fresh on attach, then on every
                // change) — the panel re-runs its agent-mirroring
                // auto-select so the highlighted tab tracks the stream.
                ViewerEvent::Targets(targets) => panel.set_targets(targets),
                ViewerEvent::Disconnected => {
                    panel.mark_disconnected();
                    // The client thread has exited; drop our handle. Block an
                    // immediate reconnect (the panel is still visible) so a
                    // never-streaming agent doesn't busy-loop the viewer ws —
                    // re-establishing is a deliberate toggle (cleared when the
                    // panel is hidden). See `reconcile_viewer`.
                    self.viewer = None;
                    self.viewer_retry_blocked = true;
                    break;
                }
            }
        }
    }

    fn send(&self, cmd: UiCommand) {
        if let Some(b) = &self.backend {
            let _ = b.cmd_tx.send(cmd);
        }
    }

    /// Drain every queued backend event into the model, issuing any
    /// follow-up commands the reducer asks for.
    ///
    /// `PtyBytes` are intercepted here and fed straight into the live
    /// `TerminalPanel` (the VTE state machine), bypassing the reducer so
    /// `state::apply_event` stays pure. Panel lifecycle: (re)created on
    /// `SessionOpened`, torn down when the model has no active session
    /// while the wire is up. A `Disconnected` deliberately KEEPS the
    /// panels (the reducer retains `model.active`) so the terminal grid
    /// stays on screen, dimmed, through the reconnect.
    fn drain_events(&mut self) {
        let events: Vec<_> = match &self.backend {
            Some(b) => b.event_rx.try_iter().collect(),
            None => return,
        };
        for ev in events {
            // PTY bytes drive the terminal directly; don't run them
            // through the (pure) reducer.
            if let BackendEvent::PtyBytes(bytes) = &ev {
                if let Some(panel) = &mut self.terminal {
                    let had_attention = panel.attention();
                    panel.feed(bytes);
                    // A FRESH bell (off → on) also nudges the OS window
                    // if we're unfocused — handled in `update()`.
                    if !had_attention && panel.attention() {
                        self.attention_nudge = true;
                    }
                }
                continue;
            }

            // A new session: spin up a fresh terminal + browser panel and
            // reset the view to the default split. 80×24 matches the
            // hardcoded OpenSession size. The viewer ws stays closed until
            // the browser panel is first shown (lazy connect — see
            // `reconcile_viewer`).
            if matches!(ev, BackendEvent::SessionOpened { .. }) {
                self.terminal = Some(TerminalPanel::new(80, 24));
                self.browser = Some(BrowserPanel::new());
                self.session_view = SessionView::default();
                // A stale viewer from a previous session must not survive, and
                // a fresh session starts with retry unblocked.
                self.disconnect_viewer();
                self.viewer_retry_blocked = false;
            }

            let follow_ups = apply_event(&mut self.model, ev);

            // No active session in the model AND the wire is up → the
            // session genuinely ended (closed / switch / failed reopen):
            // release the terminal, the browser panel, and the viewer ws
            // (→ agent stops screencast). While RECONNECTING the model
            // keeps `active`, so the panels survive the blip dimmed.
            if self.model.active.is_none() && self.terminal.is_some() {
                self.terminal = None;
                self.browser = None;
                self.disconnect_viewer();
            }

            for f in follow_ups {
                match f {
                    FollowUp::ListWorkspaces => self.send(UiCommand::ListWorkspaces),
                    FollowUp::ListAgents => self.send(UiCommand::ListAgents),
                    // The auto-reattach hero flow / switch landing: reopen
                    // the remembered workspace on the fresh connection.
                    FollowUp::OpenSession { agent, workspace } => {
                        self.send(UiCommand::OpenSession { agent, workspace })
                    }
                }
            }
        }
    }

    /// Sidebar row click → the pure switch decision → commands. Open when
    /// idle; cycle the connection when a session is live (see
    /// `state::SwitchDecision::SwitchViaReconnect` for why).
    fn click_workspace(&mut self, agent: String, workspace: String) {
        match switch_decision(&self.model, &agent, &workspace) {
            SwitchDecision::Ignore => {}
            SwitchDecision::Open => {
                self.send(UiCommand::OpenSession { agent, workspace });
            }
            SwitchDecision::SwitchViaReconnect => {
                self.model.begin_switch(agent, workspace);
                self.send(UiCommand::SwitchWorkspace);
            }
        }
    }

    // -----------------------------------------------------------------
    // Sidebar
    // -----------------------------------------------------------------

    fn render_sidebar(&mut self, ui: &mut egui::Ui) {
        let reconnecting = matches!(self.model.phase, Phase::Connecting { reconnecting: true });

        // --- Bottom status cell first (bottom-up), so the scroll list can
        // take the remaining height. ---
        ui.with_layout(egui::Layout::bottom_up(egui::Align::Min), |ui| {
            ui.add_space(theme::SP_2);
            ui.horizontal(|ui| {
                if reconnecting {
                    ui.colored_label(theme::WARN, "●");
                    ui.colored_label(theme::WARN, "reconnecting…");
                } else {
                    ui.colored_label(theme::OK, "●");
                    ui.label(
                        egui::RichText::new(if self.model.account.is_empty() {
                            "connected".to_string()
                        } else {
                            self.model.account.clone()
                        })
                        .color(theme::TEXT_MUTED),
                    );
                }
            });
            ui.separator();

            // Everything above the status cell, top-down again.
            ui.with_layout(egui::Layout::top_down(egui::Align::Min), |ui| {
                self.render_sidebar_main(ui);
            });
        });
    }

    fn render_sidebar_main(&mut self, ui: &mut egui::Ui) {
        ui.add_space(theme::SP_2);
        ui.horizontal(|ui| {
            ui.label(
                egui::RichText::new("CloudCode")
                    .strong()
                    .size(16.0)
                    .color(theme::TEXT),
            );
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui
                    .small_button("⟳")
                    .on_hover_text("refresh workspaces")
                    .clicked()
                {
                    self.send(UiCommand::ListWorkspaces);
                    self.send(UiCommand::ListAgents);
                }
            });
        });
        ui.add_space(theme::SP_1);
        ui.separator();
        ui.add_space(theme::SP_1);

        // Workspace rows. Clone the list so the loop can mutate `self`
        // (send commands) without borrow conflicts.
        let workspaces = self.model.workspaces.clone();
        let active_key = self
            .model
            .active
            .as_ref()
            .map(|a| (a.agent.clone(), a.workspace.clone()));
        // Bell-driven attention on the live terminal — `row_badge` gates
        // it onto the ACTIVE row only (V1: the bell is only detectable in
        // the attached session's PTY stream).
        let attention = self.terminal.as_ref().is_some_and(|t| t.attention());
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                if workspaces.is_empty() {
                    ui.label(
                        egui::RichText::new("(no workspaces yet)")
                            .color(theme::TEXT_FAINT)
                            .small(),
                    );
                }
                for w in &workspaces {
                    let is_active = active_key
                        .as_ref()
                        .is_some_and(|(a, n)| *a == w.agent && *n == w.name);
                    match workspace_row(ui, w, is_active, attention) {
                        RowAction::None => {}
                        RowAction::Clicked => {
                            self.click_workspace(w.agent.clone(), w.name.clone())
                        }
                        RowAction::Delete => {
                            self.confirm = Some(Confirm::Delete {
                                name: w.name.clone(),
                                agent: w.agent.clone(),
                            })
                        }
                        RowAction::Reset => {
                            self.confirm = Some(Confirm::Reset {
                                name: w.name.clone(),
                                agent: w.agent.clone(),
                            })
                        }
                    }
                }

                ui.add_space(theme::SP_2);
                self.render_new_workspace(ui);
            });
    }

    /// The "+ new workspace" row: a button that expands inline into a
    /// name input + agent dropdown (agents from ListAgents).
    fn render_new_workspace(&mut self, ui: &mut egui::Ui) {
        if !self.new_open {
            if ui
                .add(
                    egui::Button::new(
                        egui::RichText::new("+ new workspace").color(theme::TEXT_MUTED),
                    )
                    .frame(false),
                )
                .clicked()
            {
                self.new_open = true;
                // Refresh the dropdown's source when the form opens.
                self.send(UiCommand::ListAgents);
            }
            return;
        }

        egui::Frame::none()
            .fill(theme::BG2)
            .rounding(theme::RADIUS)
            .inner_margin(theme::SP_2)
            .show(ui, |ui| {
                ui.add(
                    egui::TextEdit::singleline(&mut self.new_name)
                        .hint_text("workspace name")
                        .desired_width(f32::INFINITY),
                );
                // Default the dropdown to the first known agent.
                if self.new_agent.is_empty() {
                    if let Some(a) = self.model.agents.first() {
                        self.new_agent = a.name.clone();
                    }
                }
                egui::ComboBox::from_id_source("new_ws_agent")
                    .width(ui.available_width())
                    .selected_text(if self.new_agent.is_empty() {
                        "(no agents online)".to_string()
                    } else {
                        self.new_agent.clone()
                    })
                    .show_ui(ui, |ui| {
                        for a in &self.model.agents {
                            ui.selectable_value(&mut self.new_agent, a.name.clone(), &a.name);
                        }
                    });
                ui.add_space(theme::SP_1);
                ui.horizontal(|ui| {
                    let ready = !self.new_name.trim().is_empty()
                        && !self.new_agent.trim().is_empty();
                    if ui.add_enabled(ready, egui::Button::new("Create")).clicked() {
                        self.send(UiCommand::CreateWorkspace {
                            name: self.new_name.trim().to_string(),
                            agent: self.new_agent.trim().to_string(),
                        });
                        self.new_name.clear();
                        self.new_open = false;
                    }
                    if ui.button("Cancel").clicked() {
                        self.new_open = false;
                        self.new_name.clear();
                    }
                });
            });
    }

    /// The delete/reset confirmation popup (simple centered window).
    fn render_confirm(&mut self, ctx: &egui::Context) {
        let Some(confirm) = self.confirm.clone() else {
            return;
        };
        let (title, body) = match &confirm {
            Confirm::Delete { name, agent } => (
                "Delete workspace?",
                format!("Delete '{name}' on agent '{agent}'?\nThis removes its directory on the agent."),
            ),
            Confirm::Reset { name, agent } => (
                "Reset workspace?",
                format!("Reset '{name}' on agent '{agent}'?\nThis kills its tmux session and wipes claude history."),
            ),
        };
        let mut open = true;
        egui::Window::new(title)
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO)
            .show(ctx, |ui| {
                ui.label(body);
                ui.add_space(theme::SP_2);
                ui.horizontal(|ui| {
                    if ui
                        .button(egui::RichText::new("Confirm").color(theme::ERR))
                        .clicked()
                    {
                        match confirm.clone() {
                            Confirm::Delete { name, agent } => {
                                self.send(UiCommand::DeleteWorkspace { name, agent })
                            }
                            Confirm::Reset { name, agent } => {
                                self.send(UiCommand::ResetWorkspace { name, agent })
                            }
                        }
                        self.confirm = None;
                    }
                    if ui.button("Cancel").clicked() {
                        self.confirm = None;
                    }
                });
            });
        if !open {
            self.confirm = None;
        }
    }

    // -----------------------------------------------------------------
    // Content area
    // -----------------------------------------------------------------

    fn render_content(&mut self, ui: &mut egui::Ui) {
        let reconnecting = matches!(self.model.phase, Phase::Connecting { reconnecting: true });

        // Reconnect banner: an orange strip across the top of the content
        // area (replaces P3's full-screen reconnecting page — the model,
        // sidebar and panels all stay up).
        if reconnecting {
            egui::Frame::none()
                .fill(theme::WARN.linear_multiply(0.22))
                .inner_margin(egui::Margin::symmetric(theme::SP_3, theme::SP_1 + 2.0))
                .show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    ui.horizontal(|ui| {
                        ui.add(egui::Spinner::new().size(12.0).color(theme::WARN));
                        let host = if self.model.hub_url.is_empty() {
                            "reconnecting…".to_string()
                        } else {
                            format!("reconnecting to {}…", self.model.hub_url)
                        };
                        ui.colored_label(theme::WARN, host);
                        ui.label(
                            egui::RichText::new("· your session keeps running on the agent")
                                .color(theme::TEXT_MUTED)
                                .small(),
                        );
                    });
                });
        }

        // Transient error toast (failed open/create/reopen).
        if let Some(err) = self.model.error.clone() {
            egui::Frame::none()
                .fill(theme::ERR.linear_multiply(0.18))
                .inner_margin(egui::Margin::symmetric(theme::SP_3, theme::SP_1 + 2.0))
                .show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    ui.horizontal(|ui| {
                        ui.colored_label(theme::ERR, err);
                        ui.with_layout(
                            egui::Layout::right_to_left(egui::Align::Center),
                            |ui| {
                                if ui.small_button("✕").clicked() {
                                    self.model.error = None;
                                }
                            },
                        );
                    });
                });
        }

        if self.model.active.is_some() {
            // Remember the rect so the reconnect dim-overlay can cover
            // exactly the session content.
            let content_rect = ui.available_rect_before_wrap();
            // While reconnecting: panels stay visible but input is dead
            // (disabled UI → no focus → terminal/browser capture nothing).
            ui.add_enabled_ui(!reconnecting, |ui| {
                self.render_session(ui);
            });
            if reconnecting {
                ui.painter()
                    .rect_filled(content_rect, 0.0, theme::DIM_OVERLAY);
            }
        } else {
            ui.centered_and_justified(|ui| {
                ui.label(
                    egui::RichText::new("select a workspace to attach")
                        .color(theme::TEXT_FAINT)
                        .size(15.0),
                );
            });
        }
    }

    /// Render the active session: a top bar (ws@agent · cwd · view toggle),
    /// then the split (terminal | browser) layout, single-panel when
    /// collapsed. Terminal input/resize go to the PTY ws; browser input goes
    /// to the viewer ws.
    ///
    /// Focus routing is egui's native single-focus: each panel calls
    /// `request_focus()` on click on its own `allocate_painter` response
    /// (distinct ids — the terminal's and the browser's regions never share
    /// one), and both gate keyboard/IME capture on `has_focus()`, so typing
    /// only ever reaches the clicked panel.
    fn render_session(&mut self, ui: &mut egui::Ui) {
        let Some(active) = self.model.active.clone() else {
            return;
        };
        // --- Top bar ---
        // A live-screencast dot: lit only when the browser is shown AND a
        // frame has actually arrived (lazy-connect is up and streaming).
        let browser_live = self.session_view.shows_browser()
            && self.browser.as_ref().is_some_and(|p| p.has_frame());
        ui.add_space(theme::SP_1);
        ui.horizontal(|ui| {
            ui.add_space(theme::SP_2);
            ui.label(
                egui::RichText::new(format!("{}@{}", active.workspace, active.agent))
                    .strong()
                    .color(theme::TEXT),
            );
            ui.label(
                egui::RichText::new(active.cwd.as_str())
                    .color(theme::TEXT_MUTED)
                    .small(),
            );
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.add_space(theme::SP_2);
                // Three-state toggle. `selectable_value` shows the current
                // mode pressed. (Cmd/Ctrl+B cycles the same states.)
                ui.selectable_value(&mut self.session_view, SessionView::BrowserOnly, "Browser");
                ui.selectable_value(&mut self.session_view, SessionView::Split, "Split");
                ui.selectable_value(
                    &mut self.session_view,
                    SessionView::TerminalOnly,
                    "Terminal",
                );
                if browser_live {
                    ui.add_space(theme::SP_2);
                    ui.colored_label(theme::OK, "● live");
                }
            });
        });
        ui.add_space(theme::SP_1);
        ui.separator();

        let view = self.session_view;
        match view {
            SessionView::TerminalOnly => {
                let avail = ui.available_size();
                self.render_terminal(ui, avail);
            }
            SessionView::BrowserOnly => {
                self.render_browser(ui);
            }
            SessionView::Split => {
                // Browser on the right in a resizable SidePanel (egui
                // persists its width by id), terminal fills the rest. The
                // separator drag is bounded to [20%, 80%] of the width so
                // neither panel can be collapsed away.
                let total_w = ui.available_width();
                let range = split_width_range(total_w);
                egui::SidePanel::right("browser_panel")
                    .resizable(true)
                    .default_width((total_w * 0.5).clamp(*range.start(), *range.end()))
                    .width_range(range)
                    .show_inside(ui, |ui| {
                        self.render_browser(ui);
                    });
                egui::CentralPanel::default().show_inside(ui, |ui| {
                    let avail = ui.available_size();
                    self.render_terminal(ui, avail);
                });
            }
        }
    }

    /// Draw the terminal panel and forward its captured input/resize to the
    /// PTY ws. Focus is the panel's own (click-to-focus, gated internally).
    ///
    /// When the panel's bell-driven attention flag is set, a 2px ACCENT
    /// halo is stroked around the panel's rect. Painted here inside the
    /// session content (BEFORE `render_content`'s reconnect dim overlay)
    /// so a reconnect dims the halo along with everything else.
    fn render_terminal(&mut self, ui: &mut egui::Ui, avail: egui::Vec2) {
        let panel_rect =
            egui::Rect::from_min_size(ui.available_rect_before_wrap().min, avail);
        let output = if let Some(panel) = &mut self.terminal {
            let out = panel.ui(ui, avail, std::time::Instant::now());
            if panel.attention() {
                // Inset by 1px so the 2px stroke isn't clipped at the
                // panel's edges.
                ui.painter().rect_stroke(
                    panel_rect.shrink(1.0),
                    theme::RADIUS,
                    egui::Stroke::new(2.0, theme::ACCENT),
                );
            }
            out
        } else {
            ui.colored_label(theme::TEXT_FAINT, "(terminal not ready)");
            terminal::UiOutput::default()
        };
        if !output.input.is_empty() {
            self.send(UiCommand::SendInput(output.input));
        }
        if let Some(r) = output.resize {
            self.send(UiCommand::Resize {
                cols: r.cols,
                rows: r.rows,
            });
        }
    }

    /// Draw the browser panel (tab bar + screencast) and forward what it
    /// captured up the viewer ws: mouse/key/IME events as `SendInput`, a
    /// tab click as `SelectTarget`. With no viewer handle (lazy connect
    /// pending / a drop) the panel shows its placeholder and produces
    /// nothing to forward.
    fn render_browser(&mut self, ui: &mut egui::Ui) {
        let output = match &mut self.browser {
            Some(panel) => panel.ui(ui),
            None => {
                ui.colored_label(theme::TEXT_FAINT, "(browser not ready)");
                return;
            }
        };
        if let Some(handle) = &self.viewer {
            for ev in output.input {
                let _ = handle.cmd_tx.send(ViewerCommand::SendInput(ev));
            }
            if let Some(target_id) = output.select {
                let _ = handle.cmd_tx.send(ViewerCommand::SelectTarget(target_id));
            }
        }
    }
}

/// What the user did to a sidebar workspace row this frame.
enum RowAction {
    None,
    Clicked,
    Delete,
    Reset,
}

/// Paint one sidebar workspace row: status dot, name, `@agent`, the
/// "attached elsewhere" hint, the bell-driven attention dot, active-row
/// highlight (BG2 + accent left bar) and hover-revealed delete/reset
/// icons. Pure-ish (egui only) — the badge derivation it leans on
/// (`state::row_badge`) is the table-tested part.
fn workspace_row(
    ui: &mut egui::Ui,
    w: &cloudcode_proto::WorkspaceInfo,
    is_active: bool,
    attention: bool,
) -> RowAction {
    let badge = row_badge(w, is_active, attention);
    let row_h = 40.0;
    let (rect, response) = ui.allocate_exact_size(
        egui::vec2(ui.available_width(), row_h),
        if badge.clickable {
            egui::Sense::click()
        } else {
            egui::Sense::hover()
        },
    );
    let hovered = response.hovered();
    let painter = ui.painter();

    // Background: active row = raised card + accent bar; hover = subtle.
    if is_active {
        painter.rect_filled(rect, theme::RADIUS, theme::BG2);
        let bar = egui::Rect::from_min_size(rect.min, egui::vec2(3.0, rect.height()));
        painter.rect_filled(bar, 1.5, theme::ACCENT);
    } else if hovered {
        painter.rect_filled(rect, theme::RADIUS, theme::BG2.linear_multiply(0.6));
    }

    // Status dot.
    let dot_color = match badge.dot {
        Dot::Running => theme::OK,
        Dot::Saved => theme::TEXT_FAINT,
        Dot::Offline => theme::BORDER,
    };
    let dot_center = egui::pos2(rect.min.x + 14.0, rect.center().y);
    painter.circle_filled(dot_center, 4.0, dot_color);

    // Attention dot (bell rang, user hasn't typed back): ACCENT, at the
    // row's right edge. Hidden while hovered — the delete/reset icons
    // occupy that corner, and a hovering user is about to act anyway.
    if badge.attention && !hovered {
        let attn_center = egui::pos2(rect.max.x - 14.0, rect.center().y);
        painter.circle_filled(attn_center, 4.0, theme::ACCENT);
    }

    // Name + @agent, dimmed when the agent is offline.
    let name_color = if badge.dot == Dot::Offline {
        theme::TEXT_FAINT
    } else {
        theme::TEXT
    };
    let text_x = rect.min.x + 26.0;
    painter.text(
        egui::pos2(text_x, rect.center().y - 9.0),
        egui::Align2::LEFT_CENTER,
        &w.name,
        egui::FontId::proportional(13.0),
        name_color,
    );
    let sub = if badge.attached_elsewhere {
        format!("@{} · attached elsewhere", w.agent)
    } else {
        format!("@{}", w.agent)
    };
    painter.text(
        egui::pos2(text_x, rect.center().y + 9.0),
        egui::Align2::LEFT_CENTER,
        sub,
        egui::FontId::proportional(11.0),
        if badge.attached_elsewhere {
            theme::WARN
        } else {
            theme::TEXT_MUTED
        },
    );

    // Hover actions: small delete/reset icon buttons on the right. Placed
    // after (on top of) the row's click region, so egui routes their
    // clicks to the buttons, not the row.
    let mut action = RowAction::None;
    if hovered {
        let icons_rect = egui::Rect::from_min_max(
            egui::pos2(rect.max.x - 52.0, rect.min.y),
            rect.max,
        );
        let mut icons_ui = ui.child_ui(
            icons_rect,
            egui::Layout::right_to_left(egui::Align::Center),
            None,
        );
        if icons_ui
            .small_button("🗑")
            .on_hover_text("delete workspace")
            .clicked()
        {
            action = RowAction::Delete;
        }
        if icons_ui
            .small_button("↺")
            .on_hover_text("reset workspace (kill tmux + history)")
            .clicked()
        {
            action = RowAction::Reset;
        }
    }
    if matches!(action, RowAction::None) && response.clicked() {
        action = RowAction::Clicked;
    }
    ui.add_space(2.0);
    action
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.drain_events();

        // A fresh bell while the OS window is unfocused → best-effort
        // attention request (dock bounce on macOS, taskbar flash
        // elsewhere). One-shot per bell; focused windows skip it (the
        // in-app halo is enough).
        if std::mem::take(&mut self.attention_nudge) && !ctx.input(|i| i.focused) {
            ctx.send_viewport_cmd(egui::ViewportCommand::RequestUserAttention(
                egui::UserAttentionType::Informational,
            ));
        }

        // Cmd/Ctrl+B cycles the session view (Terminal → Split → Browser).
        // Only meaningful with an open session; harmless elsewhere. Consume
        // the shortcut so it doesn't also reach the terminal as a Ctrl-B byte.
        if self.model.active.is_some()
            && ctx.input_mut(|i| {
                i.consume_shortcut(&egui::KeyboardShortcut::new(
                    egui::Modifiers::COMMAND,
                    egui::Key::B,
                ))
            })
        {
            self.session_view = self.session_view.cycle();
        }

        // Lazy viewer-ws lifecycle: open the second ws when the browser panel
        // is visible, drop it (→ agent stops screencast) when hidden. Then
        // pump any frames/disconnects into the panel.
        self.reconcile_viewer(ctx);
        self.drain_viewer_events(ctx);

        // --- Fatal: full-window error, nothing else to show. ---
        if let Some(message) = self.model.fatal.clone() {
            egui::CentralPanel::default().show(ctx, |ui| {
                ui.vertical_centered(|ui| {
                    ui.add_space(ui.available_height() * 0.4);
                    ui.colored_label(theme::ERR, "Error");
                    ui.label(message);
                    ui.add_space(theme::SP_3);
                    if ui.button("Quit").clicked() {
                        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                    }
                });
            });
            return;
        }

        // --- Initial connect: centered spinner (no sidebar yet — there's
        // nothing to list before the first Welcome). A RECONNECT renders
        // the normal layout instead: sidebar + banner + dimmed panels. ---
        if self.model.phase
            == (Phase::Connecting {
                reconnecting: false,
            })
        {
            egui::CentralPanel::default().show(ctx, |ui| {
                ui.vertical_centered(|ui| {
                    ui.add_space(ui.available_height() * 0.4);
                    ui.add(egui::Spinner::new().size(32.0));
                    ui.add_space(theme::SP_3);
                    ui.colored_label(
                        theme::TEXT_MUTED,
                        format!("connecting to {}", self.model.hub_url),
                    );
                });
            });
            return;
        }

        // --- Persistent layout: sidebar + content. ---
        egui::SidePanel::left("sidebar")
            .resizable(true)
            .default_width(theme::SIDEBAR_W)
            .min_width(160.0)
            .frame(
                egui::Frame::none()
                    .fill(theme::BG1)
                    .inner_margin(egui::Margin::symmetric(theme::SP_2, theme::SP_1)),
            )
            .show(ctx, |ui| {
                self.render_sidebar(ui);
            });

        egui::CentralPanel::default()
            .frame(egui::Frame::none().fill(theme::BG0))
            .show(ctx, |ui| {
                self.render_content(ui);
            });

        self.render_confirm(ctx);
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        // Tear down both ws: the PTY (via the backend) and the viewer
        // (→ hub ViewerDetach → agent stops screencast).
        self.disconnect_viewer();
        self.send(UiCommand::Close);
    }
}

#[cfg(test)]
mod tests {
    use super::Args;

    #[test]
    fn parses_config_flag_space_form() {
        let args = Args::parse(
            ["--config", "/tmp/c.toml"]
                .into_iter()
                .map(String::from),
        );
        assert_eq!(args.config.unwrap().to_str().unwrap(), "/tmp/c.toml");
    }

    #[test]
    fn parses_config_flag_eq_form() {
        let args = Args::parse(["--config=/x/y.toml"].into_iter().map(String::from));
        assert_eq!(args.config.unwrap().to_str().unwrap(), "/x/y.toml");
    }

    #[test]
    fn no_config_flag_is_none() {
        let args = Args::parse(["--other"].into_iter().map(String::from));
        assert!(args.config.is_none());
    }
}
