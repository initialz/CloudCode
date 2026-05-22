//! Per-workspace OS-level sandbox.
//!
//! Wraps the `claude` subprocess so it can only touch the files it needs:
//! the active workspace dir, the user's `~/.claude` credentials dir, and a
//! small set of system read-only paths. Network is left open so claude can
//! reach the Anthropic API, package registries, git remotes, etc.
//!
//! - **macOS**: Seatbelt via `sandbox_init_with_parameters` + a SBPL
//!   profile authored in this crate.
//! - **Linux**: user + mount + PID namespaces + seccomp (TODO; currently
//!   returns an unimplemented error and the agent should refuse to enable
//!   the sandbox on this platform until it lands).
//!
//! The implementation is original. The high-level approach (Seatbelt on
//! macOS, namespaces + seccomp on Linux) is the same one used by
//! Chromium's renderer sandbox, bubblewrap, and many others — that
//! pattern is a published technique, not anyone's code.

use anyhow::Result;
use std::path::PathBuf;

/// Inputs the sandbox profile interpolates into its allow rules.
#[derive(Debug, Clone)]
pub struct SandboxParams {
    /// The workspace directory `claude` will be working in. Read + write
    /// access is granted on this subtree.
    pub workspace: PathBuf,
    /// Root that holds every account's workspaces on this agent. We deny
    /// reads on the whole subtree so a session can't peek into other
    /// workspaces or other accounts — then explicitly re-allow reads on
    /// `workspace` above.
    pub workspace_root: PathBuf,
    /// The user's home dir. The sandbox grants RW only to `~/.claude`
    /// (OAuth) and read-only access elsewhere.
    pub home: PathBuf,
    /// Which sandbox profile to apply. `Strict` is the per-workspace
    /// secrets-and-persistence-hardened profile that ships with the
    /// "sandbox = on" account toggle. `Permissive` is the minimal
    /// profile we apply even when the toggle is off — it lets the
    /// process touch the whole host (network, secrets, all of ~/,
    /// /tmp, …) but still denies access to *other accounts'*
    /// workspaces under WORKSPACE_ROOT. Sandbox = off is therefore
    /// no longer the same as "no sandbox at all" — cross-account
    /// isolation is unconditional.
    pub mode: SandboxMode,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SandboxMode {
    Strict,
    Permissive,
}

impl SandboxMode {
    /// Stable wire form for the `sandbox-exec` subcommand. Kept ASCII
    /// so it round-trips through CLI args cleanly.
    pub fn as_str(self) -> &'static str {
        match self {
            SandboxMode::Strict => "strict",
            SandboxMode::Permissive => "permissive",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "strict" => Some(SandboxMode::Strict),
            "permissive" => Some(SandboxMode::Permissive),
            _ => None,
        }
    }
}

/// Whether the workspace sandbox is implemented on this platform.
pub fn is_supported() -> bool {
    cfg!(target_os = "macos")
}

/// Apply the sandbox to the calling process. Inherits to all child
/// processes the caller spawns afterwards. Once applied it cannot be
/// removed for the lifetime of the process.
pub fn apply(params: &SandboxParams) -> Result<()> {
    #[cfg(target_os = "macos")]
    {
        macos::apply(params)
    }
    #[cfg(target_os = "linux")]
    {
        linux::apply(params)
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        let _ = params;
        Err(anyhow::anyhow!(
            "workspace sandbox is not implemented on this platform"
        ))
    }
}

#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "linux")]
mod linux;
