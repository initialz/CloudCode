//! Background OAuth refresh loop.
//!
//! Claude Code's OAuth setup is not officially documented; the token
//! endpoint, client_id, and request shape below are derived from the
//! claude code client and may break if Anthropic changes them.
//!
//! Strategy: every minute, check the credentials snapshot. If access_token
//! expires within REFRESH_LEAD_SECS, refresh it and write back to disk so
//! the rotation stays in sync with the local claude tool.

use crate::credentials::{CredentialsStore, OAuthCredentials};
use anyhow::{Context, Result};
use serde::Deserialize;
use std::sync::Arc;
use std::time::Duration;

const CLAUDE_OAUTH_CLIENT_ID: &str = "9d1c250a-e61b-44d9-88ed-5944d1962f5e";
const REFRESH_URL: &str = "https://console.anthropic.com/v1/oauth/token";
const REFRESH_LEAD_SECS: i64 = 5 * 60; // refresh when <5min remaining
const CHECK_INTERVAL: Duration = Duration::from_secs(60);

pub fn spawn(store: Arc<CredentialsStore>, http: reqwest::Client) {
    tokio::spawn(async move {
        loop {
            if let Err(e) = tick(&store, &http).await {
                tracing::warn!(error = %e, "credentials refresh tick failed");
            }
            tokio::time::sleep(CHECK_INTERVAL).await;
        }
    });
}

async fn tick(store: &CredentialsStore, http: &reqwest::Client) -> Result<()> {
    let snap = store.snapshot();
    let now_ms = chrono::Utc::now().timestamp_millis();
    let remaining_secs = (snap.expires_at - now_ms) / 1000;

    if remaining_secs > REFRESH_LEAD_SECS {
        return Ok(()); // still fresh
    }

    tracing::info!(
        remaining_secs,
        "access_token nearing expiry; refreshing"
    );

    let refreshed = refresh_once(http, &snap.refresh_token).await?;
    let new_creds = OAuthCredentials {
        access_token: refreshed.access_token,
        refresh_token: refreshed
            .refresh_token
            .unwrap_or_else(|| snap.refresh_token.clone()),
        expires_at: chrono::Utc::now().timestamp_millis()
            + (refreshed.expires_in.unwrap_or(3600) as i64) * 1000,
        scopes: snap.scopes,
        subscription_type: snap.subscription_type,
    };
    store.replace(new_creds)?;
    tracing::info!("credentials refreshed and persisted");
    Ok(())
}

#[derive(Debug, Deserialize)]
struct RefreshResponse {
    access_token: String,
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default)]
    expires_in: Option<u64>,
}

async fn refresh_once(http: &reqwest::Client, refresh_token: &str) -> Result<RefreshResponse> {
    let body = serde_json::json!({
        "grant_type": "refresh_token",
        "refresh_token": refresh_token,
        "client_id": CLAUDE_OAUTH_CLIENT_ID,
    });

    let resp = http
        .post(REFRESH_URL)
        .header("content-type", "application/json")
        .json(&body)
        .send()
        .await
        .context("calling OAuth refresh endpoint")?;

    let status = resp.status();
    let bytes = resp
        .bytes()
        .await
        .context("reading OAuth refresh response body")?;

    if !status.is_success() {
        let snippet = String::from_utf8_lossy(&bytes);
        anyhow::bail!("OAuth refresh failed: HTTP {} body={}", status, snippet);
    }

    serde_json::from_slice::<RefreshResponse>(&bytes)
        .context("parsing OAuth refresh response as JSON")
}
