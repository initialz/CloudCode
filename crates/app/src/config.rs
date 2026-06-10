//! Client config loading, ported from `crates/client/src/main.rs`.
//!
//! Same on-disk format and default path as the CLI client
//! (`$XDG_CONFIG_HOME/cloudcode/config.toml`, falling back to
//! `~/.config/cloudcode/config.toml`), so the desktop app reads the
//! exact same `config.toml` the user already set up for `cloudcode`.

use anyhow::{Context, Result};
use serde::Deserialize;
use std::path::{Path, PathBuf};

#[derive(Deserialize, Debug, Clone)]
pub struct HubConfig {
    pub hub_url: String,
    pub token: String,
}

pub fn default_config_path() -> Result<PathBuf> {
    if let Ok(p) = std::env::var("XDG_CONFIG_HOME") {
        if !p.is_empty() {
            return Ok(PathBuf::from(p).join("cloudcode").join("config.toml"));
        }
    }
    let home = dirs::home_dir().context("could not find home dir")?;
    Ok(home.join(".config").join("cloudcode").join("config.toml"))
}

pub fn resolve_config_path(override_path: Option<&Path>) -> Result<PathBuf> {
    match override_path {
        Some(p) => Ok(p.to_path_buf()),
        None => default_config_path(),
    }
}

pub fn load_config(override_path: Option<&Path>) -> Result<HubConfig> {
    let path = resolve_config_path(override_path)?;
    let s = std::fs::read_to_string(&path).with_context(|| {
        format!(
            "reading {} (set up the cloudcode client config first)",
            path.display()
        )
    })?;
    parse_config(&s).with_context(|| format!("parsing {}", path.display()))
}

/// Split out so the TOML parsing is unit-testable without touching the
/// filesystem.
fn parse_config(s: &str) -> Result<HubConfig> {
    Ok(toml::from_str(s)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Run `f` with `XDG_CONFIG_HOME` / `HOME` forced to known values so
    /// the path-resolution tests are deterministic regardless of the
    /// machine they run on. Env is process-global, so keep these serial
    /// by routing both through one test.
    fn with_env<T>(xdg: Option<&str>, home: Option<&str>, f: impl FnOnce() -> T) -> T {
        let prev_xdg = std::env::var_os("XDG_CONFIG_HOME");
        let prev_home = std::env::var_os("HOME");
        match xdg {
            Some(v) => std::env::set_var("XDG_CONFIG_HOME", v),
            None => std::env::remove_var("XDG_CONFIG_HOME"),
        }
        match home {
            Some(v) => std::env::set_var("HOME", v),
            None => std::env::remove_var("HOME"),
        }
        let out = f();
        match prev_xdg {
            Some(v) => std::env::set_var("XDG_CONFIG_HOME", v),
            None => std::env::remove_var("XDG_CONFIG_HOME"),
        }
        match prev_home {
            Some(v) => std::env::set_var("HOME", v),
            None => std::env::remove_var("HOME"),
        }
        out
    }

    #[test]
    fn path_logic_and_parsing() {
        // XDG_CONFIG_HOME wins when set.
        with_env(Some("/tmp/xdg"), Some("/home/u"), || {
            let p = default_config_path().unwrap();
            assert_eq!(p, PathBuf::from("/tmp/xdg/cloudcode/config.toml"));
        });

        // Falls back to $HOME/.config when XDG is unset.
        with_env(None, Some("/home/u"), || {
            let p = default_config_path().unwrap();
            assert_eq!(p, PathBuf::from("/home/u/.config/cloudcode/config.toml"));
        });

        // Empty XDG is treated as unset.
        with_env(Some(""), Some("/home/u"), || {
            let p = default_config_path().unwrap();
            assert_eq!(p, PathBuf::from("/home/u/.config/cloudcode/config.toml"));
        });
    }

    #[test]
    fn override_path_takes_precedence() {
        let custom = PathBuf::from("/etc/cc/config.toml");
        let resolved = resolve_config_path(Some(custom.as_path())).unwrap();
        assert_eq!(resolved, custom);
    }

    #[test]
    fn parses_valid_config() {
        let cfg = parse_config("hub_url = \"http://localhost:7100\"\ntoken = \"cc_abc\"\n").unwrap();
        assert_eq!(cfg.hub_url, "http://localhost:7100");
        assert_eq!(cfg.token, "cc_abc");
    }

    #[test]
    fn missing_field_errors() {
        assert!(parse_config("hub_url = \"http://x\"\n").is_err());
    }

    #[test]
    fn missing_config_file_errors() {
        // A path that definitely doesn't exist should surface a read error.
        let res = load_config(Some(Path::new("/nonexistent/cloudcode/config.toml")));
        assert!(res.is_err());
    }
}
