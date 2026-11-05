use std::{path::PathBuf, time::Duration};

use anyhow::{Context, Result, bail};
use tokio::sync::mpsc;
use tracing_subscriber::EnvFilter;
use twitch_irc_bridge::{
    bridge::{self, Bridge},
    config::Config,
    irc::IrcClient,
    twitch::TwitchClient,
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
                "{} OK: twitch.tv/{} <-> {} ({}, puppets {})",
                path.display(),
                cfg.twitch.channel,
                cfg.irc.channel,
                match cfg.lifecycle.trigger {
                    twitch_irc_bridge::config::Trigger::Auto => "follows stream",
                    twitch_irc_bridge::config::Trigger::Manual => "manual start/stop",
                },
                if cfg.puppets.enabled { "on" } else { "off" },
            );
            Ok(())
        }
        "run" => {
            let (tx, rx) = mpsc::channel(1024);
            let twitch = TwitchClient::connect(cfg.twitch.clone(), tx.clone())
                .await
                .context("connecting to Twitch")?;
            let irc = IrcClient::connect(cfg.irc.clone(), tx)
                .await
                .context("connecting to IRC")?;
            bridge::run(Bridge::new(cfg, twitch, irc), rx, Duration::from_secs(30)).await
        }
        _ => bail!(USAGE),
    }
}
