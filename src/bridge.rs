//! The bridge core: consumes events from both sides and decides what to send.
//! It is transport-agnostic; the Twitch and IRC backends implement [`TwitchOut`]
//! and [`IrcOut`] and feed [`Event`]s in.

use std::time::{Duration, Instant};

use anyhow::Result;
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use crate::{
    config::Config,
    lifecycle::{Lifecycle, Signal, Transition},
    moderation::Moderation,
    naming::TwitchUser,
    puppets::{Ensure, PuppetRegistry},
    ratelimit::RateLimiter,
    relay,
};

#[derive(Debug, Clone)]
pub enum Event {
    Twitch(TwitchEvent),
    Irc(IrcEvent),
    Tick,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TwitchEvent {
    StreamOnline,
    StreamOffline,
    Chat {
        user: TwitchUser,
        text: String,
        action: bool,
    },
    /// A moderator cleared a user's messages (Twitch does this on every ban and
    /// timeout). Twitch already stops them chatting, so the bridge only removes
    /// their puppet.
    UserCleared {
        user_id: String,
    },
    /// Ban (`duration: None`) or timeout with a known duration.
    UserBanned {
        user_id: String,
        duration: Option<Duration>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IrcEvent {
    /// PRIVMSG to the bridged channel (raw body, may contain CTCP ACTION / formatting).
    Message {
        nick: String,
        text: String,
        is_op: bool,
    },
    Join {
        nick: String,
    },
    /// Someone sent a private message to a puppet.
    PuppetDirectMessage {
        from: String,
        puppet: String,
    },
    /// The server's ISUPPORT NICKLEN.
    NickLen(usize),
}

/// Sending side of the Twitch backend.
#[allow(async_fn_in_trait)]
pub trait TwitchOut {
    async fn send_chat(&mut self, text: &str) -> Result<()>;
}

/// Sending side of the IRC backend. Puppet methods address puppets by nick; the backend
/// owns their connections (and must split lines that exceed the server's line limit).
#[allow(async_fn_in_trait)]
pub trait IrcOut {
    /// `remote_id` is the Twitch user ID (sent as `X-Slashcheer-Puppet-Id`).
    async fn puppet_connect(&mut self, nick: &str, remote_id: &str, realname: &str) -> Result<()>;
    async fn puppet_say(&mut self, nick: &str, text: &str, action: bool) -> Result<()>;
    async fn puppet_quit(&mut self, nick: &str, reason: &str) -> Result<()>;
    /// Message from the control connection into the bridged channel.
    async fn say(&mut self, text: &str) -> Result<()>;
    async fn notice(&mut self, target: &str, text: &str) -> Result<()>;
    async fn set_topic(&mut self, topic: &str) -> Result<()>;
    async fn set_moderated(&mut self, on: bool) -> Result<()>;
}

pub struct Bridge<T, I> {
    cfg: Config,
    twitch: T,
    irc: I,
    lifecycle: Lifecycle,
    puppets: PuppetRegistry,
    moderation: Moderation,
    limiter: RateLimiter,
}

impl<T: TwitchOut, I: IrcOut> Bridge<T, I> {
    pub fn new(cfg: Config, twitch: T, irc: I) -> Self {
        let lifecycle = Lifecycle::new(cfg.lifecycle.trigger, cfg.lifecycle.grace_period);
        let puppets = PuppetRegistry::new(
            &cfg.puppets,
            cfg.irc.nicklen,
            &[cfg.irc.control_nick.as_str()],
        );
        let limiter = RateLimiter::new(cfg.relay.rate_limit);
        Self {
            cfg,
            twitch,
            irc,
            lifecycle,
            puppets,
            moderation: Moderation::default(),
            limiter,
        }
    }

    pub fn is_relaying(&self) -> bool {
        self.lifecycle.is_relaying()
    }

    /// Stops relaying and quits all puppets.
    pub async fn shutdown(&mut self, now: Instant) -> Result<()> {
        self.signal(Signal::ManualStop, now).await
    }

    pub async fn handle(&mut self, event: Event, now: Instant) -> Result<()> {
        match event {
            Event::Tick => {
                self.signal(Signal::Tick, now).await?;
                self.moderation.sweep(now);
                for nick in self.puppets.expire(now, self.cfg.puppets.idle_timeout) {
                    self.irc.puppet_quit(&nick, "Idle").await?;
                }
            }
            Event::Twitch(TwitchEvent::StreamOnline) => {
                self.signal(Signal::StreamOnline, now).await?
            }
            Event::Twitch(TwitchEvent::StreamOffline) => {
                self.signal(Signal::StreamOffline, now).await?
            }
            Event::Twitch(TwitchEvent::UserCleared { user_id }) => {
                if let Some(p) = self.puppets.remove(&user_id) {
                    self.irc
                        .puppet_quit(&p.nick, "Removed by Twitch moderators")
                        .await?;
                }
            }
            Event::Twitch(TwitchEvent::UserBanned { user_id, duration }) => {
                self.moderation.block_twitch(&user_id, duration, now);
                if let Some(p) = self.puppets.remove(&user_id) {
                    self.irc
                        .puppet_quit(&p.nick, "Removed by Twitch moderators")
                        .await?;
                }
            }
            Event::Twitch(TwitchEvent::Chat { user, text, action }) => {
                self.twitch_chat(user, text, action, now).await?
            }
            Event::Irc(IrcEvent::Message { nick, text, is_op }) => {
                self.irc_message(nick, text, is_op, now).await?
            }
            Event::Irc(IrcEvent::Join { nick }) => {
                if self.is_relaying() && !self.is_bridge_nick(&nick) {
                    self.irc
                        .notice(&nick, &self.cfg.lifecycle.join_notice)
                        .await?;
                }
            }
            Event::Irc(IrcEvent::PuppetDirectMessage { from, puppet }) => {
                let msg = format!(
                    "{puppet} is a Twitch viewer bridged into {} and can't receive private messages.",
                    self.cfg.irc.channel
                );
                self.irc.notice(&from, &msg).await?;
            }
            Event::Irc(IrcEvent::NickLen(n)) => self.puppets.set_nicklen(n),
        }
        Ok(())
    }

    async fn signal(&mut self, signal: Signal, now: Instant) -> Result<()> {
        match self.lifecycle.handle(signal, now) {
            Some(Transition::WentLive) => {
                info!("bridge live");
                self.irc.set_topic(&self.cfg.lifecycle.live_topic).await?;
                if self.cfg.lifecycle.offline_moderated {
                    self.irc.set_moderated(false).await?;
                }
            }
            Some(Transition::WentOffline) => {
                info!("bridge offline");
                for nick in self.puppets.drain() {
                    self.irc
                        .puppet_quit(&nick, &self.cfg.puppets.quit_message)
                        .await?;
                }
                self.irc
                    .set_topic(&self.cfg.lifecycle.offline_topic)
                    .await?;
                if self.cfg.lifecycle.offline_moderated {
                    self.irc.set_moderated(true).await?;
                }
            }
            None => {}
        }
        Ok(())
    }

    fn is_bridge_nick(&self, nick: &str) -> bool {
        crate::naming::casefold(nick) == crate::naming::casefold(&self.cfg.irc.control_nick)
            || self.puppets.by_nick(nick).is_some()
    }

    async fn twitch_chat(
        &mut self,
        user: TwitchUser,
        text: String,
        action: bool,
        now: Instant,
    ) -> Result<()> {
        if !self.is_relaying() || user.login.eq_ignore_ascii_case(&self.cfg.twitch.bot_login) {
            return Ok(());
        }
        if self.moderation.is_twitch_blocked(&user.id, now) {
            debug!(user = %user.login, "dropping message from blocked Twitch user");
            return Ok(());
        }
        match self.puppets.ensure(&user, now) {
            Ensure::Existing { nick } => return self.irc.puppet_say(&nick, &text, action).await,
            Ensure::Created { nick, realname } => {
                self.irc.puppet_connect(&nick, &user.id, &realname).await?;
                return self.irc.puppet_say(&nick, &text, action).await;
            }
            Ensure::Renamed { old, new, realname } => {
                self.irc.puppet_quit(&old, "Renamed on Twitch").await?;
                self.irc.puppet_connect(&new, &user.id, &realname).await?;
                return self.irc.puppet_say(&new, &text, action).await;
            }
            // Overflow only: relay through the control connection.
            Ensure::Full => warn!(
                "max_puppets reached; relaying {} via the control connection",
                user.login
            ),
        }
        let text = if action { format!("* {text}") } else { text };
        let line = relay::render(&self.cfg.relay.to_irc_format, &user.display_name, &text);
        self.irc.say(&line).await
    }

    async fn irc_message(
        &mut self,
        nick: String,
        raw: String,
        is_op: bool,
        now: Instant,
    ) -> Result<()> {
        if self.is_bridge_nick(&nick) {
            return Ok(());
        }
        if is_op && raw.starts_with('!') {
            return self.command(&nick, &raw, now).await;
        }
        if !self.is_relaying() || self.moderation.is_irc_muted(&nick) {
            return Ok(());
        }
        let (action, body) = relay::parse_action(&raw);
        let mut text = relay::strip_irc_formatting(body);
        if !self.cfg.relay.relay_links {
            text = relay::strip_links(&text);
        }
        if self.cfg.relay.rewrite_mentions {
            text = relay::rewrite_mentions(&text, &self.puppets.mention_map());
        }
        if text.trim().is_empty() {
            return Ok(());
        }
        let template = if action {
            &self.cfg.relay.to_twitch_action_format
        } else {
            &self.cfg.relay.to_twitch_format
        };
        for line in relay::render_bounded(
            template,
            &nick,
            &text,
            self.cfg.relay.max_len,
            self.cfg.relay.overflow,
        ) {
            if !self.limiter.try_acquire(now) {
                warn!(%nick, "Twitch rate limit reached; dropping message");
                self.irc
                    .notice(
                        &nick,
                        "Slow down: the Twitch rate limit was hit and your message wasn't relayed.",
                    )
                    .await?;
                break;
            }
            self.twitch.send_chat(&line).await?;
        }
        Ok(())
    }

    async fn command(&mut self, op: &str, raw: &str, now: Instant) -> Result<()> {
        let parts: Vec<&str> = raw.split_whitespace().collect();
        let reply = match parts.as_slice() {
            ["!bridge", "start"] => {
                self.signal(Signal::ManualStart, now).await?;
                "Bridge started.".to_string()
            }
            ["!bridge", "stop"] => {
                self.signal(Signal::ManualStop, now).await?;
                "Bridge stopped.".to_string()
            }
            ["!bridge", "status"] => format!("State: {:?}, puppets: {}", self.lifecycle.state(), self.puppets.len()),
            ["!tw", "block", nick] => match self.puppets.by_nick(nick).map(|p| p.user.id.clone()) {
                Some(id) => {
                    self.moderation.block_twitch(&id, None, now);
                    if let Some(p) = self.puppets.remove(&id) {
                        self.irc.puppet_quit(&p.nick, "Blocked by IRC operators").await?;
                    }
                    format!("Blocked Twitch user {id} ({nick}).")
                }
                None => format!("{nick} isn't an active Twitch puppet."),
            },
            ["!tw", "unblock", id] => {
                if self.moderation.unblock_twitch(id) { format!("Unblocked Twitch user {id}.") } else { format!("{id} wasn't blocked.") }
            }
            ["!irc", "mute", nick] => {
                self.moderation.mute_irc(nick);
                format!("{nick} is no longer relayed to Twitch.")
            }
            ["!irc", "unmute", nick] => {
                self.moderation.unmute_irc(nick);
                format!("{nick} is relayed to Twitch again.")
            }
            _ => "Commands: !bridge start|stop|status, !tw block <puppet>, !tw unblock <twitch-id>, !irc mute|unmute <nick>".to_string(),
        };
        self.irc.notice(op, &reply).await
    }
}

/// Main loop: feeds events and periodic ticks into the bridge until the channel closes
/// or Ctrl-C is pressed.
pub async fn run<T: TwitchOut, I: IrcOut>(
    mut bridge: Bridge<T, I>,
    mut rx: mpsc::Receiver<Event>,
    tick: Duration,
) -> Result<()> {
    let mut interval = tokio::time::interval(tick);
    let stop = shutdown_signal();
    tokio::pin!(stop);
    loop {
        let event = tokio::select! {
            ev = rx.recv() => match ev { Some(ev) => ev, None => break },
            _ = interval.tick() => Event::Tick,
            _ = &mut stop => {
                info!("shutting down");
                if let Err(err) = bridge.shutdown(Instant::now()).await {
                    warn!("shutdown: {err:#}");
                }
                break;
            }
        };
        if let Err(err) = bridge.handle(event, Instant::now()).await {
            warn!("{err:#}");
        }
    }
    Ok(())
}

/// Resolves on Ctrl-C (SIGINT) or SIGTERM (`docker stop`, container platforms).
async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        match signal(SignalKind::terminate()) {
            Ok(mut term) => {
                tokio::select! {
                    _ = tokio::signal::ctrl_c() => {}
                    _ = term.recv() => {}
                }
            }
            Err(_) => {
                let _ = tokio::signal::ctrl_c().await;
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    #[derive(Default)]
    struct Log(Vec<String>);

    #[derive(Default)]
    struct FakeTwitch(Log);
    impl TwitchOut for FakeTwitch {
        async fn send_chat(&mut self, text: &str) -> Result<()> {
            self.0.0.push(format!("twitch: {text}"));
            Ok(())
        }
    }

    #[derive(Default)]
    struct FakeIrc(Log);
    impl IrcOut for FakeIrc {
        async fn puppet_connect(
            &mut self,
            nick: &str,
            _remote_id: &str,
            realname: &str,
        ) -> Result<()> {
            self.0.0.push(format!("connect {nick} ({realname})"));
            Ok(())
        }
        async fn puppet_say(&mut self, nick: &str, text: &str, action: bool) -> Result<()> {
            self.0.0.push(if action {
                format!("<{nick}> * {text}")
            } else {
                format!("<{nick}> {text}")
            });
            Ok(())
        }
        async fn puppet_quit(&mut self, nick: &str, reason: &str) -> Result<()> {
            self.0.0.push(format!("quit {nick} ({reason})"));
            Ok(())
        }
        async fn say(&mut self, text: &str) -> Result<()> {
            self.0.0.push(format!("say {text}"));
            Ok(())
        }
        async fn notice(&mut self, target: &str, text: &str) -> Result<()> {
            self.0.0.push(format!("notice {target}: {text}"));
            Ok(())
        }
        async fn set_topic(&mut self, topic: &str) -> Result<()> {
            self.0.0.push(format!("topic {topic}"));
            Ok(())
        }
        async fn set_moderated(&mut self, on: bool) -> Result<()> {
            self.0.0.push(format!("moderated {on}"));
            Ok(())
        }
    }

    fn bridge(extra: &str) -> Bridge<FakeTwitch, FakeIrc> {
        let raw = format!(
            r##"
            [twitch]
            channel = "ourshow"
            bot_login = "ourshow_irc"
            [irc]
            url = "wss://irc.example.org/web/irc"
            channel = "#stream"
            [lifecycle]
            live_topic = "LIVE"
            offline_topic = "OFF"
            join_notice = "public!"
            {extra}
            "##
        );
        Bridge::new(
            Config::parse(&raw).unwrap(),
            FakeTwitch::default(),
            FakeIrc::default(),
        )
    }

    fn viewer() -> TwitchUser {
        TwitchUser {
            id: "412983377".into(),
            login: "neuralnomad".into(),
            display_name: "NeuralNomad".into(),
        }
    }

    fn chat(user: TwitchUser, text: &str) -> Event {
        Event::Twitch(TwitchEvent::Chat {
            user,
            text: text.into(),
            action: false,
        })
    }

    fn irc(nick: &str, text: &str) -> Event {
        Event::Irc(IrcEvent::Message {
            nick: nick.into(),
            text: text.into(),
            is_op: false,
        })
    }

    #[tokio::test]
    async fn nothing_is_relayed_while_offline() {
        let mut b = bridge("");
        let now = Instant::now();
        b.handle(chat(viewer(), "hello"), now).await.unwrap();
        b.handle(irc("mihok", "hello"), now).await.unwrap();
        assert!(b.twitch.0.0.is_empty());
        assert!(b.irc.0.0.is_empty());
    }

    #[tokio::test]
    async fn full_conversation_while_live() {
        let mut b = bridge("");
        let now = Instant::now();
        b.handle(Event::Twitch(TwitchEvent::StreamOnline), now)
            .await
            .unwrap();
        b.handle(chat(viewer(), "has anyone run MCP over websockets?"), now)
            .await
            .unwrap();
        b.handle(irc("mihok", "\x02yes\x02, we do"), now)
            .await
            .unwrap();
        b.handle(
            irc("sarahk", "NeuralNomad|tw: see https://example.org"),
            now,
        )
        .await
        .unwrap();
        b.handle(irc("mihok", "\x01ACTION waves\x01"), now)
            .await
            .unwrap();
        b.handle(
            chat(
                TwitchUser {
                    id: "9".into(),
                    login: "ourshow_irc".into(),
                    display_name: "ourshow_irc".into(),
                },
                "[irc] echo",
            ),
            now,
        )
        .await
        .unwrap();

        assert_eq!(
            b.irc.0.0,
            vec![
                "topic LIVE",
                "moderated false",
                "connect NeuralNomad|tw (Twitch viewer: https://twitch.tv/neuralnomad)",
                "<NeuralNomad|tw> has anyone run MCP over websockets?",
            ]
        );
        assert_eq!(
            b.twitch.0.0,
            vec![
                "twitch: [irc] mihok: yes, we do",
                "twitch: [irc] sarahk: @NeuralNomad see <link>",
                "twitch: [irc] * mihok waves"
            ]
        );
    }

    #[tokio::test]
    async fn stream_end_quits_puppets_after_grace() {
        let mut b = bridge("");
        let t0 = Instant::now();
        b.handle(Event::Twitch(TwitchEvent::StreamOnline), t0)
            .await
            .unwrap();
        b.handle(chat(viewer(), "hi"), t0).await.unwrap();
        b.handle(Event::Twitch(TwitchEvent::StreamOffline), t0)
            .await
            .unwrap();
        b.handle(Event::Tick, t0 + Duration::from_secs(60))
            .await
            .unwrap();
        assert!(b.is_relaying());
        b.handle(Event::Tick, t0 + Duration::from_secs(601))
            .await
            .unwrap();
        assert!(!b.is_relaying());
        assert_eq!(
            &b.irc.0.0[b.irc.0.0.len() - 3..],
            [
                "quit NeuralNomad|tw (Stream ended)",
                "topic OFF",
                "moderated true"
            ]
        );
    }

    #[tokio::test]
    async fn twitch_ban_removes_puppet_and_blocks() {
        let mut b = bridge("");
        let now = Instant::now();
        b.handle(Event::Twitch(TwitchEvent::StreamOnline), now)
            .await
            .unwrap();
        b.handle(chat(viewer(), "spam"), now).await.unwrap();
        b.handle(
            Event::Twitch(TwitchEvent::UserBanned {
                user_id: "412983377".into(),
                duration: None,
            }),
            now,
        )
        .await
        .unwrap();
        b.handle(chat(viewer(), "more spam"), now).await.unwrap();
        assert_eq!(
            b.irc.0.0.last().unwrap(),
            "quit NeuralNomad|tw (Removed by Twitch moderators)"
        );
    }

    #[tokio::test]
    async fn op_commands() {
        let mut b = bridge("trigger = \"manual\"");
        let now = Instant::now();
        let op = |text: &str| {
            Event::Irc(IrcEvent::Message {
                nick: "mihok".into(),
                text: text.into(),
                is_op: true,
            })
        };
        b.handle(op("!bridge start"), now).await.unwrap();
        assert!(b.is_relaying());
        b.handle(op("!irc mute sarahk"), now).await.unwrap();
        b.handle(irc("sarahk", "ignored"), now).await.unwrap();
        // Non-ops can't run commands; their "!..." text is relayed like any message.
        b.handle(irc("troll", "!bridge stop"), now).await.unwrap();
        assert!(b.is_relaying());
        assert_eq!(b.twitch.0.0, vec!["twitch: [irc] troll: !bridge stop"]);
    }

    #[tokio::test]
    async fn rate_limit_drops_and_tells_sender() {
        let mut b = bridge("[relay]\nrate_limit = \"1/30s\"");
        let now = Instant::now();
        b.handle(Event::Twitch(TwitchEvent::StreamOnline), now)
            .await
            .unwrap();
        b.handle(irc("mihok", "one"), now).await.unwrap();
        b.handle(irc("mihok", "two"), now).await.unwrap();
        assert_eq!(b.twitch.0.0, vec!["twitch: [irc] mihok: one"]);
        assert!(
            b.irc
                .0
                .0
                .last()
                .unwrap()
                .starts_with("notice mihok: Slow down")
        );
    }

    #[tokio::test]
    async fn overflow_beyond_max_puppets_goes_through_control_connection() {
        let mut b = bridge("[puppets]\nmax_puppets = 1");
        let now = Instant::now();
        b.handle(Event::Twitch(TwitchEvent::StreamOnline), now)
            .await
            .unwrap();
        b.handle(chat(viewer(), "first"), now).await.unwrap();
        let other = TwitchUser {
            id: "98110234".into(),
            login: "42kilobytes".into(),
            display_name: "42kilobytes".into(),
        };
        b.handle(chat(other, "hello"), now).await.unwrap();
        assert_eq!(b.irc.0.0.last().unwrap(), "say <42kilobytes> hello");
    }
}
