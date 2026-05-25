mod audit;
mod config;
mod fs;
mod jsonl;
mod name;
mod pty;
mod sandbox;
mod supervise;
mod tunnel;
mod update;
mod ws;

use anyhow::{anyhow, Context};
use clap::{Parser, Subcommand};
use std::ffi::CString;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::config::Config;
use crate::pty::PtyManager;

pub struct AppState {
    pub name: String,
    pub config: Config,
    pub manager: Arc<PtyManager>,
    /// Shared slot the ws layer plugs the live hub sender into so the
    /// agent-level audit task (spawned once at startup) can ship
    /// `UserInteraction` frames to the hub.
    pub audit_slot: audit::SenderSlot,
}

#[derive(Parser)]
#[command(
    name = "cloudcode-agent",
    version,
    about = "Cloudcode agent: dials a hub via WebSocket and runs claude subprocesses on demand"
)]
struct Cli {
    /// Path to agent config. With no subcommand, agent runs in the foreground
    /// using this config and streams logs to stdout.
    #[arg(short, long, default_value = "agent.toml", global = true)]
    config: PathBuf,

    /// One-time setup: write a fresh agent.toml template at `--config`.
    /// Refuses to overwrite if the file already exists. After running this,
    /// paste the agent registration token from your hub admin into
    /// [auth].registration_token before starting the agent.
    #[arg(long)]
    init: bool,

    #[command(subcommand)]
    cmd: Option<Cmd>,
}

#[derive(Subcommand)]
enum Cmd {
    /// Run the agent main loop (connect to hub, serve PTYs). This is the
    /// child of the supervisor; you usually want `supervise` or `daemon`
    /// rather than calling `run` directly. Kept exposed so the supervisor
    /// can re-exec the same binary.
    Run,
    /// Keep an agent process alive: spawn `cloudcode-agent run`, restart
    /// it on crash with exponential backoff, restart it immediately on a
    /// clean exit (so self-update rolls forward). Forwards SIGTERM /
    /// SIGINT to the child.
    Supervise,
    /// Clear the `agent/current` symlink so the next `daemon start` /
    /// `supervise` bootstraps it from the currently-installed binary.
    /// Use after a manual reinstall (install.sh ...) when you want to
    /// undo a prior self-update and pin the daemon to the binary on
    /// PATH.
    ResetBinary,
    /// 后台管理 agent daemon（start/stop/restart/status）— daemon `start`
    /// 实际 spawn 的是 `cloudcode-agent supervise`，因此 self-update 能在
    /// 后台运行时透明热切换。
    Daemon {
        #[command(subcommand)]
        cmd: cloudcode_daemon::DaemonCmd,
    },
    /// Internal: wrap the following command in the workspace sandbox and
    /// exec it. Used by the agent's PTY spawn path; not meant for users.
    #[command(hide = true)]
    SandboxExec {
        #[arg(long)]
        workspace: PathBuf,
        #[arg(long)]
        workspace_root: PathBuf,
        #[arg(long)]
        home: PathBuf,
        /// `strict` (the full per-workspace profile, applied when the
        /// per-account sandbox toggle is on) or `permissive` (a
        /// minimal cross-account-only profile, applied even when the
        /// toggle is off). Defaults to `strict` for backwards
        /// compatibility with older callers.
        #[arg(long, default_value = "strict")]
        mode: String,
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        argv: Vec<String>,
    },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Pick rustls' ring CryptoProvider before any TLS code runs; rustls 0.23
    // requires this when crate features can't disambiguate a default.
    let _ = rustls::crypto::ring::default_provider().install_default();

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,cloudcode_agent=debug".into()),
        )
        .init();

    let cli = Cli::parse();
    if cli.init {
        if cli.cmd.is_some() {
            return Err(anyhow!("--init cannot be combined with a subcommand"));
        }
        return init_config(&cli.config);
    }
    match cli.cmd {
        // Backward compat: pre-v1.6 daemon installs invoke the binary
        // with no subcommand. We treat that the same as `run`.
        None => serve(cli.config).await,
        Some(Cmd::Run) => serve(cli.config).await,
        Some(Cmd::Supervise) => supervise::run(cli.config),
        Some(Cmd::ResetBinary) => supervise::reset_current(),
        // daemon start re-execs us with the `supervise` subcommand so
        // crashes (and self-update exits) can be recovered without
        // tearing the daemon down.
        Some(Cmd::Daemon { cmd }) => {
            cloudcode_daemon::run_with_prefix("agent", "agent.toml", cmd, &["supervise"])
        }
        Some(Cmd::SandboxExec {
            workspace,
            workspace_root,
            home,
            mode,
            argv,
        }) => run_sandbox_exec(workspace, workspace_root, home, mode, argv),
    }
}

/// Apply the workspace sandbox and exec the target command. This function
/// never returns on success — it replaces the current process image via
/// `execvp(3)`. On error it writes to stderr and exits with status 127.
fn run_sandbox_exec(
    workspace: PathBuf,
    workspace_root: PathBuf,
    home: PathBuf,
    mode: String,
    argv: Vec<String>,
) -> anyhow::Result<()> {
    if argv.is_empty() {
        return Err(anyhow!("sandbox-exec: missing target command after `--`"));
    }
    let mode = sandbox::SandboxMode::parse(&mode)
        .ok_or_else(|| anyhow!("sandbox-exec: unknown --mode {:?}", mode))?;
    sandbox::apply(&sandbox::SandboxParams {
        workspace,
        workspace_root,
        home,
        mode,
    })
    .context("applying workspace sandbox")?;

    let program = CString::new(argv[0].as_bytes())
        .map_err(|_| anyhow!("sandbox-exec: target program path contains NUL"))?;
    let c_argv: Vec<CString> = argv
        .iter()
        .map(|s| {
            CString::new(s.as_bytes())
                .map_err(|_| anyhow!("sandbox-exec: argv element contains NUL"))
        })
        .collect::<anyhow::Result<_>>()?;
    let mut raw: Vec<*const libc::c_char> = c_argv.iter().map(|s| s.as_ptr()).collect();
    raw.push(std::ptr::null());

    unsafe {
        libc::execvp(program.as_ptr(), raw.as_ptr());
    }
    let errno = std::io::Error::last_os_error();
    Err(anyhow!(
        "sandbox-exec: execvp `{}` failed: {}",
        argv[0],
        errno
    ))
}

async fn serve(config_path: PathBuf) -> anyhow::Result<()> {
    if !config_path.exists() {
        return Err(anyhow!(
            "{} not found; run `cloudcode-agent --init --config {}` to generate one",
            config_path.display(),
            config_path.display()
        ));
    }

    let config =
        Config::load(&config_path).with_context(|| format!("loading {}", config_path.display()))?;

    let name = config
        .agent
        .name
        .clone()
        .unwrap_or_else(name::default_agent_name);
    tracing::info!(agent = %name, "starting cloudcode-agent");

    let manager = Arc::new(PtyManager::new(
        config.claude.clone(),
        config.tools.tools.clone(),
        config.tools.default.clone(),
        config.tmux.clone(),
        config.recording.clone(),
        config.sandbox.clone(),
    )?);

    // Process-level audit pipeline: watches ~/.claude/projects/ and
    // ships every user-typed prompt up to the hub. Spawned ONCE per
    // agent lifetime — survives reconnects via SenderSlot.
    let audit_slot = audit::SenderSlot::new();
    let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("/"));
    let workspace_root = expand_workspace_root(&config.claude.workspace_root);
    audit::spawn(home, workspace_root, audit_slot.clone());

    let state = Arc::new(AppState {
        name,
        config,
        manager,
        audit_slot,
    });

    ws::run(state).await
}

/// Mirror of `pty::expand_path` for the audit pipeline. Kept tiny + local
/// so `audit.rs` doesn't need to depend on `pty.rs` just for tilde
/// expansion.
fn expand_workspace_root(p: &Path) -> PathBuf {
    let s = p.to_string_lossy();
    let expanded = if let Some(rest) = s.strip_prefix("~/") {
        if let Some(home) = dirs::home_dir() {
            home.join(rest)
        } else {
            p.to_path_buf()
        }
    } else {
        p.to_path_buf()
    };
    if expanded.is_absolute() {
        expanded
    } else if let Ok(abs) = std::fs::canonicalize(&expanded) {
        abs
    } else if let Ok(cwd) = std::env::current_dir() {
        cwd.join(&expanded)
    } else {
        expanded
    }
}

fn init_config(path: &Path) -> anyhow::Result<()> {
    if path.exists() {
        return Err(anyhow!(
            "{} already exists; refusing to overwrite. Delete it first if you really want to re-init.",
            path.display()
        ));
    }

    let agent_name = name::default_agent_name();

    let template = format!(
        r#"# Auto-generated on first run. Edit [hub].url and [auth].registration_token
# before re-running.

[hub]
url = "wss://hub.example.com/v1/agent/ws"

[agent]
# Auto-detected from hostname-user; override if hub reports name_taken.
# name = "{agent_name}"

[auth]
# Plaintext registration token issued by the hub. Ask your hub admin —
# they got it from `cloudcode-hub --init`.
registration_token = "ag_PASTE_TOKEN_HERE"

# [claude] only carries workspace_root now (filesystem root for per-account
# workspace dirs). The tool wiring moved to [tools] below.
# [claude]
# workspace_root = "~/cloudcode-agent/workspaces"

# [tools] picks which AI CLIs this agent can spawn. The first pane runs
# `default`; additional panes (split) can run any other entry by name.
# Add a [tools.<name>] table per tool you want to expose.
[tools]
default = "claude"

[tools.claude]
executable     = "claude"                # PATH lookup by default
resume_command = "claude --continue"     # empty -> always fresh
extra_args     = []

# Uncomment to also expose Codex side-by-side with Claude. Requires
# `codex` to be installed and logged in as this OS user.
# [tools.codex]
# executable     = "codex"
# resume_command = ""    # codex doesn't support resume; relaunch fresh
# extra_args     = []

# [sandbox] is deprecated. The sandbox toggle moved to the hub —
# it's now per-account, set from the admin UI's Accounts page.
# Any value here is ignored; the section is left commented out
# only so older agent.toml files keep parsing.
# [sandbox]
# enabled = false
"#
    );

    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
    }
    std::fs::write(path, template).with_context(|| format!("writing {}", path.display()))?;

    println!("# Wrote {}", path.display());
    println!("# Auto-detected agent name: {}", agent_name);
    println!();
    println!("# Next steps:");
    println!("#   1) Ask your hub admin for the agent registration token");
    println!("#      (printed once by `cloudcode-hub --init`).");
    println!(
        "#   2) Paste it into [auth].registration_token in {}.",
        path.display()
    );
    println!("#   3) Set [hub].url to your hub endpoint, then run cloudcode-agent.");
    Ok(())
}
