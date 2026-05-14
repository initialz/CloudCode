//! 通用 daemon 生命周期管理：start/stop/restart/status。
//!
//! 调用方（cloudcode-hub / cloudcode-agent）注入 service 名和默认 config 路径；
//! 本 crate 会 spawn `current_exe --config <config>`（前台模式），setsid
//! 后台跑，PID 与日志默认放 `~/.local/state/cloudcode/<name>.{pid,log}`，
//! 可由 `CLOUDCODE_STATE_DIR` 覆盖。

use anyhow::{bail, Context, Result};
use clap::Subcommand;
use std::fs::{File, OpenOptions};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread::sleep;
use std::time::Duration;

#[derive(Subcommand)]
pub enum DaemonCmd {
    /// 后台启动 daemon
    Start {
        #[arg(short, long)]
        config: Option<PathBuf>,
    },
    /// 停止 daemon（SIGTERM，5s 后 SIGKILL）
    Stop,
    /// 重启 daemon
    Restart {
        #[arg(short, long)]
        config: Option<PathBuf>,
    },
    /// 显示 daemon 状态、pid 与日志路径
    Status,
}

pub fn run(name: &str, default_config: &str, cmd: DaemonCmd) -> Result<()> {
    run_with_prefix(name, default_config, cmd, &[])
}

/// Same as [`run`] but `prefix_args` are inserted before `--config` when
/// spawning the daemon child. Useful when the daemonized binary needs a
/// subcommand (e.g. `cloudcode-agent supervise --config …`) rather than
/// running its default mode.
pub fn run_with_prefix(
    name: &str,
    default_config: &str,
    cmd: DaemonCmd,
    prefix_args: &[&str],
) -> Result<()> {
    match cmd {
        DaemonCmd::Start { config } => start(name, default_config, config, prefix_args),
        DaemonCmd::Stop => stop(name),
        DaemonCmd::Restart { config } => {
            let _ = stop(name);
            start(name, default_config, config, prefix_args)
        }
        DaemonCmd::Status => status(name),
    }
}

fn state_dir() -> Result<PathBuf> {
    if let Ok(p) = std::env::var("CLOUDCODE_STATE_DIR") {
        let p = PathBuf::from(p);
        std::fs::create_dir_all(&p).with_context(|| format!("creating {}", p.display()))?;
        return Ok(p);
    }
    let base = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .or_else(|| dirs::home_dir().map(|h| h.join(".local").join("state")))
        .context(
            "could not determine state dir; set CLOUDCODE_STATE_DIR, XDG_STATE_HOME, or HOME",
        )?;
    let dir = base.join("cloudcode");
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    Ok(dir)
}

fn pid_path(name: &str) -> Result<PathBuf> {
    Ok(state_dir()?.join(format!("{}.pid", name)))
}

fn log_path(name: &str) -> Result<PathBuf> {
    Ok(state_dir()?.join(format!("{}.log", name)))
}

fn read_pid(path: &Path) -> Result<Option<i32>> {
    match std::fs::read_to_string(path) {
        Ok(s) => {
            let pid = s
                .trim()
                .parse::<i32>()
                .with_context(|| format!("parsing pid from {}", path.display()))?;
            Ok(Some(pid))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
    }
}

fn is_alive(pid: i32) -> bool {
    // kill(pid, 0)：不发信号、只测试是否能给该进程发信号。EPERM 也意味着进程存在。
    let r = unsafe { libc::kill(pid, 0) };
    if r == 0 {
        return true;
    }
    std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

fn start(
    name: &str,
    default_config: &str,
    config: Option<PathBuf>,
    prefix_args: &[&str],
) -> Result<()> {
    let pid_p = pid_path(name)?;
    let log_p = log_path(name)?;

    if let Some(pid) = read_pid(&pid_p)? {
        if is_alive(pid) {
            println!("{} already running (pid {})", name, pid);
            return Ok(());
        }
        let _ = std::fs::remove_file(&pid_p);
    }

    let cfg = config.unwrap_or_else(|| PathBuf::from(default_config));
    let cfg = std::fs::canonicalize(&cfg)
        .with_context(|| format!("locating config {}", cfg.display()))?;

    let exe = std::env::current_exe().context("finding current executable")?;

    let log = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_p)
        .with_context(|| format!("opening log {}", log_p.display()))?;
    let log_err = log.try_clone()?;
    let dev_null = File::open("/dev/null").context("opening /dev/null")?;

    let mut cmd = Command::new(&exe);
    for a in prefix_args {
        cmd.arg(a);
    }
    cmd.arg("--config")
        .arg(&cfg)
        .stdin(Stdio::from(dev_null))
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(log_err));

    unsafe {
        cmd.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }

    let child = cmd
        .spawn()
        .with_context(|| format!("spawning {}", exe.display()))?;
    let pid = child.id() as i32;
    std::fs::write(&pid_p, format!("{}\n", pid))
        .with_context(|| format!("writing {}", pid_p.display()))?;

    // 给子进程一点时间，万一立刻挂了就早点报错。
    sleep(Duration::from_millis(250));
    if !is_alive(pid) {
        let _ = std::fs::remove_file(&pid_p);
        bail!(
            "{} failed to start; tail the log at {}",
            name,
            log_p.display()
        );
    }
    println!("{} started (pid {})", name, pid);
    println!("  config: {}", cfg.display());
    println!("  logs:   {}", log_p.display());
    Ok(())
}

fn stop(name: &str) -> Result<()> {
    let pid_p = pid_path(name)?;
    let Some(pid) = read_pid(&pid_p)? else {
        println!("{} not running (no pid file)", name);
        return Ok(());
    };
    if !is_alive(pid) {
        let _ = std::fs::remove_file(&pid_p);
        println!("{} not running (stale pid {})", name, pid);
        return Ok(());
    }

    if unsafe { libc::kill(pid, libc::SIGTERM) } != 0 {
        bail!(
            "kill(pid {}, SIGTERM) failed: {}",
            pid,
            std::io::Error::last_os_error()
        );
    }
    for _ in 0..50 {
        if !is_alive(pid) {
            break;
        }
        sleep(Duration::from_millis(100));
    }
    if is_alive(pid) {
        println!("{} did not exit in 5s; sending SIGKILL", name);
        unsafe {
            libc::kill(pid, libc::SIGKILL);
        }
        sleep(Duration::from_millis(200));
    }
    let _ = std::fs::remove_file(&pid_p);
    println!("{} stopped (pid {})", name, pid);
    Ok(())
}

fn status(name: &str) -> Result<()> {
    let pid_p = pid_path(name)?;
    let log_p = log_path(name)?;
    match read_pid(&pid_p)? {
        Some(pid) if is_alive(pid) => println!("{}: running (pid {})", name, pid),
        Some(pid) => println!("{}: stopped (stale pid {})", name, pid),
        None => println!("{}: stopped", name),
    }
    println!("  pid:  {}", pid_p.display());
    println!("  logs: {}", log_p.display());
    Ok(())
}
