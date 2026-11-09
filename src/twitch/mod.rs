//! Twitch backend.
//!
//! - **Read:** EventSub over WebSocket (outbound only, nothing exposed publicly):
//!   `channel.chat.message`, `channel.chat.clear_user_messages`, `stream.online`,
//!   `stream.offline`. See [`eventsub`].
//! - **Write:** Helix Send Chat Message as `twitch.bot_login`. With `send_as = "app"` (the
//!   default) messages use an app access token and carry the Chat Bot badge; if Twitch refuses
//!   that (the broadcaster hasn't granted `channel:bot` and the bot isn't a moderator), the
//!   bridge sends with the bot's own token instead.
//! - **Secrets** come from the environment: `TWITCH_CLIENT_ID`, `TWITCH_CLIENT_SECRET`, and
//!   `TWITCH_BOT_REFRESH_TOKEN` or `TWITCH_BOT_REFRESH_TOKEN_FILE`. See [`auth`].
//!
//! Connecting never fails startup: looking up the accounts is retried in the background (so
//! a Twitch outage or a bad token doesn't crash-loop the process and churn the IRC side), and
//! `!bridge status` shows what's wrong.

pub mod auth;
pub mod eventsub;
pub mod helix;

use std::{
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use anyhow::{Context, Result, anyhow};
use tokio::sync::mpsc;
use tracing::{info, warn};

use self::{
    auth::{Credentials, Tokens},
    eventsub::EventSub,
    helix::{Helix, HttpError},
};
use crate::{
    bridge::{Event, TwitchOut},
    config::{SendAs, TwitchConfig},
};

/// What `!bridge status` says about Twitch.
#[derive(Debug, Clone, Default)]
pub struct Health(Arc<Mutex<HealthState>>);

#[derive(Debug, Default)]
struct HealthState {
    /// Problem looking up the accounts (the client isn't usable until that works).
    setup: Option<String>,
    /// EventSub: `None` until the first attempt, then subscribed or the last error.
    reading: Option<Result<(), String>>,
    /// Sending with the bot's user token because the app token was refused.
    sending_as_user: bool,
}

impl Health {
    fn state(&self) -> std::sync::MutexGuard<'_, HealthState> {
        self.0.lock().expect("health lock")
    }

    pub(crate) fn setup(&self, problem: Option<String>) {
        self.state().setup = problem;
    }

    pub(crate) fn reading(&self, r: Result<(), String>) {
        self.state().reading = Some(r);
    }

    pub fn line(&self) -> String {
        let s = self.state();
        if let Some(problem) = &s.setup {
            return format!("not connected ({problem})");
        }
        let mut line = match &s.reading {
            None => "connecting".to_string(),
            Some(Ok(())) => "connected".to_string(),
            Some(Err(e)) => format!("not receiving chat ({e})"),
        };
        if s.sending_as_user {
            line.push_str(", sending as the bot (no Chat Bot badge: make the bot a moderator or have the broadcaster grant channel:bot)");
        }
        line
    }
}

struct Ids {
    broadcaster: String,
    bot: String,
}

pub struct TwitchClient {
    helix: Arc<Helix>,
    ids: Arc<OnceLock<Ids>>,
    as_app: Arc<AtomicBool>,
    health: Health,
    /// For `!bridge status`: which channel, as which account.
    channel: String,
    bot_login: String,
}

impl TwitchClient {
    /// Starts connecting with credentials from the environment.
    pub async fn connect(cfg: TwitchConfig, events: mpsc::Sender<Event>) -> Result<Self> {
        Self::start(cfg, Credentials::from_env()?, events).await
    }

    /// Looks up the channel and bot (retrying in the background until it works), then runs
    /// EventSub, which reconnects on its own.
    pub async fn start(
        cfg: TwitchConfig,
        creds: Credentials,
        events: mpsc::Sender<Event>,
    ) -> Result<Self> {
        let http = reqwest::Client::builder()
            .user_agent(concat!("twitch-irc-bridge/", env!("CARGO_PKG_VERSION")))
            .timeout(Duration::from_secs(20))
            .build()?;
        let tokens = Arc::new(Tokens::new(http.clone(), &cfg.auth_url, creds));
        let helix = Arc::new(Helix::new(http, &cfg.api_url, tokens));
        let ids = Arc::new(OnceLock::new());
        let health = Health::default();

        let as_app = Arc::new(AtomicBool::new(cfg.send_as == SendAs::App));
        let (channel, bot_login) = (cfg.channel.clone(), cfg.bot_login.clone());
        let (h, i, hl) = (helix.clone(), ids.clone(), health.clone());
        tokio::spawn(async move {
            let mut backoff = Duration::from_secs(2);
            let found = loop {
                match lookup(&h, &cfg).await {
                    Ok(found) => break found,
                    Err(e) => {
                        warn!("Twitch: {e:#}; retrying in {backoff:?}");
                        hl.setup(Some(format!("{e:#}")));
                        tokio::time::sleep(backoff).await;
                        backoff = (backoff * 2).min(Duration::from_secs(300));
                    }
                }
            };
            hl.setup(None);
            info!(broadcaster = %found.broadcaster, bot = %found.bot, "found the Twitch channel and bot");
            let (broadcaster_id, bot_id) = (found.broadcaster.clone(), found.bot.clone());
            let _ = i.set(found);
            EventSub {
                default_url: cfg.eventsub_url.clone(),
                helix: h,
                broadcaster_id,
                bot_id,
                events,
                health: hl,
            }
            .run()
            .await;
        });
        Ok(Self {
            helix,
            ids,
            as_app,
            health,
            channel,
            bot_login,
        })
    }

    async fn ids(&self) -> Result<&Ids> {
        // Right after startup the lookup may still be running; give it a moment.
        for _ in 0..50 {
            if let Some(ids) = self.ids.get() {
                return Ok(ids);
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        Err(anyhow!("not connected to Twitch: {}", self.health.line()))
    }
}

async fn lookup(helix: &Helix, cfg: &TwitchConfig) -> Result<Ids> {
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
    Ok(Ids {
        broadcaster: id_of(&cfg.channel)?,
        bot: id_of(&cfg.bot_login)?,
    })
}

impl TwitchOut for TwitchClient {
    async fn send_chat(&mut self, text: &str) -> Result<()> {
        let ids = self.ids().await?;
        let as_app = self.as_app.load(Ordering::Relaxed);
        match self
            .helix
            .send_message(&ids.broadcaster, &ids.bot, text, as_app)
            .await
        {
            // The app token needs the broadcaster's channel:bot or the bot being a
            // moderator. Without either, the bot's own token still works (no badge).
            // Twitch answers 401 for this (the docs say 403); `call` has already retried
            // the 401 with a fresh app token, so it isn't an expired token.
            Err(e)
                if as_app
                    && e.downcast_ref::<HttpError>()
                        .is_some_and(|h| matches!(h.status, 401 | 403)) =>
            {
                warn!(
                    "Twitch refused the app token ({e:#}); sending as the bot's user token from now on. \
                     For the Chat Bot badge, make the bot a moderator or have the broadcaster grant channel:bot."
                );
                self.as_app.store(false, Ordering::Relaxed);
                self.health.state().sending_as_user = true;
                self.helix
                    .send_message(&ids.broadcaster, &ids.bot, text, false)
                    .await
            }
            r => r,
        }
    }

    fn status(&self) -> String {
        format!(
            "{} (twitch.tv/{} as {})",
            self.health.line(),
            self.channel,
            self.bot_login
        )
    }
}

#[cfg(test)]
pub(crate) mod tests;
