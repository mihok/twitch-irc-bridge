//! IRC backend for slashcheer-irc (Ergo behind a Cloudflare Worker), not implemented yet.
//!
//! Transport: IRC over WebSocket (`text.ircv3.net`) to `irc.url`, authenticated at the edge
//! with a Cloudflare Access service token (`CF_ACCESS_CLIENT_ID` / `CF_ACCESS_CLIENT_SECRET`).
//! The Worker's IrcGate logs each connection in as the account bound to that token.
//!
//! - Control connection: the account bound with `irc-admin.sh bind-bot`; joins `irc.channel`,
//!   needs channel-op to set the topic and `+m`, reports PRIVMSG/JOIN as [`crate::bridge::IrcEvent`]s.
//! - Puppet connections: one websocket per active Twitch viewer. This needs a puppet mode in
//!   slashcheer-irc's Worker (see DESIGN.md, "slashcheer-irc changes"): the bridge's token may
//!   ask to be logged in as an account in the reserved puppet namespace.
//! - Lines longer than the server's limit must be split here (Twitch allows 500 characters,
//!   which can exceed 512 bytes).

use anyhow::{Result, bail};
use tokio::sync::mpsc;

use crate::{
    bridge::{Event, IrcOut},
    config::IrcConfig,
};

pub struct IrcClient {
    _cfg: IrcConfig,
}

impl IrcClient {
    /// Opens the control connection and starts forwarding events into `tx`.
    pub async fn connect(cfg: IrcConfig, _tx: mpsc::Sender<Event>) -> Result<Self> {
        let _ = Self { _cfg: cfg };
        bail!("the IRC backend is not implemented yet")
    }
}

const TODO: &str = "the IRC backend is not implemented yet";

impl IrcOut for IrcClient {
    async fn puppet_connect(&mut self, _nick: &str, _realname: &str) -> Result<()> {
        bail!(TODO)
    }
    async fn puppet_rename(&mut self, _old: &str, _new: &str) -> Result<()> {
        bail!(TODO)
    }
    async fn puppet_say(&mut self, _nick: &str, _text: &str, _action: bool) -> Result<()> {
        bail!(TODO)
    }
    async fn puppet_quit(&mut self, _nick: &str, _reason: &str) -> Result<()> {
        bail!(TODO)
    }
    async fn say(&mut self, _text: &str) -> Result<()> {
        bail!(TODO)
    }
    async fn notice(&mut self, _target: &str, _text: &str) -> Result<()> {
        bail!(TODO)
    }
    async fn set_topic(&mut self, _topic: &str) -> Result<()> {
        bail!(TODO)
    }
    async fn set_moderated(&mut self, _on: bool) -> Result<()> {
        bail!(TODO)
    }
}
