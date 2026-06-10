//! cloudcode-app — native egui desktop client (P3).
//!
//! Architecture (see `backend.rs`): eframe owns the winit main thread;
//! all hub I/O runs on a tokio runtime in a separate std::thread. The
//! two halves talk over `UiCommand` / `BackendEvent` channels, and the
//! backend wakes the UI with `egui::Context::request_repaint()` on every
//! event. The view is a `Screen` state machine folded by the pure
//! `state::apply_event` reducer (unit-tested in `state.rs`).

mod backend;
mod config;
mod session_view;
mod state;
mod terminal;
mod viewer;
mod wire;

use backend::{spawn, BackendEvent, BackendHandle, UiCommand};
use config::HubConfig;
use session_view::{
    reconcile_viewer_action, should_connect_viewer, split_width_range, SessionView, ViewerAction,
};
use state::{apply_event, badge, FollowUp, Screen};
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

    // Load config up front so a misconfigured client shows the Error
    // screen rather than spinning forever on Connecting.
    let cfg_result = config::load_config(args.config.as_deref());

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default().with_inner_size([900.0, 640.0]),
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

struct App {
    screen: Screen,
    /// `None` until config loads cleanly and the backend spawns.
    backend: Option<BackendHandle>,
    /// Hub base URL + account token from config — threaded into the App so
    /// the Session screen can open the *second* (viewer) ws lazily. The
    /// backend already has its own copy for the PTY ws.
    hub_url: String,
    token: String,
    /// New-workspace form state on the picker.
    new_name: String,
    new_agent: String,
    /// The live terminal. `Some` only while a `Screen::Session` is open;
    /// created on `SessionOpened`, fed PTY bytes each frame, rendered by
    /// the session arm. Lives here (not in `Screen`) so the `state`
    /// reducer stays pure and unit-testable.
    terminal: Option<TerminalPanel>,
    /// Which panel(s) the Session screen shows (terminal / split / browser).
    /// Persisted across frames; reset to the default on each new session.
    session_view: SessionView,
    /// The browser (screencast) panel — decodes JPEG frames to a texture and
    /// captures mouse/key/IME. Created on `SessionOpened`, torn down when we
    /// leave the session (alongside `terminal`).
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
}

impl App {
    fn new(cc: &eframe::CreationContext<'_>, cfg: anyhow::Result<HubConfig>) -> App {
        // Register a system CJK font as a fallback so Chinese renders
        // instead of tofu. Runtime-loaded (not embedded) — see fonts.rs.
        install_cjk_font(&cc.egui_ctx);

        match cfg {
            Ok(cfg) => {
                let hub_url = cfg.hub_url.clone();
                let token = cfg.token.clone();
                // Hand the egui context to the backend so it can wake us
                // on incoming events from its own thread.
                let backend = spawn(cfg, cc.egui_ctx.clone());
                App {
                    screen: Screen::connecting(hub_url.clone()),
                    backend: Some(backend),
                    hub_url,
                    token,
                    new_name: String::new(),
                    new_agent: String::new(),
                    terminal: None,
                    session_view: SessionView::default(),
                    browser: None,
                    viewer: None,
                    viewer_retry_blocked: false,
                }
            }
            Err(e) => App {
                screen: Screen::Error {
                    message: format!("config error: {e:#}"),
                },
                backend: None,
                hub_url: String::new(),
                token: String::new(),
                new_name: String::new(),
                new_agent: String::new(),
                terminal: None,
                session_view: SessionView::default(),
                browser: None,
                viewer: None,
                viewer_retry_blocked: false,
            },
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
        let Screen::Session { session_id, .. } = &self.screen else {
            return; // not in a session — nothing to watch
        };
        if session_id.is_empty() {
            // No session id (older hub that didn't send one) — can't open the
            // viewer ws. Leave the panel on its placeholder.
            tracing::warn!("viewer: no session_id; browser panel unavailable");
            return;
        }
        self.viewer = Some(ViewerHandle::connect(
            self.hub_url.clone(),
            self.token.clone(),
            session_id.clone(),
            ctx.clone(),
        ));
    }

    /// Reconcile the viewer ws with the current view: connect when the
    /// browser panel is visible, disconnect when it's hidden. Idempotent —
    /// called every frame; only acts on a change.
    ///
    /// After a drop we set `viewer_retry_blocked` so we don't reconnect on the
    /// very next frame (the panel is still visible). The block is cleared the
    /// moment the panel is hidden, so toggling the browser away and back is the
    /// deliberate "reconnect" gesture (matching the client's no-auto-reconnect
    /// contract; otherwise a `browser.enabled=false` agent would busy-loop the
    /// viewer ws every frame).
    fn reconcile_viewer(&mut self, ctx: &egui::Context) {
        let want = matches!(self.screen, Screen::Session { .. })
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

    /// Drain every queued backend event into the screen state, issuing
    /// any follow-up commands the reducer asks for.
    ///
    /// `PtyBytes` are intercepted here and fed straight into the live
    /// `TerminalPanel` (the VTE state machine), bypassing the reducer so
    /// `state::apply_event` stays pure. The panel is (re)created on
    /// `SessionOpened` and torn down whenever we leave the session.
    fn drain_events(&mut self) {
        let events: Vec<_> = match &self.backend {
            Some(b) => b.event_rx.try_iter().collect(),
            None => return,
        };
        for ev in events {
            // PTY bytes drive the terminal directly; don't run them
            // through the (pure) screen reducer.
            if let BackendEvent::PtyBytes(bytes) = &ev {
                if let Some(panel) = &mut self.terminal {
                    panel.feed(bytes);
                }
                continue;
            }

            // A new session: spin up a fresh terminal + browser panel and
            // reset the view to the default split. 80×24 matches the
            // hardcoded OpenSession size (Task 5 makes it dynamic). The
            // viewer ws stays closed until the browser panel is first shown
            // (lazy connect — see `reconcile_viewer`).
            if matches!(ev, BackendEvent::SessionOpened { .. }) {
                self.terminal = Some(TerminalPanel::new(80, 24));
                self.browser = Some(BrowserPanel::new());
                self.session_view = SessionView::default();
                // A stale viewer from a previous session must not survive, and
                // a fresh session starts with retry unblocked.
                self.disconnect_viewer();
                self.viewer_retry_blocked = false;
            }

            let screen = std::mem::replace(
                &mut self.screen,
                Screen::Error {
                    message: String::new(),
                },
            );
            let (next, follow_ups) = apply_event(screen, ev);
            // Dropped out of the session view — release the terminal, the
            // browser panel, and the viewer ws (→ agent stops screencast).
            if !matches!(next, Screen::Session { .. }) {
                self.terminal = None;
                self.browser = None;
                self.disconnect_viewer();
            }
            self.screen = next;
            for f in follow_ups {
                match f {
                    FollowUp::ListWorkspaces => self.send(UiCommand::ListWorkspaces),
                }
            }
        }
    }

    /// Render the Session screen: a header + view toolbar, then the split
    /// (terminal | browser) layout, single-panel when collapsed. Terminal
    /// input/resize go to the PTY ws; browser input goes to the viewer ws.
    ///
    /// Focus routing is egui's native single-focus: each panel calls
    /// `request_focus()` on click on its own `allocate_painter` response
    /// (distinct ids — the terminal's and the browser's regions never share
    /// one), and both gate keyboard/IME capture on `has_focus()`, so typing
    /// only ever reaches the clicked panel.
    fn render_session(
        &mut self,
        ui: &mut egui::Ui,
        agent: &str,
        workspace: &str,
        cwd: &str,
    ) {
        // --- Header + view toolbar ---
        // A live-screencast dot: lit only when the browser is shown AND a
        // frame has actually arrived (lazy-connect is up and streaming).
        let browser_live = self.session_view.shows_browser()
            && self.browser.as_ref().is_some_and(|p| p.has_frame());
        ui.horizontal(|ui| {
            ui.heading(format!("session: {workspace}@{agent}"));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.weak(format!("cwd: {cwd}"));
                ui.add_space(12.0);
                if browser_live {
                    ui.colored_label(egui::Color32::from_rgb(80, 200, 120), "● live");
                    ui.add_space(8.0);
                }
                // Three-state toggle. `selectable_value` shows the current
                // mode pressed. (Cmd/Ctrl+B cycles the same states.)
                ui.selectable_value(
                    &mut self.session_view,
                    SessionView::BrowserOnly,
                    "Browser",
                );
                ui.selectable_value(&mut self.session_view, SessionView::Split, "Split");
                ui.selectable_value(
                    &mut self.session_view,
                    SessionView::TerminalOnly,
                    "Terminal",
                );
            });
        });
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
    fn render_terminal(&mut self, ui: &mut egui::Ui, avail: egui::Vec2) {
        let output = if let Some(panel) = &mut self.terminal {
            panel.ui(ui, avail, std::time::Instant::now())
        } else {
            ui.weak("(terminal not ready)");
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

    /// Draw the browser panel and forward its captured mouse/key/IME events
    /// up the viewer ws. With no viewer handle (lazy connect pending / a
    /// drop) the panel shows its placeholder and produces no events.
    fn render_browser(&mut self, ui: &mut egui::Ui) {
        let events = match &mut self.browser {
            Some(panel) => panel.ui(ui),
            None => {
                ui.weak("(browser not ready)");
                return;
            }
        };
        if let Some(handle) = &self.viewer {
            for ev in events {
                let _ = handle.cmd_tx.send(ViewerCommand::SendInput(ev));
            }
        }
    }
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.drain_events();

        // Cmd/Ctrl+B cycles the Session view (Terminal → Split → Browser).
        // Only meaningful in a session; harmless elsewhere. Consume the
        // shortcut so it doesn't also reach the terminal as a Ctrl-B byte.
        if matches!(self.screen, Screen::Session { .. })
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

        egui::CentralPanel::default().show(ctx, |ui| {
            // Clone the screen out for rendering so we don't hold a
            // borrow of `self` while mutating it (sending commands).
            let screen = self.screen.clone();
            match screen {
                Screen::Connecting {
                    hub_url,
                    reconnecting,
                } => {
                    ui.vertical_centered(|ui| {
                        ui.add_space(ui.available_height() * 0.4);
                        ui.add(egui::Spinner::new().size(32.0));
                        ui.add_space(12.0);
                        // A mid-session drop reads as "reconnecting…"
                        // (the backend is in its backoff loop); a fresh
                        // launch reads as "connecting to <host>".
                        if reconnecting {
                            let host = if hub_url.is_empty() {
                                "reconnecting…".to_string()
                            } else {
                                format!("reconnecting to {hub_url}…")
                            };
                            ui.colored_label(egui::Color32::from_rgb(220, 170, 60), host);
                            ui.add_space(4.0);
                            ui.weak("the session will return to the workspace picker");
                        } else {
                            ui.label(format!("connecting to {hub_url}"));
                        }
                    });
                }
                Screen::Picker {
                    account,
                    workspaces,
                    error,
                } => {
                    ui.horizontal(|ui| {
                        ui.heading("Workspaces");
                        ui.with_layout(
                            egui::Layout::right_to_left(egui::Align::Center),
                            |ui| {
                                ui.weak(format!("account: {account}"));
                            },
                        );
                    });
                    if let Some(err) = &error {
                        ui.colored_label(egui::Color32::from_rgb(220, 80, 80), err);
                    }
                    ui.separator();

                    egui::ScrollArea::vertical()
                        .auto_shrink([false, false])
                        .max_height(ui.available_height() - 90.0)
                        .show(ui, |ui| {
                            if workspaces.is_empty() {
                                ui.weak("(no workspaces — create one below)");
                            }
                            for w in &workspaces {
                                ui.horizontal(|ui| {
                                    let label = format!("{}@{}", w.name, w.agent);
                                    ui.label(label);
                                    ui.label(
                                        egui::RichText::new(format!("[{}]", badge(w)))
                                            .weak()
                                            .small(),
                                    );
                                    let openable = w.agent_online;
                                    if ui
                                        .add_enabled(openable, egui::Button::new("Open"))
                                        .clicked()
                                    {
                                        self.send(UiCommand::OpenSession {
                                            agent: w.agent.clone(),
                                            workspace: w.name.clone(),
                                        });
                                    }
                                    if ui.button("Delete").clicked() {
                                        self.send(UiCommand::DeleteWorkspace {
                                            name: w.name.clone(),
                                            agent: w.agent.clone(),
                                        });
                                    }
                                });
                            }
                        });

                    ui.separator();
                    ui.horizontal(|ui| {
                        ui.label("New:");
                        ui.add(
                            egui::TextEdit::singleline(&mut self.new_name)
                                .hint_text("name")
                                .desired_width(140.0),
                        );
                        ui.add(
                            egui::TextEdit::singleline(&mut self.new_agent)
                                .hint_text("agent")
                                .desired_width(140.0),
                        );
                        let ready = !self.new_name.trim().is_empty()
                            && !self.new_agent.trim().is_empty();
                        if ui.add_enabled(ready, egui::Button::new("Create")).clicked() {
                            self.send(UiCommand::CreateWorkspace {
                                name: self.new_name.trim().to_string(),
                                agent: self.new_agent.trim().to_string(),
                            });
                            self.new_name.clear();
                            self.new_agent.clear();
                        }
                        if ui.button("Refresh").clicked() {
                            self.send(UiCommand::ListWorkspaces);
                        }
                    });
                }
                Screen::Session {
                    agent,
                    workspace,
                    cwd,
                    ..
                } => {
                    self.render_session(ui, &agent, &workspace, &cwd);
                }
                Screen::Error { message } => {
                    ui.vertical_centered(|ui| {
                        ui.add_space(ui.available_height() * 0.4);
                        ui.colored_label(
                            egui::Color32::from_rgb(220, 80, 80),
                            "Error",
                        );
                        ui.label(message);
                        ui.add_space(12.0);
                        if ui.button("Quit").clicked() {
                            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                        }
                    });
                }
            }
        });
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
