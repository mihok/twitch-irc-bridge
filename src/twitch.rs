//! Twitch backend (not implemented yet).
//!
//! Plan:
//! - Read: EventSub over WebSocket (`wss://eventsub.wss.twitch.tv/ws`), outbound only.
//!   Subscriptions: `channel.chat.message`, `stream.online`, `stream.offline`, `channel.ban`.
//!   Each notification becomes a [`crate::bridge::TwitchEvent`].
//! - Write: Helix `POST /helix/chat/messages` as `twitch.bot_login`, with an app access token
//!   (`user:bot` from the bot, `channel:bot` from the broadcaster) so the Chat Bot badge shows.
//! - Secrets come from the environment: `TWITCH_CLIENT_ID`, `TWITCH_CLIENT_SECRET`,
//!   `TWITCH_BOT_REFRESH_TOKEN`.

use anyhow::{Result, bail};
use tokio::sync::mpsc;

use crate::{
    bridge::{Event, TwitchOut},
    config::TwitchConfig,
};

pub struct TwitchClient {
    _cfg: TwitchConfig,
}

impl TwitchClient {
    /// Connects to EventSub and starts forwarding events into `tx`.
    pub async fn connect(cfg: TwitchConfig, _tx: mpsc::Sender<Event>) -> Result<Self> {
        let _ = Self { _cfg: cfg };
        bail!("the Twitch backend is not implemented yet")
    }
}

impl TwitchOut for TwitchClient {
    async fn send_chat(&mut self, _text: &str) -> Result<()> {
        bail!("the Twitch backend is not implemented yet")
    }
}
