use serde::Deserialize;
use std::path::{Path, PathBuf};

#[derive(Debug, Deserialize)]
pub struct Config {
    pub server: ServerConfig,
    /// argon2id hash of the global agent registration token. Any agent that
    /// presents the plaintext token in its `hello` frame is accepted.
    pub agents: AgentsConfig,
    /// Legacy accounts inline in hub.toml. On first run with an empty db
    /// the hub imports these into SQLite; afterwards accounts live in the
    /// db and this list is informational only. Keep / remove as you like.
    #[serde(default)]
    pub accounts: Vec<Account>,
    #[serde(default)]
    pub admin: AdminConfig,
    #[serde(default)]
    pub workspaces: WorkspacesConfig,
}

#[derive(Debug, Deserialize)]
pub struct ServerConfig {
    pub listen: String,
    #[serde(default = "default_audit_log")]
    pub audit_log: String,
}

fn default_audit_log() -> String {
    "./audit.jsonl".into()
}

#[derive(Debug, Deserialize)]
pub struct AgentsConfig {
    /// argon2id hash of the registration token printed by `cloudcode-hub
    /// --init` (give the plaintext token to agent operators; it is the
    /// same token for every agent and never expires until you re-init).
    pub registration_token_hash: String,
}

#[derive(Debug, Deserialize, Clone)]
pub struct Account {
    pub name: String,
    pub token_hash: String,
}

#[derive(Debug, Deserialize, Clone)]
pub struct AdminConfig {
    /// SQLite database file. Holds accounts, audit events, and session
    /// records used by the admin UI.
    #[serde(default = "default_db_path")]
    pub db_path: PathBuf,
    /// Username the admin must type at login. The token alone isn't
    /// enough — both fields are compared. Default "admin"; override
    /// in hub.toml to anything non-obvious as a small defence-in-
    /// depth measure (a leaked token without the right username is
    /// still useless).
    #[serde(default = "default_admin_username")]
    pub username: String,
    /// argon2id hash of the admin UI login token. If absent the admin
    /// HTTP server is not started. The plaintext is printed once by
    /// `cloudcode-hub --init`.
    #[serde(default)]
    pub token_hash: Option<String>,
    /// HTTP listen address for the admin UI. Defaults to all interfaces
    /// on port 7101 so a fresh install is reachable out of the box; put
    /// a TLS-terminating reverse proxy in front in production so the
    /// admin token doesn't traverse the network in cleartext, and use
    /// a firewall / cloud security group to gate who can hit it.
    #[serde(default = "default_admin_listen")]
    pub listen: String,
}

impl Default for AdminConfig {
    fn default() -> Self {
        Self {
            db_path: default_db_path(),
            username: default_admin_username(),
            token_hash: None,
            listen: default_admin_listen(),
        }
    }
}

fn default_admin_username() -> String {
    "admin".into()
}

fn default_db_path() -> PathBuf {
    PathBuf::from("./cloudcode-hub.db")
}

fn default_admin_listen() -> String {
    "0.0.0.0:7101".into()
}

/// Hub-canonical workspace storage. Defaults to `./hub/workspaces`
/// (relative to the hub's cwd, like `./audit.jsonl` /
/// `./cloudcode-hub.db`). Override `root` with an absolute path when
/// workspaces should live on a separate volume.
#[derive(Debug, Deserialize, Clone, Default)]
pub struct WorkspacesConfig {
    #[serde(default)]
    pub root: Option<PathBuf>,
}

impl Config {
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let s = std::fs::read_to_string(path)?;
        let mut cfg: Config = toml::from_str(&s)?;
        // Resolve workspaces.root to an absolute path here, so
        // callers (notably main.rs) can use it directly without
        // worrying about cwd. Unset => bake in the default
        // `./hub/workspaces` and anchor it; explicit relative =>
        // anchor; explicit absolute => pass through. Anchor uses
        // the config file's directory so a daemon restart from a
        // different cwd doesn't silently relocate the canonical
        // workspace store.
        let root = cfg
            .workspaces
            .root
            .take()
            .unwrap_or_else(|| PathBuf::from("./hub/workspaces"));
        cfg.workspaces.root = Some(anchor_to_config_dir(path, &root));
        Ok(cfg)
    }
}

/// Resolve a possibly-relative path against the config file's
/// directory. Absolute paths pass through. Mirrors the helper in
/// `crates/agent/src/config.rs`.
fn anchor_to_config_dir(config_path: &Path, p: &Path) -> PathBuf {
    if p.is_absolute() {
        return p.to_path_buf();
    }
    let Some(parent) = config_path.parent() else {
        return p.to_path_buf();
    };
    let base = std::fs::canonicalize(parent).unwrap_or_else(|_| parent.to_path_buf());
    base.join(p)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn minimal_toml() -> &'static str {
        r#"
[server]
listen = "0.0.0.0:7100"

[agents]
registration_token_hash = "$argon2id$dummy"
"#
    }

    #[test]
    fn workspaces_root_defaults_to_anchored_subdir() {
        // No [workspaces].root in the file → Config::load fills in
        // `<config_dir>/hub/workspaces` and that's what main.rs sees.
        let dir = tempfile::tempdir().unwrap();
        let cfg_path = dir.path().join("hub.toml");
        std::fs::write(&cfg_path, minimal_toml()).unwrap();
        let cfg = Config::load(&cfg_path).unwrap();
        let expected = std::fs::canonicalize(dir.path())
            .unwrap()
            .join("hub/workspaces");
        assert_eq!(cfg.workspaces.root, Some(expected));
    }

    #[test]
    fn workspaces_root_explicit_relative_is_anchored() {
        let dir = tempfile::tempdir().unwrap();
        let cfg_path = dir.path().join("hub.toml");
        let body = format!(
            "{}\n[workspaces]\nroot = \"./elsewhere/ws\"\n",
            minimal_toml()
        );
        std::fs::write(&cfg_path, body).unwrap();
        let cfg = Config::load(&cfg_path).unwrap();
        let expected = std::fs::canonicalize(dir.path())
            .unwrap()
            .join("elsewhere/ws");
        assert_eq!(cfg.workspaces.root, Some(expected));
    }

    #[test]
    fn workspaces_root_absolute_passes_through() {
        let dir = tempfile::tempdir().unwrap();
        let cfg_path = dir.path().join("hub.toml");
        let body = format!(
            "{}\n[workspaces]\nroot = \"/srv/cloudcode/ws\"\n",
            minimal_toml()
        );
        std::fs::write(&cfg_path, body).unwrap();
        let cfg = Config::load(&cfg_path).unwrap();
        assert_eq!(cfg.workspaces.root, Some(PathBuf::from("/srv/cloudcode/ws")));
    }
}
