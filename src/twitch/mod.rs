//! Twitch backend.
//!
//! - **Read:** EventSub over WebSocket (outbound only, nothing exposed publicly):
//!   `channel.chat.message`, `channel.chat.clear_user_messages`, `stream.online`,
//!   `stream.offline`. See [`eventsub`].
//! - **Write:** Helix Send Chat Message as `twitch.bot_login`. With `send_as = "app"` (the
//!   default) messages use an app access token and carry the Chat Bot badge.
//! - **Secrets** come from the environment: `TWITCH_CLIENT_ID`, `TWITCH_CLIENT_SECRET`, and
//!   `TWITCH_BOT_REFRESH_TOKEN` or `TWITCH_BOT_REFRESH_TOKEN_FILE`. See [`auth`].

pub mod auth;
pub mod eventsub;
pub mod helix;

use std::sync::Arc;

use anyhow::{Context, Result};
use tokio::sync::mpsc;

use self::{auth::Credentials, auth::Tokens, eventsub::EventSub, helix::Helix};
use crate::{
    bridge::{Event, TwitchOut},
    config::{SendAs, TwitchConfig},
};

pub struct TwitchClient {
    helix: Arc<Helix>,
    broadcaster_id: String,
    bot_id: String,
    as_app: bool,
}

impl TwitchClient {
    /// Looks up the channel and bot, then starts EventSub (which reconnects on its own).
    pub async fn connect(cfg: TwitchConfig, events: mpsc::Sender<Event>) -> Result<Self> {
        Self::start(cfg, Credentials::from_env()?, events).await
    }

    pub async fn start(
        cfg: TwitchConfig,
        creds: Credentials,
        events: mpsc::Sender<Event>,
    ) -> Result<Self> {
        let http = reqwest::Client::builder()
            .user_agent(concat!("twitch-irc-bridge/", env!("CARGO_PKG_VERSION")))
            .timeout(std::time::Duration::from_secs(20))
            .build()?;
        let tokens = Arc::new(Tokens::new(http.clone(), &cfg.auth_url, creds));
        let helix = Arc::new(Helix::new(http, &cfg.api_url, tokens));

        let users = helix
            .users(&[&cfg.channel, &cfg.bot_login])
            .await
            .context("looking up the channel and bot accounts")?;
        let id_of = |login: &str| {
            users
                .iter()
                .find(|u| u.login.eq_ignore_ascii_case(login))
                .map(|u| u.id.clone())
                .with_context(|| format!("Twitch account {login} not found"))
        };
        let broadcaster_id = id_of(&cfg.channel)?;
        let bot_id = id_of(&cfg.bot_login)?;

        tokio::spawn(
            EventSub {
                default_url: cfg.eventsub_url.clone(),
                helix: helix.clone(),
                broadcaster_id: broadcaster_id.clone(),
                bot_id: bot_id.clone(),
                events,
            }
            .run(),
        );
        Ok(Self {
            helix,
            broadcaster_id,
            bot_id,
            as_app: cfg.send_as == SendAs::App,
        })
    }
}

impl TwitchOut for TwitchClient {
    async fn send_chat(&mut self, text: &str) -> Result<()> {
        self.helix
            .send_message(&self.broadcaster_id, &self.bot_id, text, self.as_app)
            .await
    }
}

#[cfg(test)]
pub(crate) mod tests;
