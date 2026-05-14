//! Supervisor for the agent run loop.
//!
//! `cloudcode-agent supervise` keeps a child `cloudcode-agent run` process
//! alive: on a clean exit it restarts immediately (that's how self-update
//! rolls forward to a new binary), on a crash it backs off exponentially
//! to 30 s. SIGTERM / SIGINT are forwarded to the child and we wait up to
//! 5 s for it to drain before exiting ourselves.
//!
//! After N consecutive failures we try the `previous` symlink as a
//! last-resort rollback. We never tear that symlink down, so an admin
//! can still inspect what went wrong and re-roll forward by hand.
//!
//! Implemented with std::process::Command and signal-hook (no tokio) —
//! this layer is fundamentally blocking and doesn't need an executor.

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

const MAX_BACKOFF: Duration = Duration::from_secs(30);
const INITIAL_BACKOFF: Duration = Duration::from_secs(1);
const SHUTDOWN_GRACE: Duration = Duration::from_secs(5);
/// After this many consecutive failures, swap to the `previous` symlink
/// (if any) so a botched self-update doesn't wedge the agent forever.
const ROLLBACK_THRESHOLD: u32 = 10;

pub fn run(config_path: PathBuf) -> Result<()> {
    let self_exe = std::env::current_exe().context("locating current cloudcode-agent binary")?;
    tracing::info!(
        exe = %self_exe.display(),
        config = %config_path.display(),
        "supervisor starting"
    );

    let shutdown = Arc::new(AtomicBool::new(false));
    install_signal_handlers(shutdown.clone())?;

    let mut next_delay = INITIAL_BACKOFF;
    let mut consecutive_failures: u32 = 0;
    let mut rolled_back = false;

    loop {
        if shutdown.load(Ordering::SeqCst) {
            tracing::info!("supervisor exiting before spawn");
            return Ok(());
        }

        let mut child = match spawn_child(&self_exe, &config_path) {
            Ok(c) => c,
            Err(e) => {
                tracing::error!(error = %e, "failed to spawn agent child; backing off");
                consecutive_failures = consecutive_failures.saturating_add(1);
                sleep_interruptible(next_delay, &shutdown);
                next_delay = (next_delay * 2).min(MAX_BACKOFF);
                continue;
            }
        };
        let child_pid = child.id() as i32;
        tracing::info!(pid = child_pid, "agent child spawned");

        let exit_status = wait_with_shutdown(&mut child, &shutdown);
        if shutdown.load(Ordering::SeqCst) {
            graceful_kill(&mut child);
            tracing::info!("supervisor exiting after child cleanup");
            return Ok(());
        }

        match exit_status {
            Ok(Some(status)) if status.success() => {
                tracing::info!(pid = child_pid, "child requested restart (exit 0)");
                next_delay = INITIAL_BACKOFF;
                consecutive_failures = 0;
                // Tight loop — supervisor relies on this to swap into a
                // freshly installed binary after self-update.
                continue;
            }
            Ok(Some(status)) => {
                consecutive_failures = consecutive_failures.saturating_add(1);
                tracing::warn!(
                    pid = child_pid,
                    status = ?status,
                    failures = consecutive_failures,
                    "agent child exited non-zero"
                );
            }
            // Ok(None) only happens when the shutdown flag was set — and
            // the branch above already returned. Treat it as a benign
            // continue, even though we shouldn't see it in practice.
            Ok(None) => continue,
            Err(e) => {
                consecutive_failures = consecutive_failures.saturating_add(1);
                tracing::warn!(error = %e, failures = consecutive_failures, "wait failed");
            }
        }

        if consecutive_failures >= ROLLBACK_THRESHOLD && !rolled_back {
            if try_rollback_to_previous() {
                tracing::warn!(
                    "rolled back agent to ~/.local/state/cloudcode/agent/previous \
                     after {} consecutive failures",
                    consecutive_failures
                );
                rolled_back = true;
                // Give the rollback a clean baseline.
                next_delay = INITIAL_BACKOFF;
                consecutive_failures = 0;
                continue;
            } else {
                tracing::error!(
                    "{} consecutive failures and no previous version to roll back to; \
                     continuing to back off",
                    consecutive_failures
                );
            }
        }

        sleep_interruptible(next_delay, &shutdown);
        next_delay = (next_delay * 2).min(MAX_BACKOFF);
    }
}

fn spawn_child(self_exe: &Path, config_path: &Path) -> std::io::Result<Child> {
    let mut cmd = Command::new(self_exe);
    cmd.arg("run").arg("--config").arg(config_path);
    cmd.spawn()
}

/// Returns once the child has exited OR the shutdown flag is set. On
/// shutdown we don't wait for the child here — `run()` calls
/// `graceful_kill` immediately afterwards and that owns the 5 s grace
/// period + SIGKILL. The `Result<Option<ExitStatus>>` lets the caller
/// distinguish "exited cleanly" from "shutdown requested mid-flight".
fn wait_with_shutdown(
    child: &mut Child,
    shutdown: &Arc<AtomicBool>,
) -> std::io::Result<Option<std::process::ExitStatus>> {
    loop {
        if shutdown.load(Ordering::SeqCst) {
            return Ok(None);
        }
        match child.try_wait()? {
            Some(status) => return Ok(Some(status)),
            None => std::thread::sleep(Duration::from_millis(200)),
        }
    }
}

fn graceful_kill(child: &mut Child) {
    let pid = child.id() as i32;
    forward_sigterm(child);
    let deadline = Instant::now() + SHUTDOWN_GRACE;
    while Instant::now() < deadline {
        match child.try_wait() {
            Ok(Some(_)) => return,
            Ok(None) => std::thread::sleep(Duration::from_millis(100)),
            Err(e) => {
                tracing::debug!(pid, error = %e, "try_wait during shutdown");
                return;
            }
        }
    }
    tracing::warn!(pid, "child did not exit in {:?}; sending SIGKILL", SHUTDOWN_GRACE);
    let _ = child.kill();
    let _ = child.wait();
}

fn forward_sigterm(child: &Child) {
    let pid = child.id() as i32;
    // SAFETY: kill is safe; we trust pid is owned by us.
    let r = unsafe { libc::kill(pid, libc::SIGTERM) };
    if r != 0 {
        let err = std::io::Error::last_os_error();
        // ESRCH = already exited; that's fine.
        if err.raw_os_error() != Some(libc::ESRCH) {
            tracing::debug!(pid, error = %err, "SIGTERM to child failed");
        }
    }
}

fn install_signal_handlers(flag: Arc<AtomicBool>) -> Result<()> {
    use signal_hook::consts::signal::{SIGINT, SIGTERM};
    use signal_hook::flag as sh_flag;
    sh_flag::register(SIGTERM, flag.clone()).context("installing SIGTERM handler")?;
    sh_flag::register(SIGINT, flag).context("installing SIGINT handler")?;
    Ok(())
}

fn sleep_interruptible(total: Duration, shutdown: &Arc<AtomicBool>) {
    let deadline = Instant::now() + total;
    while Instant::now() < deadline {
        if shutdown.load(Ordering::SeqCst) {
            return;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}

/// Best-effort rollback after repeated failures: if a `previous` symlink
/// exists under the agent versions dir, swap it into `current`. Returns
/// `true` if we touched the symlink, `false` otherwise (no rollback
/// target, or filesystem error — supervisor keeps backing off in that
/// case).
fn try_rollback_to_previous() -> bool {
    let Some(state) = state_dir() else {
        return false;
    };
    let agent_dir = state.join("agent");
    let previous = agent_dir.join("previous");
    let current = agent_dir.join("current");
    let prev_target = match std::fs::read_link(&previous) {
        Ok(t) => t,
        Err(_) => return false,
    };
    // Atomic-ish swap: write tmp → rename onto current.
    let tmp = agent_dir.join("current.rollback.tmp");
    let _ = std::fs::remove_file(&tmp);
    if std::os::unix::fs::symlink(&prev_target, &tmp).is_err() {
        return false;
    }
    if std::fs::rename(&tmp, &current).is_err() {
        let _ = std::fs::remove_file(&tmp);
        return false;
    }
    true
}

fn state_dir() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("CLOUDCODE_STATE_DIR") {
        return Some(PathBuf::from(p));
    }
    let base = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .or_else(|| dirs::home_dir().map(|h| h.join(".local").join("state")))?;
    Some(base.join("cloudcode"))
}
