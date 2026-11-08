//! The bridge core: consumes events from both sides and decides what to send.
//! It is transport-agnostic; the Twitch and IRC backends implement [`TwitchOut`]
//! and [`IrcOut`] and feed [`Event`]s in.

use std::time::{Duration, Instant};

use anyhow::Result;
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use crate::{
    config::{Config, Trigger},
    lifecycle::{Lifecycle, RunFor, Signal, State, Transition},
    moderation::Moderation,
    naming::TwitchUser,
    puppets::{Ensure, PuppetRegistry, Refusal},
    ratelimit::RateLimiter,
    relay,
};

#[derive(Debug, Clone)]
pub enum Event {
    Twitch(TwitchEvent),
    Irc(IrcEvent),
    Tick,
}

/// The message a Twitch reply answers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReplyParent {
    pub login: String,
    pub body: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TwitchEvent {
    StreamOnline,
    StreamOffline,
    Chat {
        user: TwitchUser,
        text: String,
        action: bool,
        /// Set when the message is a Twitch reply to another message.
        reply_to: Option<ReplyParent>,
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
    /// The server refused a puppet's nick because it belongs to another remote user (a
    /// Twitch name reused after a rename). `unsent` are the messages the puppet hadn't sent.
    PuppetNickRefused {
        nick: String,
        remote_id: String,
        unsent: Vec<(String, bool)>,
    },
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
    /// The exact nick of someone in the bridged channel, looked up case-insensitively.
    fn channel_nick(&self, name: &str) -> Option<String>;
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
        let manual_timeout = Some(cfg.lifecycle.manual_timeout).filter(|d| !d.is_zero());
        let lifecycle = Lifecycle::new(
            cfg.lifecycle.trigger,
            cfg.lifecycle.grace_period,
            manual_timeout,
        );
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
            Event::Twitch(TwitchEvent::Chat {
                user,
                text,
                action,
                reply_to,
            }) => {
                let text = self.mentions_for_irc(text, reply_to.as_ref());
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
            Event::Irc(IrcEvent::PuppetNickRefused {
                nick,
                remote_id,
                unsent,
            }) => {
                // The puppet's connection has already ended; drop its slot either way.
                self.irc.puppet_quit(&nick, "Nick taken").await?;
                match self.puppets.refuse_nick(&remote_id, &nick) {
                    Refusal::Renamed {
                        nick: new,
                        realname,
                    } if self.is_relaying() => {
                        info!(old = %nick, new = %new, "puppet nick belongs to another Twitch user on the server");
                        self.irc.puppet_connect(&new, &remote_id, &realname).await?;
                        for (text, action) in unsent {
                            self.irc.puppet_say(&new, &text, action).await?;
                        }
                    }
                    Refusal::Renamed { .. } | Refusal::Stale => {}
                    Refusal::GaveUp { display_name } => {
                        warn!(puppet = %nick, "server keeps refusing this viewer's nicks; relaying them via the control connection");
                        for (text, action) in unsent {
                            let text = if action { format!("* {text}") } else { text };
                            let line =
                                relay::render(&self.cfg.relay.to_irc_format, &display_name, &text);
                            self.irc.say(&line).await?;
                        }
                    }
                }
            }
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

    /// One line for `!bridge status` and command replies.
    fn status(&self, now: Instant) -> String {
        let left = |until: Instant| {
            let secs = until.saturating_duration_since(now).as_secs();
            humantime::format_duration(
                Duration::from_secs(secs - secs % 60).max(Duration::from_secs(60)),
            )
            .to_string()
        };
        let state = match self.lifecycle.state() {
            State::Offline => match self.lifecycle.trigger() {
                Trigger::Auto => "off; starts when the stream goes live".to_string(),
                Trigger::Manual => "off; start it with !bridge start".to_string(),
            },
            State::Live => "relaying (stream live)".to_string(),
            State::Grace { until } => format!("relaying (stream ended; stops in {})", left(until)),
            State::Manual { until: Some(until) } => {
                format!("relaying (started by hand; stops in {})", left(until))
            }
            State::Manual { until: None } => {
                "relaying (started by hand; until !bridge stop)".to_string()
            }
        };
        format!("{state}; puppets: {}", self.puppets.len())
    }

    /// Makes mentions in a Twitch message notify the right IRC user:
    /// - A Twitch *reply* to a message the bridge posted (`[irc] mihok: ...`) names the bot
    ///   (`@ourshow_irc thanks`); it becomes `mihok: thanks`.
    /// - `@Name` for someone in the channel becomes their exact nick.
    fn mentions_for_irc(&self, text: String, reply_to: Option<&ReplyParent>) -> String {
        let mut text = text;
        if let Some(parent) =
            reply_to.filter(|p| p.login.eq_ignore_ascii_case(&self.cfg.twitch.bot_login))
            && let Some(nick) = relay::relayed_irc_nick(
                &parent.body,
                &[
                    &self.cfg.relay.to_twitch_format,
                    &self.cfg.relay.to_twitch_action_format,
                ],
            )
        {
            // Twitch puts "@<parent author>" in front of replies; drop it.
            let rest = match text.split_once(' ') {
                Some((first, rest))
                    if first
                        .trim_start_matches('@')
                        .eq_ignore_ascii_case(&self.cfg.twitch.bot_login) =>
                {
                    rest.to_owned()
                }
                None if text
                    .trim_start_matches('@')
                    .eq_ignore_ascii_case(&self.cfg.twitch.bot_login) =>
                {
                    String::new()
                }
                _ => text.clone(),
            };
            let nick = self.irc.channel_nick(&nick).unwrap_or(nick);
            text = format!("{nick}: {rest}").trim_end().to_owned();
            return text;
        }
        relay::rewrite_twitch_mentions(&text, |name| {
            self.irc
                .channel_nick(name)
                .filter(|nick| !self.is_bridge_nick(nick))
        })
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
            // The server refused every nick we tried for this viewer.
            Ensure::Fallback => debug!(
                "relaying {} via the control connection (no puppet nick available)",
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
            text = relay::rewrite_mentions(&text, self.puppets.mention_map());
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
            ["!bridge", "start", rest @ ..] => {
                let run = match rest {
                    [] => Some(RunFor::Default),
                    ["forever"] => Some(RunFor::Forever),
                    [len] => humantime::parse_duration(len).ok().filter(|d| !d.is_zero()).map(RunFor::For),
                    _ => None,
                };
                match run {
                    None => "Usage: !bridge start [30m|2h|forever]".to_string(),
                    Some(_) if self.lifecycle.state() == State::Live => {
                        "The stream is live, so the bridge is already relaying.".to_string()
                    }
                    Some(run) => {
                        self.signal(Signal::ManualStart(run), now).await?;
                        format!("Bridge started by hand: {}", self.status(now))
                    }
                }
            }
            ["!bridge", "stop"] => {
                self.signal(Signal::ManualStop, now).await?;
                "Bridge stopped. It starts again when the stream goes live (or with !bridge start).".to_string()
            }
            ["!bridge", "status"] => self.status(now),
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
            _ => "Commands: !bridge start [30m|forever], !bridge stop|status, !tw block <puppet>, !tw unblock <twitch-id>, !irc mute|unmute <nick>".to_string(),
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

    /// Log of calls, and the IRC channel's members (for `channel_nick`).
    #[derive(Default)]
    struct FakeIrc(Log, Vec<String>);
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
        fn channel_nick(&self, name: &str) -> Option<String> {
            self.1
                .iter()
                .find(|n| n.eq_ignore_ascii_case(name))
                .cloned()
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
            reply_to: None,
        })
    }

    fn irc(nick: &str, text: &str) -> Event {
        Event::Irc(IrcEvent::Message {
            nick: nick.into(),
            text: text.into(),
            is_op: false,
        })
    }

    fn refused(nick: &str, unsent: &[(&str, bool)]) -> Event {
        Event::Irc(IrcEvent::PuppetNickRefused {
            nick: nick.into(),
            remote_id: viewer().id,
            unsent: unsent.iter().map(|(t, a)| (t.to_string(), *a)).collect(),
        })
    }

    #[tokio::test]
    async fn refused_puppet_nick_reconnects_under_the_next_one_and_resends() {
        let mut b = bridge("");
        let now = Instant::now();
        b.handle(Event::Twitch(TwitchEvent::StreamOnline), now)
            .await
            .unwrap();
        b.handle(chat(viewer(), "hi"), now).await.unwrap();
        b.irc.0.0.clear();
        b.handle(refused("NeuralNomad|twitch", &[("hi", false)]), now)
            .await
            .unwrap();
        assert_eq!(
            b.irc.0.0,
            [
                "quit NeuralNomad|twitch (Nick taken)",
                "connect NeuralNomad_2|twitch (Twitch viewer: https://twitch.tv/neuralnomad)",
                "<NeuralNomad_2|twitch> hi",
            ]
        );
        b.irc.0.0.clear();
        b.handle(chat(viewer(), "again"), now).await.unwrap();
        assert_eq!(b.irc.0.0, ["<NeuralNomad_2|twitch> again"]);
    }

    #[tokio::test]
    async fn viewer_whose_nicks_keep_being_refused_is_relayed_by_the_control_connection() {
        let mut b = bridge("");
        let now = Instant::now();
        b.handle(Event::Twitch(TwitchEvent::StreamOnline), now)
            .await
            .unwrap();
        b.handle(chat(viewer(), "hi"), now).await.unwrap();
        let mut nick = "NeuralNomad|twitch".to_string();
        for i in 2..=crate::puppets::MAX_NICK_REFUSALS + 1 {
            b.handle(refused(&nick, &[("hi", false)]), now)
                .await
                .unwrap();
            nick = format!("NeuralNomad_{i}|twitch");
        }
        b.irc.0.0.clear();
        b.handle(chat(viewer(), "still here"), now).await.unwrap();
        assert_eq!(b.irc.0.0, ["say <NeuralNomad> still here"]);
    }

    fn reply(user: TwitchUser, text: &str, parent_login: &str, parent_body: &str) -> Event {
        Event::Twitch(TwitchEvent::Chat {
            user,
            text: text.into(),
            action: false,
            reply_to: Some(ReplyParent {
                login: parent_login.into(),
                body: parent_body.into(),
            }),
        })
    }

    #[tokio::test]
    async fn twitch_mentions_reach_irc_users_by_their_exact_nick() {
        let mut b = bridge("");
        b.irc.1 = vec!["mihok".into(), "SarahK".into()];
        let now = Instant::now();
        b.handle(Event::Twitch(TwitchEvent::StreamOnline), now)
            .await
            .unwrap();
        b.irc.0.0.clear();
        // A Twitch reply to a bridged IRC message names the bot; it's turned into the IRC user.
        b.handle(
            reply(
                viewer(),
                "@ourshow_irc thanks, that worked",
                "ourshow_irc",
                "[irc] SarahK: try the websocket transport",
            ),
            now,
        )
        .await
        .unwrap();
        // Typed mentions use the exact nick (and IRC-style addressing at the start).
        b.handle(chat(viewer(), "@MIHOK are you around?"), now)
            .await
            .unwrap();
        b.handle(chat(viewer(), "gg @sarahk and @someone_on_twitch"), now)
            .await
            .unwrap();
        // A reply to another Twitch viewer stays as Twitch wrote it.
        b.handle(
            reply(viewer(), "@pixelpusher lol", "pixelpusher", "nice play"),
            now,
        )
        .await
        .unwrap();
        let said: Vec<&str> = b
            .irc
            .0
            .0
            .iter()
            .filter(|l| l.starts_with('<'))
            .map(String::as_str)
            .collect();
        assert_eq!(
            said,
            [
                "<NeuralNomad|twitch> SarahK: thanks, that worked",
                "<NeuralNomad|twitch> mihok: are you around?",
                "<NeuralNomad|twitch> gg SarahK and @someone_on_twitch",
                "<NeuralNomad|twitch> @pixelpusher lol",
            ]
        );
    }

    #[tokio::test]
    async fn irc_mentions_notify_twitch_users_by_login_even_after_idling_out() {
        let mut b = bridge("[puppets]\nidle_timeout = \"1m\"");
        let t0 = Instant::now();
        b.handle(Event::Twitch(TwitchEvent::StreamOnline), t0)
            .await
            .unwrap();
        let taro = TwitchUser {
            id: "730552190".into(),
            login: "datataro_jp".into(),
            display_name: "データ太郎".into(),
        };
        b.handle(chat(taro, "こんにちは"), t0).await.unwrap();
        b.handle(chat(viewer(), "hi"), t0).await.unwrap();
        // Both puppets idle out.
        b.handle(Event::Tick, t0 + Duration::from_secs(120))
            .await
            .unwrap();
        b.handle(
            irc(
                "mihok",
                "datataro_jp|twitch: welcome! NeuralNomad|twitch, you too",
            ),
            t0 + Duration::from_secs(121),
        )
        .await
        .unwrap();
        assert_eq!(
            b.twitch.0.0.last().unwrap(),
            "twitch: [irc] mihok: @datataro_jp welcome! @NeuralNomad, you too"
        );
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
            irc("sarahk", "NeuralNomad|twitch: see https://example.org"),
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
                "connect NeuralNomad|twitch (Twitch viewer: https://twitch.tv/neuralnomad)",
                "<NeuralNomad|twitch> has anyone run MCP over websockets?",
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
                "quit NeuralNomad|twitch (Stream ended)",
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
            "quit NeuralNomad|twitch (Removed by Twitch moderators)"
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

    fn op(text: &str) -> Event {
        Event::Irc(IrcEvent::Message {
            nick: "mihok".into(),
            text: text.into(),
            is_op: true,
        })
    }

    fn last_notice(b: &Bridge<FakeTwitch, FakeIrc>) -> String {
        b.irc
            .0
            .0
            .iter()
            .rev()
            .find(|l| l.starts_with("notice mihok: "))
            .cloned()
            .unwrap_or_default()
    }

    #[tokio::test]
    async fn manual_start_in_auto_mode_times_out() {
        let mut b = bridge("manual_timeout = \"1h\"");
        let t0 = Instant::now();
        b.handle(op("!bridge status"), t0).await.unwrap();
        assert_eq!(
            last_notice(&b),
            "notice mihok: off; starts when the stream goes live; puppets: 0"
        );
        b.handle(op("!bridge start"), t0).await.unwrap();
        assert!(b.is_relaying());
        assert_eq!(
            last_notice(&b),
            "notice mihok: Bridge started by hand: relaying (started by hand; stops in 1h); puppets: 0"
        );
        assert!(b.irc.0.0.contains(&"topic LIVE".to_string()));
        b.handle(chat(viewer(), "testing"), t0).await.unwrap();
        b.handle(Event::Tick, t0 + Duration::from_secs(3599))
            .await
            .unwrap();
        assert!(b.is_relaying());
        b.handle(Event::Tick, t0 + Duration::from_secs(3600))
            .await
            .unwrap();
        assert!(!b.is_relaying());
        // The run ended: offline topic and +m, as when a stream ends.
        let log = &b.irc.0.0;
        assert_eq!(&log[log.len() - 2..], ["topic OFF", "moderated true"]);
    }

    #[tokio::test]
    async fn manual_start_lengths() {
        let mut b = bridge("");
        let t0 = Instant::now();
        b.handle(op("!bridge start 30m"), t0).await.unwrap();
        assert!(last_notice(&b).ends_with("stops in 30m); puppets: 0"));
        b.handle(op("!bridge start forever"), t0).await.unwrap();
        assert!(last_notice(&b).ends_with("until !bridge stop); puppets: 0"));
        b.handle(Event::Tick, t0 + Duration::from_secs(86_400))
            .await
            .unwrap();
        assert!(b.is_relaying());
        b.handle(op("!bridge start soon"), t0).await.unwrap();
        assert_eq!(
            last_notice(&b),
            "notice mihok: Usage: !bridge start [30m|2h|forever]"
        );
        b.handle(op("!bridge stop"), t0).await.unwrap();
        assert!(!b.is_relaying());
        // While actually live, a manual start is a no-op.
        b.handle(Event::Twitch(TwitchEvent::StreamOnline), t0)
            .await
            .unwrap();
        b.handle(op("!bridge start 5m"), t0).await.unwrap();
        assert_eq!(
            last_notice(&b),
            "notice mihok: The stream is live, so the bridge is already relaying."
        );
        b.handle(Event::Tick, t0 + Duration::from_secs(600))
            .await
            .unwrap();
        assert!(b.is_relaying());
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
