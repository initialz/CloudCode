use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::RwLock;

/// On-disk shape of `~/.claude/.credentials.json`, as written by claude code.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CredentialsFile {
    #[serde(rename = "claudeAiOauth")]
    pub claude_ai_oauth: OAuthCredentials,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OAuthCredentials {
    #[serde(rename = "accessToken")]
    pub access_token: String,
    #[serde(rename = "refreshToken")]
    pub refresh_token: String,
    /// Unix epoch in milliseconds.
    #[serde(rename = "expiresAt")]
    pub expires_at: i64,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub scopes: Vec<String>,
    #[serde(rename = "subscriptionType", default, skip_serializing_if = "Option::is_none")]
    pub subscription_type: Option<String>,
}

pub struct CredentialsStore {
    path: PathBuf,
    inner: RwLock<OAuthCredentials>,
}

impl CredentialsStore {
    pub fn load(path: PathBuf) -> Result<Self> {
        let creds = read_file(&path)?;
        Ok(Self {
            path,
            inner: RwLock::new(creds.claude_ai_oauth),
        })
    }

    pub fn snapshot(&self) -> OAuthCredentials {
        self.inner.read().unwrap().clone()
    }

    pub fn replace(&self, new_creds: OAuthCredentials) -> Result<()> {
        {
            let mut guard = self.inner.write().unwrap();
            *guard = new_creds.clone();
        }
        write_file(
            &self.path,
            &CredentialsFile {
                claude_ai_oauth: new_creds,
            },
        )
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

fn read_file(path: &Path) -> Result<CredentialsFile> {
    let s = std::fs::read_to_string(path)
        .with_context(|| format!("reading credentials file {}", path.display()))?;
    Ok(serde_json::from_str(&s)
        .with_context(|| format!("parsing credentials file {}", path.display()))?)
}

fn write_file(path: &Path, creds: &CredentialsFile) -> Result<()> {
    let s = serde_json::to_string_pretty(creds)?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, s)
        .with_context(|| format!("writing {}", tmp.display()))?;
    std::fs::rename(&tmp, path)
        .with_context(|| format!("renaming {} -> {}", tmp.display(), path.display()))?;
    Ok(())
}
