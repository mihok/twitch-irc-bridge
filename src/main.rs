use std::{path::PathBuf, time::Duration};

use anyhow::{Context, Result, bail};
use tokio::sync::mpsc;
use tracing_subscriber::EnvFilter;
use twitch_irc_bridge::{
    bridge::{self, Bridge},
    config::Config,
    irc::{IrcClient, conn::AccessToken},
    twitch::{TwitchClient, auth::Credentials},
};

const USAGE: &str = "usage: twitch-irc-bridge <check|run> <config.toml>";

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();

    let mut args = std::env::args().skip(1);
    let (cmd, path) = match (args.next(), args.next()) {
        (Some(cmd), Some(path)) => (cmd, PathBuf::from(path)),
        _ => bail!(USAGE),
    };
    let cfg = Config::load(&path)?;

    match cmd.as_str() {
        "check" => {
            println!(
                "{} OK: twitch.tv/{} <-> {} ({})",
                path.display(),
                cfg.twitch.channel,
                cfg.irc.channel,
                match cfg.lifecycle.trigger {
                    twitch_irc_bridge::config::Trigger::Auto => "follows stream",
                    twitch_irc_bridge::config::Trigger::Manual => "manual start/stop",
                },
            );
            Ok(())
        }
        "run" => {
            let (tx, rx) = mpsc::channel(1024);
            // IRC first: the control connection then queues the topic/mode changes that
            // Twitch's initial live-state sync can trigger.
            let access = AccessToken::from_env()?;
            let twitch_creds = Credentials::from_env()?;
            let irc = IrcClient::start(cfg.irc.clone(), access, tx.clone());
            let twitch = TwitchClient::start(cfg.twitch.clone(), twitch_creds, tx)
                .await
                .context("connecting to Twitch")?;
            bridge::run(Bridge::new(cfg, twitch, irc), rx, Duration::from_secs(30)).await?;
            // Give puppet and control connections a moment to send their QUITs.
            tokio::time::sleep(Duration::from_secs(1)).await;
            Ok(())
        }
        _ => bail!(USAGE),
    }
}
