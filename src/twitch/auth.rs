//! Twitch OAuth tokens.
//!
//! - **User token** for the bot account, from a refresh token. Needs the scopes
//!   `user:read:chat user:write:chat user:bot`. Used for EventSub (WebSocket subscriptions
//!   require a user token), lookups, and sending when `send_as = "user"`.
//! - **App token** (client credentials), used to send chat when `send_as = "app"` so messages
//!   carry the Chat Bot badge.

use std::{
    path::PathBuf,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use tokio::sync::Mutex;
use tracing::{info, warn};

/// Refresh this long before a token expires.
const EXPIRY_MARGIN: Duration = Duration::from_secs(300);

#[derive(Clone)]
pub struct Credentials {
    pub client_id: String,
    pub client_secret: String,
    pub refresh_token: String,
    /// If set, the refresh token is read from and saved back to this file, so a rotated
    /// token survives restarts.
    pub refresh_token_file: Option<PathBuf>,
}

impl std::fmt::Debug for Credentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Credentials")
            .field("client_id", &self.client_id)
            .finish_non_exhaustive()
    }
}

impl Credentials {
    /// `TWITCH_CLIENT_ID`, `TWITCH_CLIENT_SECRET`, and the refresh token: from
    /// `TWITCH_BOT_REFRESH_TOKEN_FILE` if that file exists, otherwise `TWITCH_BOT_REFRESH_TOKEN`
    /// (which is then saved to the file after the first successful refresh).
    pub fn from_env() -> Result<Self> {
        let var = |name: &str| std::env::var(name).with_context(|| format!("{name} is not set"));
        let file = std::env::var_os("TWITCH_BOT_REFRESH_TOKEN_FILE").map(PathBuf::from);
        // The file wins once it exists; until then TWITCH_BOT_REFRESH_TOKEN seeds it.
        let saved = file
            .as_ref()
            .and_then(|path| std::fs::read_to_string(path).ok())
            .map(|t| t.trim().to_owned())
            .filter(|t| !t.is_empty());
        let refresh_token = match saved {
            Some(token) => token,
            None => var("TWITCH_BOT_REFRESH_TOKEN").context(
                "set TWITCH_BOT_REFRESH_TOKEN (or put the token in TWITCH_BOT_REFRESH_TOKEN_FILE)",
            )?,
        };
        Ok(Self {
            client_id: var("TWITCH_CLIENT_ID")?,
            client_secret: var("TWITCH_CLIENT_SECRET")?,
            refresh_token,
            refresh_token_file: file,
        })
    }
}

#[derive(Debug, Clone)]
struct Cached {
    token: String,
    expires: Instant,
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    expires_in: u64,
    refresh_token: Option<String>,
}

pub struct Tokens {
    http: reqwest::Client,
    auth_url: String,
    creds: Credentials,
    refresh_token: Mutex<String>,
    user: Mutex<Option<Cached>>,
    app: Mutex<Option<Cached>>,
}

impl Tokens {
    pub fn new(http: reqwest::Client, auth_url: &str, creds: Credentials) -> Self {
        let refresh_token = Mutex::new(creds.refresh_token.clone());
        Self {
            http,
            auth_url: auth_url.trim_end_matches('/').to_owned(),
            creds,
            refresh_token,
            user: Mutex::new(None),
            app: Mutex::new(None),
        }
    }

    pub fn client_id(&self) -> &str {
        &self.creds.client_id
    }

    pub async fn user_token(&self) -> Result<String> {
        let mut cached = self.user.lock().await;
        if let Some(c) = cached
            .as_ref()
            .filter(|c| Instant::now() + EXPIRY_MARGIN < c.expires)
        {
            return Ok(c.token.clone());
        }
        let mut refresh = self.refresh_token.lock().await;
        let resp = self
            .token_request(&[
                ("grant_type", "refresh_token"),
                ("refresh_token", refresh.as_str()),
            ])
            .await
            .context(
                "refreshing the bot's user token (is TWITCH_BOT_REFRESH_TOKEN still valid?)",
            )?;
        let current = resp.refresh_token.unwrap_or_else(|| refresh.clone());
        if current != *refresh {
            info!("Twitch rotated the bot's refresh token");
            if self.creds.refresh_token_file.is_none() {
                warn!(
                    "set TWITCH_BOT_REFRESH_TOKEN_FILE so rotated refresh tokens survive restarts"
                );
            }
        }
        // Save it whenever the file is missing or stale (first run seeds it from the env var).
        if let Some(path) = &self.creds.refresh_token_file
            && std::fs::read_to_string(path).unwrap_or_default().trim() != current
            && let Err(e) = std::fs::write(path, &current)
        {
            warn!("couldn't save the refresh token to {}: {e}", path.display());
        }
        *refresh = current;
        let c = Cached {
            token: resp.access_token,
            expires: Instant::now() + Duration::from_secs(resp.expires_in),
        };
        *cached = Some(c.clone());
        Ok(c.token)
    }

    pub async fn app_token(&self) -> Result<String> {
        let mut cached = self.app.lock().await;
        if let Some(c) = cached
            .as_ref()
            .filter(|c| Instant::now() + EXPIRY_MARGIN < c.expires)
        {
            return Ok(c.token.clone());
        }
        let resp = self
            .token_request(&[("grant_type", "client_credentials")])
            .await
            .context("getting an app access token")?;
        let c = Cached {
            token: resp.access_token,
            expires: Instant::now() + Duration::from_secs(resp.expires_in),
        };
        *cached = Some(c.clone());
        Ok(c.token)
    }

    /// Forgets a token the API rejected, so the next call fetches a new one.
    pub async fn invalidate(&self, app: bool) {
        if app {
            *self.app.lock().await = None;
        } else {
            *self.user.lock().await = None;
        }
    }

    async fn token_request(&self, params: &[(&str, &str)]) -> Result<TokenResponse> {
        let mut form = vec![
            ("client_id", self.creds.client_id.as_str()),
            ("client_secret", self.creds.client_secret.as_str()),
        ];
        form.extend_from_slice(params);
        let resp = self
            .http
            .post(format!("{}/token", self.auth_url))
            .form(&form)
            .send()
            .await?;
        let status = resp.status();
        if !status.is_success() {
            bail!("{status}: {}", resp.text().await.unwrap_or_default());
        }
        Ok(resp.json().await?)
    }
}
