use std::{path::PathBuf, time::Duration};

use anyhow::{Context, Result, bail};
use tokio::sync::mpsc;
use tracing_subscriber::EnvFilter;
use twitch_irc_bridge::{
    bridge::{self, Bridge},
    config::Config,
    irc::{IrcClient, conn::AccessToken},
    lifecycle::Saved,
    state::StateStore,
    twitch::{TwitchClient, auth::Credentials},
};

const USAGE: &str = "usage: twitch-irc-bridge <check|run> <config.toml>\n\
     (if BRIDGE_CONFIG is set, its TOML is used instead of the file)";

/// Where the config comes from: `BRIDGE_CONFIG` (TOML in an environment variable, for hosts
/// such as Cloudflare Containers that pass env vars but don't mount files) or the file.
fn load_config(path: &std::path::Path) -> Result<(Config, String)> {
    match std::env::var("BRIDGE_CONFIG") {
        Ok(raw) if !raw.trim().is_empty() => Ok((
            Config::parse(&raw).context("parsing BRIDGE_CONFIG")?,
            "BRIDGE_CONFIG".to_owned(),
        )),
        _ => Ok((Config::load(path)?, path.display().to_string())),
    }
}

/// Saves state changes in the background, latest first, retrying failures, so a slow or
/// unreachable store never holds up relaying.
fn spawn_state_writer(store: StateStore) -> mpsc::UnboundedSender<Saved> {
    let (tx, mut rx) = mpsc::unbounded_channel::<Saved>();
    tokio::spawn(async move {
        while let Some(mut latest) = rx.recv().await {
            while let Ok(newer) = rx.try_recv() {
                latest = newer;
            }
            let mut delay = Duration::from_secs(1);
            while let Err(e) = store.save(&latest).await {
                tracing::warn!("couldn't save the bridge state: {e:#}; retrying in {delay:?}");
                tokio::time::sleep(delay).await;
                delay = (delay * 2).min(Duration::from_secs(60));
                while let Ok(newer) = rx.try_recv() {
                    latest = newer;
                }
            }
        }
    });
    tx
}

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
    let (cfg, source) = load_config(&path)?;

    match cmd.as_str() {
        "check" => {
            println!(
                "{} OK: twitch.tv/{} <-> {} ({})",
                source,
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
            tracing::info!(config = %source, "starting");
            let (tx, rx) = mpsc::channel(1024);
            // IRC first: the control connection then queues the topic/mode changes that
            // Twitch's initial live-state sync can trigger.
            let access = AccessToken::from_env()?;
            let twitch_creds = Credentials::from_env()?;
            let store = StateStore::from_env();
            let saved = match store.load().await {
                Ok(saved) => saved,
                Err(e) => {
                    tracing::warn!("couldn't load the saved bridge state, starting off: {e:#}");
                    None
                }
            };
            let irc = IrcClient::start(cfg.irc.clone(), access, tx.clone());
            let twitch = TwitchClient::start(cfg.twitch.clone(), twitch_creds, tx)
                .await
                .context("setting up the Twitch client")?;
            let mut bridge = Bridge::new(cfg, twitch, irc);
            if store.is_persistent() {
                bridge = bridge.with_persistence(spawn_state_writer(store));
            }
            if let Some(saved) = saved {
                bridge.restore(&saved, std::time::Instant::now()).await?;
            }
            bridge::run(bridge, rx, Duration::from_secs(30)).await?;
            // Give puppet and control connections a moment to send their QUITs.
            tokio::time::sleep(Duration::from_secs(1)).await;
            Ok(())
        }
        _ => bail!(USAGE),
    }
}
