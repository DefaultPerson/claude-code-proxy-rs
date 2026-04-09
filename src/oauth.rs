//! OAuth token management for native mode.
//!
//! Port of cc-gateway/src/oauth.ts.
//! Loads OAuth tokens from config, caches them, and auto-refreshes
//! before expiration via platform.claude.com.

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::RwLock;
use tracing::{error, info};

use crate::config::OAuthConfig;

const TOKEN_URL: &str = "https://platform.claude.com/v1/oauth/token";
const CLIENT_ID: &str = "9d1c250a-e61b-44d9-88ed-5944d1962f5e";
const SCOPES: &str =
    "user:inference user:profile user:sessions:claude_code user:mcp_servers user:file_upload";

struct OAuthTokens {
    access_token: String,
    refresh_token: String,
    expires_at: u64, // ms since epoch
}

pub struct CredentialStore {
    tokens: Arc<RwLock<OAuthTokens>>,
    http: reqwest::Client,
}

impl CredentialStore {
    /// Initialize from config. Uses existing access_token if valid.
    pub fn new(config: &OAuthConfig) -> Result<Arc<Self>, String> {
        let expires_at = config.expires_at.unwrap_or(0);
        let access_token = config.access_token.clone().unwrap_or_default();

        let store = Arc::new(Self {
            tokens: Arc::new(RwLock::new(OAuthTokens {
                access_token,
                refresh_token: config.refresh_token.clone(),
                expires_at,
            })),
            http: reqwest::Client::new(),
        });

        Ok(store)
    }

    /// Initialize and validate token, refreshing if needed.
    pub async fn init(self: &Arc<Self>) -> Result<(), String> {
        let now = now_ms();
        let five_min = 5 * 60 * 1000;

        let tokens = self.tokens.read().await;
        let has_token = !tokens.access_token.is_empty();
        let valid = tokens.expires_at > now + five_min;
        drop(tokens);

        if has_token && valid {
            let tokens = self.tokens.read().await;
            let remaining = (tokens.expires_at.saturating_sub(now)) / 60_000;
            info!("Using existing access token (expires in {remaining} min)");
        } else {
            if has_token {
                info!("Access token expired, refreshing...");
            } else {
                info!("No access token provided, refreshing...");
            }
            self.refresh().await.map_err(|e| format!("Initial token refresh failed: {e}"))?;
        }

        Ok(())
    }

    /// Get current access token if valid.
    pub async fn get_access_token(&self) -> Result<String, String> {
        let tokens = self.tokens.read().await;
        if tokens.access_token.is_empty() {
            return Err("No OAuth token available".to_string());
        }
        if now_ms() >= tokens.expires_at {
            return Err("OAuth token expired, waiting for refresh".to_string());
        }
        Ok(tokens.access_token.clone())
    }

    /// Refresh the OAuth token via platform.claude.com.
    pub async fn refresh(&self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let refresh_token = {
            let tokens = self.tokens.read().await;
            tokens.refresh_token.clone()
        };

        let body = serde_json::json!({
            "grant_type": "refresh_token",
            "refresh_token": refresh_token,
            "client_id": CLIENT_ID,
            "scope": SCOPES,
        });

        let resp = self
            .http
            .post(TOKEN_URL)
            .json(&body)
            .send()
            .await?;

        let status = resp.status();
        let resp_body: serde_json::Value = resp.json().await?;

        if !status.is_success() {
            return Err(format!("OAuth refresh failed ({status}): {resp_body}").into());
        }

        let access_token = resp_body["access_token"]
            .as_str()
            .ok_or("Missing access_token in response")?
            .to_string();

        let new_refresh = resp_body["refresh_token"]
            .as_str()
            .map(|s| s.to_string())
            .unwrap_or(refresh_token);

        let expires_in = resp_body["expires_in"].as_u64().unwrap_or(3600);
        let expires_at = now_ms() + expires_in * 1000;

        let mut tokens = self.tokens.write().await;
        tokens.access_token = access_token;
        tokens.refresh_token = new_refresh;
        tokens.expires_at = expires_at;

        info!(
            "OAuth token refreshed, expires at {}",
            chrono_like_iso(expires_at)
        );

        Ok(())
    }

    /// Start background refresh loop. Refreshes 5 minutes before expiry.
    pub fn start_refresh_loop(self: Arc<Self>) {
        tokio::spawn(async move {
            loop {
                let ms_until_expiry = {
                    let tokens = self.tokens.read().await;
                    tokens.expires_at.saturating_sub(now_ms())
                };

                // Refresh 5 minutes before expiry, minimum 10 seconds
                let refresh_in_ms = ms_until_expiry.saturating_sub(5 * 60 * 1000).max(10_000);
                tokio::time::sleep(Duration::from_millis(refresh_in_ms)).await;

                match self.refresh().await {
                    Ok(()) => {}
                    Err(e) => {
                        error!("OAuth refresh failed: {e}. Retrying in 30s...");
                        tokio::time::sleep(Duration::from_secs(30)).await;
                    }
                }
            }
        });
    }
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}

/// Simple ISO-ish timestamp without pulling in chrono.
fn chrono_like_iso(ms: u64) -> String {
    let secs = ms / 1000;
    // Just show relative minutes for simplicity
    let now = now_ms() / 1000;
    let diff_min = (secs.saturating_sub(now)) / 60;
    format!("in {diff_min} min")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn now_ms_is_reasonable() {
        let now = now_ms();
        // Should be after 2024-01-01 in ms
        assert!(now > 1_704_067_200_000);
    }
}
