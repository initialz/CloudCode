mod config;
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
            argv,
        }) => run_sandbox_exec(workspace, workspace_root, home, argv),
    }
}

/// Apply the workspace sandbox and exec the target command. This function
/// never returns on success — it replaces the current process image via
/// `execvp(3)`. On error it writes to stderr and exits with status 127.
fn run_sandbox_exec(
    workspace: PathBuf,
    workspace_root: PathBuf,
    home: PathBuf,
    argv: Vec<String>,
) -> anyhow::Result<()> {
    if argv.is_empty() {
        return Err(anyhow!("sandbox-exec: missing target command after `--`"));
    }
    sandbox::apply(&sandbox::SandboxParams {
        workspace,
        workspace_root,
        home,
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
        config.tmux.clone(),
        config.recording.clone(),
        config.sandbox.clone(),
    )?);

    let state = Arc::new(AppState {
        name,
        config,
        manager,
    });

    ws::run(state).await
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

# [claude] section is optional; defaults below are usually fine. The agent
# spawns `claude` as a subprocess for every user turn, so claude must be
# installed and you must have run `claude /login` once as the same OS user.
# [claude]
# executable     = "claude"                            # PATH lookup by default
# workspace_root = "~/cloudcode-agent/workspaces"      # one dir per workspace
# extra_args     = []                                  # appended to claude args

# [sandbox] wraps each spawned claude (and its tmux session) in an OS-level
# sandbox so it can only touch the active workspace dir + ~/.claude + scratch
# space. Off by default — opt in once you've confirmed the profile fits the
# tools your projects use. macOS only at the moment (Seatbelt); Linux is
# coming. With it enabled, escaping the workspace is a kernel-enforced EPERM
# rather than a "please don't".
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
