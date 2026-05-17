//! Supervisor for the hub run loop.
//!
//! Same shape as `crates/agent/src/supervise.rs`. `cloudcode-hub
//! supervise` keeps a child `cloudcode-hub` (no subcommand) process
//! alive: clean exits restart immediately (so a self-update rolls
//! forward to the new binary), crashes back off exponentially to
//! 30 s, SIGTERM / SIGINT are forwarded to the child with a 5 s
//! grace period.
//!
//! After 10 consecutive failures we try the `previous` symlink as a
//! last-resort rollback so a broken self-update doesn't wedge the
//! hub forever.

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

const MAX_BACKOFF: Duration = Duration::from_secs(30);
const INITIAL_BACKOFF: Duration = Duration::from_secs(1);
const SHUTDOWN_GRACE: Duration = Duration::from_secs(5);
const ROLLBACK_THRESHOLD: u32 = 10;

pub fn run(config_path: PathBuf) -> Result<()> {
    let self_exe = std::env::current_exe().context("locating current cloudcode-hub binary")?;
    bootstrap_current_symlink(&self_exe);
    let spawn_target = active_binary_path().unwrap_or_else(|| self_exe.clone());
    tracing::info!(
        self_exe = %self_exe.display(),
        spawn_target = %spawn_target.display(),
        config = %config_path.display(),
        "hub supervisor starting"
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

        let target = active_binary_path().unwrap_or_else(|| self_exe.clone());
        let mut child = match spawn_child(&target, &config_path) {
            Ok(c) => c,
            Err(e) => {
                tracing::error!(error = %e, "failed to spawn hub child; backing off");
                consecutive_failures = consecutive_failures.saturating_add(1);
                sleep_interruptible(next_delay, &shutdown);
                next_delay = (next_delay * 2).min(MAX_BACKOFF);
                continue;
            }
        };
        let child_pid = child.id() as i32;
        tracing::info!(pid = child_pid, "hub child spawned");

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
                continue;
            }
            Ok(Some(status)) => {
                consecutive_failures = consecutive_failures.saturating_add(1);
                tracing::warn!(
                    pid = child_pid,
                    status = ?status,
                    failures = consecutive_failures,
                    "hub child exited non-zero"
                );
            }
            Ok(None) => continue,
            Err(e) => {
                consecutive_failures = consecutive_failures.saturating_add(1);
                tracing::warn!(error = %e, failures = consecutive_failures, "wait failed");
            }
        }

        if consecutive_failures >= ROLLBACK_THRESHOLD && !rolled_back {
            if try_rollback_to_previous() {
                tracing::warn!(
                    "rolled back hub to ~/.local/state/cloudcode/hub/previous \
                     after {} consecutive failures",
                    consecutive_failures
                );
                rolled_back = true;
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
    // Hub has no `run` subcommand — bare invocation with --config IS
    // the run path. (Unlike the agent, which uses an explicit `run`
    // because it has multiple subcommands behind --init.)
    let mut cmd = Command::new(self_exe);
    cmd.arg("--config").arg(config_path);
    cmd.spawn()
}

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

fn try_rollback_to_previous() -> bool {
    let Some(state) = crate::update::state_dir() else {
        return false;
    };
    let hub_dir = state.join("hub");
    let previous = hub_dir.join("previous");
    let current = hub_dir.join("current");
    let prev_target = match std::fs::read_link(&previous) {
        Ok(t) => t,
        Err(_) => return false,
    };
    let tmp = hub_dir.join("current.rollback.tmp");
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

fn bootstrap_current_symlink(self_exe: &Path) {
    let Some(state) = crate::update::state_dir() else { return };
    let hub_dir = state.join("hub");
    if std::fs::create_dir_all(&hub_dir).is_err() {
        return;
    }
    let current = hub_dir.join("current");
    if current.symlink_metadata().is_ok() {
        return;
    }
    if let Err(e) = std::os::unix::fs::symlink(self_exe, &current) {
        tracing::warn!(error = %e, "could not bootstrap hub/current symlink");
    }
}

fn active_binary_path() -> Option<PathBuf> {
    let state = crate::update::state_dir()?;
    let current = state.join("hub").join("current");
    if current.symlink_metadata().is_ok() {
        Some(current)
    } else {
        None
    }
}
