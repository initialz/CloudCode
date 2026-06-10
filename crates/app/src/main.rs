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
mod state;
mod terminal;
// Browser-panel viewer ws client (P4 Task 2). The transport + channels
// land here; the panel that drives them is Task 3, so the public surface
// is unused until then — silence dead-code until it's wired in.
#[allow(dead_code)]
mod viewer;
mod wire;

use backend::{spawn, BackendEvent, BackendHandle, UiCommand};
use config::HubConfig;
use state::{apply_event, badge, FollowUp, Screen};
use std::path::PathBuf;
use terminal::{install_cjk_font, TerminalPanel};
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
    /// New-workspace form state on the picker.
    new_name: String,
    new_agent: String,
    /// The live terminal. `Some` only while a `Screen::Session` is open;
    /// created on `SessionOpened`, fed PTY bytes each frame, rendered by
    /// the session arm. Lives here (not in `Screen`) so the `state`
    /// reducer stays pure and unit-testable.
    terminal: Option<TerminalPanel>,
}

impl App {
    fn new(cc: &eframe::CreationContext<'_>, cfg: anyhow::Result<HubConfig>) -> App {
        // Register a system CJK font as a fallback so Chinese renders
        // instead of tofu. Runtime-loaded (not embedded) — see fonts.rs.
        install_cjk_font(&cc.egui_ctx);

        match cfg {
            Ok(cfg) => {
                let hub_url = cfg.hub_url.clone();
                // Hand the egui context to the backend so it can wake us
                // on incoming events from its own thread.
                let backend = spawn(cfg, cc.egui_ctx.clone());
                App {
                    screen: Screen::connecting(hub_url),
                    backend: Some(backend),
                    new_name: String::new(),
                    new_agent: String::new(),
                    terminal: None,
                }
            }
            Err(e) => App {
                screen: Screen::Error {
                    message: format!("config error: {e:#}"),
                },
                backend: None,
                new_name: String::new(),
                new_agent: String::new(),
                terminal: None,
            },
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

            // A new session: spin up a fresh terminal panel. 80×24 matches
            // the hardcoded OpenSession size (Task 5 makes it dynamic).
            if matches!(ev, BackendEvent::SessionOpened { .. }) {
                self.terminal = Some(TerminalPanel::new(80, 24));
            }

            let screen = std::mem::replace(
                &mut self.screen,
                Screen::Error {
                    message: String::new(),
                },
            );
            let (next, follow_ups) = apply_event(screen, ev);
            // Dropped out of the session view — release the terminal.
            if !matches!(next, Screen::Session { .. }) {
                self.terminal = None;
            }
            self.screen = next;
            for f in follow_ups {
                match f {
                    FollowUp::ListWorkspaces => self.send(UiCommand::ListWorkspaces),
                }
            }
        }
    }
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.drain_events();

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
                    output: _,
                } => {
                    // Live terminal panel (Task 3). The VTE state machine
                    // is fed PtyBytes in `drain_events`; here we just
                    // render its current grid.
                    ui.horizontal(|ui| {
                        ui.heading(format!("session: {workspace}@{agent}"));
                        ui.with_layout(
                            egui::Layout::right_to_left(egui::Align::Center),
                            |ui| ui.weak(format!("cwd: {cwd}")),
                        );
                    });
                    ui.separator();
                    // The terminal owns its own scrollback (alacritty grid +
                    // wheel scroll), so no egui ScrollArea — the panel sizes
                    // itself to the remaining region (pixels → cols/rows).
                    let avail = ui.available_size();
                    let output = if let Some(panel) = &mut self.terminal {
                        panel.ui(ui, avail, std::time::Instant::now())
                    } else {
                        ui.weak("(terminal not ready)");
                        terminal::UiOutput::default()
                    };
                    // Keyboard/IME/paste bytes from the panel -> hub PTY.
                    if !output.input.is_empty() {
                        self.send(UiCommand::SendInput(output.input));
                    }
                    // Debounced pixel→grid resize -> hub PTY resize.
                    if let Some(r) = output.resize {
                        self.send(UiCommand::Resize {
                            cols: r.cols,
                            rows: r.rows,
                        });
                    }
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
