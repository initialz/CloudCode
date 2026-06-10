//! Shared path helpers for the agent's on-disk state.
//!
//! The state-dir convention (`CLOUDCODE_STATE_DIR` env → `XDG_STATE_HOME/cloudcode`
//! → `~/.local/state/cloudcode`) used to be a private duplicated function in both
//! `update.rs` and `supervise.rs`. It's promoted here as a single shared `pub fn`
//! so the browser endpoint (Task 4) and the updater/supervisor all agree on it.

use std::path::PathBuf;

/// Resolve the CloudCode state root:
/// `CLOUDCODE_STATE_DIR` env → `XDG_STATE_HOME/cloudcode` →
/// `~/.local/state/cloudcode`. Returns `None` only if neither an env override
/// nor a home dir can be determined.
pub fn state_dir() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("CLOUDCODE_STATE_DIR") {
        return Some(PathBuf::from(p));
    }
    let base = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .or_else(|| dirs::home_dir().map(|h| h.join(".local").join("state")))?;
    Some(base.join("cloudcode"))
}

/// The agent-scoped state dir, `<state>/agent`. This is where the Chrome
/// profile, version symlinks, and browser scratch dir all live.
pub fn agent_state_dir() -> Option<PathBuf> {
    state_dir().map(|s| s.join("agent"))
}
