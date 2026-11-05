//! Configuration file (TOML). Secrets are never stored here; they come from
//! environment variables (see `config.example.toml`).

use std::{path::Path, str::FromStr, time::Duration};

use anyhow::{Context, Result, bail};
use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub twitch: TwitchConfig,
    pub irc: IrcConfig,
    #[serde(default)]
    pub lifecycle: LifecycleConfig,
    #[serde(default)]
    pub puppets: PuppetConfig,
    #[serde(default)]
    pub relay: RelayConfig,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TwitchConfig {
    /// Broadcaster login, e.g. `ourshow`.
    pub channel: String,
    /// Login of the bot account that posts IRC messages into Twitch chat.
    pub bot_login: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IrcConfig {
    /// IRC-over-WebSocket endpoint, e.g. `wss://irc.slashcheer.com/web/irc`.
    pub url: String,
    /// The bridged channel, e.g. `#stream`.
    pub channel: String,
    /// Nick of the bridge's own control connection.
    #[serde(default = "default_control_nick")]
    pub control_nick: String,
    /// Upper bound for nick length; the server's ISUPPORT `NICKLEN` wins if lower.
    #[serde(default = "default_nicklen")]
    pub nicklen: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Trigger {
    /// Follow Twitch `stream.online` / `stream.offline`.
    Auto,
    /// Only IRC ops start/stop the bridge.
    Manual,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct LifecycleConfig {
    pub trigger: Trigger,
    #[serde(with = "humantime_serde")]
    pub grace_period: Duration,
    pub offline_moderated: bool,
    pub live_topic: String,
    pub offline_topic: String,
    pub join_notice: String,
}

impl Default for LifecycleConfig {
    fn default() -> Self {
        Self {
            trigger: Trigger::Auto,
            grace_period: Duration::from_secs(10 * 60),
            offline_moderated: true,
            live_topic: "🔴 LIVE: everything said here is shown publicly on Twitch and kept in the VOD chat replay".into(),
            offline_topic: "📴 Bridge offline: nothing here is relayed".into(),
            join_notice: "Heads up: while we're live, messages in this channel are public on Twitch and archived."
                .into(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NameSource {
    /// Use the display name when it only differs from the login by case, otherwise the login.
    DisplayIfAscii,
    /// Always use the (lowercase) login.
    Login,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct PuppetConfig {
    /// Must contain `{name}`.
    pub nick_template: String,
    pub name_source: NameSource,
    /// Placeholders: `{login}`, `{display}`, `{id}`.
    pub realname_template: String,
    #[serde(with = "humantime_serde")]
    pub idle_timeout: Duration,
    pub max_puppets: usize,
    pub quit_message: String,
}

impl Default for PuppetConfig {
    fn default() -> Self {
        Self {
            nick_template: "{name}|tw".into(),
            name_source: NameSource::DisplayIfAscii,
            realname_template: "Twitch viewer: https://twitch.tv/{login}".into(),
            idle_timeout: Duration::from_secs(20 * 60),
            max_puppets: 200,
            quit_message: "Stream ended".into(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Overflow {
    Split,
    Truncate,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct RelayConfig {
    /// Placeholders: `{nick}`, `{text}`.
    pub to_twitch_format: String,
    pub to_twitch_action_format: String,
    /// Used only when `max_puppets` is reached. Placeholders: `{name}`, `{text}`.
    pub to_irc_format: String,
    pub rewrite_mentions: bool,
    pub max_len: usize,
    pub overflow: Overflow,
    pub rate_limit: RateLimitSpec,
    pub relay_links: bool,
}

impl Default for RelayConfig {
    fn default() -> Self {
        Self {
            to_twitch_format: "[irc] {nick}: {text}".into(),
            to_twitch_action_format: "[irc] * {nick} {text}".into(),
            to_irc_format: "<{name}> {text}".into(),
            rewrite_mentions: true,
            max_len: 500,
            overflow: Overflow::Split,
            rate_limit: RateLimitSpec {
                count: 20,
                per: Duration::from_secs(30),
            },
            relay_links: false,
        }
    }
}

/// `"<count>/<duration>"`, e.g. `"20/30s"`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(try_from = "String")]
pub struct RateLimitSpec {
    pub count: u32,
    pub per: Duration,
}

impl FromStr for RateLimitSpec {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self> {
        let (count, per) = s
            .split_once('/')
            .context("rate limit must look like \"20/30s\"")?;
        let count: u32 = count.trim().parse().context("rate limit count")?;
        let per = humantime::parse_duration(per.trim()).context("rate limit window")?;
        if count == 0 || per.is_zero() {
            bail!("rate limit count and window must be non-zero");
        }
        Ok(Self { count, per })
    }
}

impl TryFrom<String> for RateLimitSpec {
    type Error = anyhow::Error;
    fn try_from(s: String) -> Result<Self> {
        s.parse()
    }
}

fn default_control_nick() -> String {
    "twbridge".into()
}

fn default_nicklen() -> usize {
    32
}

impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        let raw =
            std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        Self::parse(&raw).with_context(|| format!("in {}", path.display()))
    }

    pub fn parse(raw: &str) -> Result<Self> {
        let cfg: Config = toml::from_str(raw)?;
        cfg.validate()?;
        Ok(cfg)
    }

    fn validate(&self) -> Result<()> {
        if !self.irc.channel.starts_with('#') {
            bail!("irc.channel must start with '#'");
        }
        if !(self.irc.url.starts_with("wss://") || self.irc.url.starts_with("ws://")) {
            bail!("irc.url must be a ws:// or wss:// URL");
        }
        if !self.puppets.nick_template.contains("{name}") {
            bail!("puppets.nick_template must contain {{name}}");
        }
        crate::naming::validate_template(&self.puppets.nick_template, self.irc.nicklen)?;
        if self.relay.max_len < 50 {
            bail!("relay.max_len is unreasonably small");
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MINIMAL: &str = r##"
        [twitch]
        channel = "ourshow"
        bot_login = "ourshow_irc"

        [irc]
        url = "wss://irc.example.org/web/irc"
        channel = "#stream"
    "##;

    #[test]
    fn minimal_config_gets_defaults() {
        let cfg = Config::parse(MINIMAL).unwrap();
        assert_eq!(cfg.puppets.nick_template, "{name}|tw");
        assert_eq!(
            cfg.relay.rate_limit,
            RateLimitSpec {
                count: 20,
                per: Duration::from_secs(30)
            }
        );
        assert_eq!(cfg.lifecycle.trigger, Trigger::Auto);
    }

    #[test]
    fn example_config_parses() {
        Config::parse(include_str!("../config.example.toml")).unwrap();
    }

    #[test]
    fn rejects_template_without_name() {
        let raw = format!("{MINIMAL}\n[puppets]\nnick_template = \"tw\"\n");
        assert!(Config::parse(&raw).is_err());
    }

    #[test]
    fn rate_limit_parsing() {
        assert_eq!("100/30s".parse::<RateLimitSpec>().unwrap().count, 100);
        assert!("0/30s".parse::<RateLimitSpec>().is_err());
        assert!("20".parse::<RateLimitSpec>().is_err());
    }
}
