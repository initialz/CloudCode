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
mod wire;

use backend::{spawn, BackendHandle, UiCommand};
use config::HubConfig;
use state::{apply_event, badge, FollowUp, Screen};
use std::path::PathBuf;

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
}

impl App {
    fn new(cc: &eframe::CreationContext<'_>, cfg: anyhow::Result<HubConfig>) -> App {
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
                }
            }
            Err(e) => App {
                screen: Screen::Error {
                    message: format!("config error: {e:#}"),
                },
                backend: None,
                new_name: String::new(),
                new_agent: String::new(),
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
    fn drain_events(&mut self) {
        let events: Vec<_> = match &self.backend {
            Some(b) => b.event_rx.try_iter().collect(),
            None => return,
        };
        for ev in events {
            let screen = std::mem::replace(
                &mut self.screen,
                Screen::Error {
                    message: String::new(),
                },
            );
            let (next, follow_ups) = apply_event(screen, ev);
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
                Screen::Connecting { hub_url } => {
                    ui.vertical_centered(|ui| {
                        ui.add_space(ui.available_height() * 0.4);
                        ui.add(egui::Spinner::new().size(32.0));
                        ui.add_space(12.0);
                        ui.label(format!("connecting to {hub_url}"));
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
                    output,
                } => {
                    // PLACEHOLDER for Task 3's TerminalPanel. Shows the
                    // session identity + a scrolling lossy-utf8 dump of
                    // incoming PTY bytes so the skeleton visibly works.
                    // Task 3 swaps this whole block for a TerminalPanel
                    // widget that consumes the same PtyBytes events.
                    ui.heading(format!("session open: {workspace}@{agent}"));
                    ui.weak(format!("cwd: {cwd}"));
                    ui.separator();
                    egui::ScrollArea::vertical()
                        .auto_shrink([false, false])
                        .stick_to_bottom(true)
                        .show(ui, |ui| {
                            ui.add(
                                egui::Label::new(
                                    egui::RichText::new(&output).monospace(),
                                )
                                .wrap(),
                            );
                        });
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
