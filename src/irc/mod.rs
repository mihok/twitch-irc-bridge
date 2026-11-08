//! IRC backend for slashcheer-irc (Ergo behind a Cloudflare Worker).
//!
//! - **Control connection**: the account bound to the bridge's Access service token
//!   (`irc.control_nick`). It sits in `irc.channel`, follows who is a channel operator, reports
//!   channel messages and joins as [`IrcEvent`]s, and sends topic/mode/notice/relay lines. It
//!   needs channel-op status (e.g. `/CS AMODE #stream +o twbridge`) to set the topic and `+m`.
//! - **Puppet connections**: one websocket per active Twitch viewer, with the same service token
//!   plus `X-Slashcheer-Puppet` / `X-Slashcheer-Puppet-Id`. slashcheer-irc logs it in as the
//!   puppet account and only lets it join and talk in the bridge's channels.
//!
//! Both kinds of session are small state machines ([`ControlSession`], [`PuppetSession`]) that
//! turn incoming lines into lines to send and events, so they can be tested without a network.

pub mod conn;
pub mod proto;

use std::{
    collections::{HashMap, HashSet, VecDeque},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use anyhow::{Result, bail};
use tokio::{sync::mpsc, task::JoinHandle};
use tracing::{debug, info, warn};

use self::{
    conn::{AccessToken, Connection, PuppetLogin, Refused},
    proto::{ModeInfo, notices, privmsgs},
};
use crate::{
    bridge::{Event, IrcEvent, IrcOut},
    config::IrcConfig,
    naming::casefold,
};

/// Lines queued while a connection isn't in the channel yet.
const MAX_PENDING: usize = 200;
/// A puppet whose connection failed isn't retried more often than this.
const PUPPET_RETRY: Duration = Duration::from_secs(60);

/// What a session wants done after handling a line.
#[derive(Debug, PartialEq)]
pub enum Action {
    Send(String),
    Emit(IrcEvent),
    /// Fatal for this connection (e.g. the server refused our nick).
    Close(String),
}

// ---------------------------------------------------------------------------
// Control connection
// ---------------------------------------------------------------------------

/// Who's in the bridged channel: case-folded nick → nick as the server spells it. Shared
/// between the control connection (which keeps it current) and the bridge (which uses it
/// to turn Twitch `@name` mentions into the exact IRC nick, so IRC clients highlight them).
pub type Members = Arc<Mutex<HashMap<String, String>>>;

#[derive(Debug)]
pub struct ControlSession {
    nick: String,
    channel: String,
    joined: bool,
    ops: HashSet<String>,
    modes: ModeInfo,
    members: Members,
}

impl ControlSession {
    pub fn new(nick: &str, channel: &str) -> Self {
        Self::with_members(nick, channel, Members::default())
    }

    /// A session that keeps `members` current (cleared on start: a new connection
    /// learns the channel's members from scratch).
    pub fn with_members(nick: &str, channel: &str, members: Members) -> Self {
        members.lock().expect("members lock").clear();
        Self {
            nick: nick.to_owned(),
            channel: channel.to_owned(),
            joined: false,
            ops: HashSet::new(),
            modes: ModeInfo::default(),
            members,
        }
    }

    fn member_add(&self, nick: &str) {
        if !nick.is_empty() && !self.is_me(nick) {
            self.members
                .lock()
                .expect("members lock")
                .insert(casefold(nick), nick.to_owned());
        }
    }

    fn member_remove(&self, nick: &str) {
        self.members
            .lock()
            .expect("members lock")
            .remove(&casefold(nick));
    }

    pub fn register(&self) -> Vec<String> {
        vec![
            format!("NICK {}", self.nick),
            format!("USER bridge 0 * :Twitch chat bridge for {}", self.channel),
        ]
    }

    pub fn joined(&self) -> bool {
        self.joined
    }

    fn is_me(&self, nick: &str) -> bool {
        casefold(nick) == casefold(&self.nick)
    }

    fn is_channel(&self, chan: &str) -> bool {
        casefold(chan) == casefold(&self.channel)
    }

    pub fn on_line(&mut self, line: &str) -> Vec<Action> {
        let Some(m) = proto::parse(line) else {
            return vec![];
        };
        let from = m.nick().unwrap_or("").to_owned();
        match m.command.as_str() {
            "PING" => vec![Action::Send(format!("PONG :{}", m.param(0)))],
            "001" => {
                self.nick = m.param(0).to_owned();
                vec![Action::Send(format!("JOIN {}", self.channel))]
            }
            "005" => {
                let mut out = vec![];
                for token in m
                    .params
                    .iter()
                    .skip(1)
                    .take(m.params.len().saturating_sub(2))
                {
                    self.modes.apply_isupport(token);
                    if let Some(n) = token.strip_prefix("NICKLEN=").and_then(|n| n.parse().ok()) {
                        out.push(Action::Emit(IrcEvent::NickLen(n)));
                    }
                }
                out
            }
            "433" | "432" => vec![Action::Close(format!(
                "server refused nick {}: control_nick must equal the account bound to the service token",
                self.nick
            ))],
            "353" if self.is_channel(m.param(2)) => {
                let op_symbols = self.modes.op_symbols().to_owned();
                let all_symbols = self.modes.prefix_symbols.clone();
                for entry in m.param(3).split(' ').filter(|e| !e.is_empty()) {
                    let name = entry.trim_start_matches(|c| all_symbols.contains(c));
                    // userhost-in-names: "nick!user@host"
                    self.member_add(name.split('!').next().unwrap_or(name));
                    let is_op = entry.starts_with(|c| op_symbols.contains(c));
                    if is_op {
                        self.ops.insert(casefold(name));
                    } else {
                        self.ops.remove(&casefold(name));
                    }
                }
                vec![]
            }
            "JOIN" if self.is_channel(m.param(0)) => {
                if self.is_me(&from) {
                    self.joined = true;
                    self.ops.clear();
                    self.members.lock().expect("members lock").clear();
                    vec![]
                } else {
                    self.member_add(&from);
                    vec![Action::Emit(IrcEvent::Join { nick: from })]
                }
            }
            "PART" if self.is_channel(m.param(0)) => {
                if self.is_me(&from) {
                    self.joined = false;
                    vec![Action::Send(format!("JOIN {}", self.channel))]
                } else {
                    self.ops.remove(&casefold(&from));
                    self.member_remove(&from);
                    vec![]
                }
            }
            "KICK" if self.is_channel(m.param(0)) => {
                if self.is_me(m.param(1)) {
                    self.joined = false;
                    warn!(
                        "control connection was kicked from {}; rejoining",
                        self.channel
                    );
                    vec![Action::Send(format!("JOIN {}", self.channel))]
                } else {
                    self.ops.remove(&casefold(m.param(1)));
                    self.member_remove(m.param(1));
                    vec![]
                }
            }
            "QUIT" => {
                self.ops.remove(&casefold(&from));
                self.member_remove(&from);
                vec![]
            }
            "NICK" => {
                if self.is_me(&from) {
                    self.nick = m.param(0).to_owned();
                } else {
                    if self.ops.remove(&casefold(&from)) {
                        self.ops.insert(casefold(m.param(0)));
                    }
                    let was_member = self
                        .members
                        .lock()
                        .expect("members lock")
                        .remove(&casefold(&from))
                        .is_some();
                    if was_member {
                        self.member_add(m.param(0));
                    }
                }
                vec![]
            }
            "MODE" if self.is_channel(m.param(0)) => {
                for (nick, is_op_mode, adding) in self
                    .modes
                    .prefix_changes(m.param(1), &m.params[2.min(m.params.len())..])
                {
                    if is_op_mode && adding {
                        self.ops.insert(casefold(nick));
                    } else if is_op_mode {
                        self.ops.remove(&casefold(nick));
                    }
                }
                vec![]
            }
            "PRIVMSG" if self.is_channel(m.param(0)) && !self.is_me(&from) => {
                let is_op = self.ops.contains(&casefold(&from));
                vec![Action::Emit(IrcEvent::Message {
                    nick: from,
                    text: m.param(1).to_owned(),
                    is_op,
                })]
            }
            "482" => {
                warn!("not a channel operator in {}: {}", self.channel, m.param(2));
                vec![]
            }
            "ERROR" => vec![Action::Close(m.param(0).to_owned())],
            _ => vec![],
        }
    }
}

async fn control_task(
    cfg: IrcConfig,
    token: AccessToken,
    mut rx: mpsc::UnboundedReceiver<String>,
    events: mpsc::Sender<Event>,
    members: Members,
) {
    let mut pending: VecDeque<String> = VecDeque::new();
    let mut backoff = Duration::from_secs(1);
    loop {
        let mut conn = match Connection::open(&cfg.url, &token, None).await {
            Ok(c) => c,
            Err(e) => {
                warn!("control connection: {e:#}; retrying in {backoff:?}");
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(Duration::from_secs(60));
                continue;
            }
        };
        let mut session =
            ControlSession::with_members(&cfg.control_nick, &cfg.channel, members.clone());
        let mut ok = true;
        for l in session.register() {
            ok &= conn.send(&l).await.is_ok();
        }
        while ok {
            tokio::select! {
                line = conn.recv() => {
                    let line = match line {
                        Ok(Some(l)) => l,
                        Ok(None) => { warn!("control connection closed"); break }
                        Err(e) => { warn!("control connection: {e:#}"); break }
                    };
                    let was_joined = session.joined();
                    for action in session.on_line(&line) {
                        match action {
                            Action::Send(l) => ok &= conn.send(&l).await.is_ok(),
                            Action::Emit(e) => { let _ = events.send(Event::Irc(e)).await; }
                            Action::Close(reason) => { warn!("control connection: {reason}"); ok = false }
                        }
                    }
                    if !was_joined && session.joined() {
                        info!("control connection joined {}", cfg.channel);
                        backoff = Duration::from_secs(1);
                        while let Some(l) = pending.pop_front() {
                            ok &= conn.send(&l).await.is_ok();
                        }
                    }
                }
                cmd = rx.recv() => match cmd {
                    Some(l) if session.joined() => ok &= conn.send(&l).await.is_ok(),
                    Some(l) => {
                        if pending.len() >= MAX_PENDING { pending.pop_front(); }
                        pending.push_back(l);
                    }
                    None => {
                        let _ = conn.send("QUIT :Bridge shutting down").await;
                        conn.close().await;
                        return;
                    }
                },
            }
        }
        conn.close().await;
        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(Duration::from_secs(60));
    }
}

// ---------------------------------------------------------------------------
// Puppet connections
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub enum PuppetCmd {
    Say { text: String, action: bool },
    Quit { reason: String },
}

#[derive(Debug)]
pub struct PuppetSession {
    nick: String,
    channel: String,
    realname: String,
    joined: bool,
}

impl PuppetSession {
    pub fn new(nick: &str, channel: &str, realname: &str) -> Self {
        Self {
            nick: nick.to_owned(),
            channel: channel.to_owned(),
            realname: realname.to_owned(),
            joined: false,
        }
    }

    pub fn register(&self) -> Vec<String> {
        vec![
            format!("NICK {}", self.nick),
            format!("USER twitch 0 * :{}", proto::sanitize(&self.realname)),
        ]
    }

    pub fn joined(&self) -> bool {
        self.joined
    }

    pub fn on_line(&mut self, line: &str) -> Vec<Action> {
        let Some(m) = proto::parse(line) else {
            return vec![];
        };
        let from = m.nick().unwrap_or("").to_owned();
        let is_me = |n: &str| casefold(n) == casefold(&self.nick);
        match m.command.as_str() {
            "PING" => vec![Action::Send(format!("PONG :{}", m.param(0)))],
            "001" => vec![Action::Send(format!("JOIN {}", self.channel))],
            "JOIN" if is_me(&from) && casefold(m.param(0)) == casefold(&self.channel) => {
                self.joined = true;
                vec![]
            }
            "KICK" if is_me(m.param(1)) => vec![Action::Close(format!(
                "kicked from {}: {}",
                self.channel,
                m.param(2)
            ))],
            // Someone messaged the puppet directly; CTCP queries (VERSION etc.) are ignored.
            "PRIVMSG" if is_me(m.param(0)) && !m.param(1).starts_with('\x01') => {
                vec![Action::Emit(IrcEvent::PuppetDirectMessage {
                    from,
                    puppet: self.nick.clone(),
                })]
            }
            "432" | "433" => vec![Action::Close(format!("server refused nick {}", self.nick))],
            "ERROR" => vec![Action::Close(m.param(0).to_owned())],
            _ => vec![],
        }
    }
}

struct PuppetTask {
    url: String,
    token: AccessToken,
    login: PuppetLogin,
    channel: String,
    realname: String,
    events: mpsc::Sender<Event>,
}

impl PuppetTask {
    async fn run(self, mut rx: mpsc::UnboundedReceiver<PuppetCmd>) {
        let nick = self.login.nick.clone();
        let Err(e) = self.session(&mut rx).await else {
            return;
        };
        if e.downcast_ref::<Refused>()
            .is_some_and(Refused::name_in_use)
        {
            // The connection never opened, so everything said so far is still queued.
            let mut unsent = Vec::new();
            while let Ok(cmd) = rx.try_recv() {
                if let PuppetCmd::Say { text, action } = cmd {
                    unsent.push((text, action));
                }
            }
            info!(puppet = %nick, "nick belongs to another remote user on the server");
            let _ = self
                .events
                .send(Event::Irc(IrcEvent::PuppetNickRefused {
                    nick,
                    remote_id: self.login.remote_id.clone(),
                    unsent,
                }))
                .await;
            return;
        }
        warn!(puppet = %nick, "{e:#}");
    }

    async fn session(&self, rx: &mut mpsc::UnboundedReceiver<PuppetCmd>) -> Result<()> {
        let mut conn = Connection::open(&self.url, &self.token, Some(&self.login)).await?;
        let mut session = PuppetSession::new(&self.login.nick, &self.channel, &self.realname);
        for l in session.register() {
            conn.send(&l).await?;
        }
        let mut pending: VecDeque<String> = VecDeque::new();
        loop {
            tokio::select! {
                line = conn.recv() => {
                    let Some(line) = line? else { bail!("connection closed") };
                    let was_joined = session.joined();
                    for action in session.on_line(&line) {
                        match action {
                            Action::Send(l) => conn.send(&l).await?,
                            Action::Emit(e) => { let _ = self.events.send(Event::Irc(e)).await; }
                            Action::Close(reason) => { conn.close().await; bail!(reason) }
                        }
                    }
                    if !was_joined && session.joined() {
                        debug!(puppet = %self.login.nick, "joined");
                        while let Some(l) = pending.pop_front() {
                            conn.send(&l).await?;
                        }
                    }
                }
                cmd = rx.recv() => match cmd {
                    Some(PuppetCmd::Say { text, action }) => {
                        for l in privmsgs(&self.channel, &text, action) {
                            if session.joined() {
                                conn.send(&l).await?;
                            } else if pending.len() < MAX_PENDING {
                                pending.push_back(l);
                            }
                        }
                    }
                    Some(PuppetCmd::Quit { reason }) => {
                        let _ = conn.send(&format!("QUIT :{}", proto::sanitize(&reason))).await;
                        conn.close().await;
                        return Ok(());
                    }
                    None => {
                        let _ = conn.send("QUIT :Bridge shutting down").await;
                        conn.close().await;
                        return Ok(());
                    }
                },
            }
        }
    }
}

struct Puppet {
    remote_id: String,
    realname: String,
    tx: mpsc::UnboundedSender<PuppetCmd>,
    task: JoinHandle<()>,
    started: Instant,
}

// ---------------------------------------------------------------------------
// IrcOut
// ---------------------------------------------------------------------------

pub struct IrcClient {
    cfg: IrcConfig,
    token: AccessToken,
    events: mpsc::Sender<Event>,
    control: mpsc::UnboundedSender<String>,
    /// Keyed by case-folded nick.
    puppets: HashMap<String, Puppet>,
    members: Members,
}

impl IrcClient {
    /// Starts the control connection (it reconnects on its own) using the Access service token
    /// from the environment.
    pub async fn connect(cfg: IrcConfig, events: mpsc::Sender<Event>) -> Result<Self> {
        Ok(Self::start(cfg, AccessToken::from_env()?, events))
    }

    pub fn start(cfg: IrcConfig, token: AccessToken, events: mpsc::Sender<Event>) -> Self {
        let (control, rx) = mpsc::unbounded_channel();
        let members = Members::default();
        tokio::spawn(control_task(
            cfg.clone(),
            token.clone(),
            rx,
            events.clone(),
            members.clone(),
        ));
        Self {
            cfg,
            token,
            events,
            control,
            puppets: HashMap::new(),
            members,
        }
    }

    fn spawn_puppet(&self, nick: &str, remote_id: &str, realname: &str) -> Puppet {
        let (tx, rx) = mpsc::unbounded_channel();
        let task = PuppetTask {
            url: self.cfg.url.clone(),
            token: self.token.clone(),
            login: PuppetLogin {
                nick: nick.to_owned(),
                remote_id: remote_id.to_owned(),
            },
            channel: self.cfg.channel.clone(),
            realname: realname.to_owned(),
            events: self.events.clone(),
        };
        Puppet {
            remote_id: remote_id.to_owned(),
            realname: realname.to_owned(),
            tx,
            task: tokio::spawn(task.run(rx)),
            started: Instant::now(),
        }
    }

    fn control_lines(&self, lines: Vec<String>) -> Result<()> {
        for l in lines {
            if self.control.send(l).is_err() {
                bail!("control connection task has stopped");
            }
        }
        Ok(())
    }
}

impl IrcOut for IrcClient {
    async fn puppet_connect(&mut self, nick: &str, remote_id: &str, realname: &str) -> Result<()> {
        let puppet = self.spawn_puppet(nick, remote_id, realname);
        if let Some(old) = self.puppets.insert(casefold(nick), puppet) {
            let _ = old.tx.send(PuppetCmd::Quit {
                reason: "Reconnecting".into(),
            });
        }
        Ok(())
    }

    async fn puppet_say(&mut self, nick: &str, text: &str, action: bool) -> Result<()> {
        let key = casefold(nick);
        let Some(p) = self.puppets.get(&key) else {
            bail!("no puppet connection for {nick}")
        };
        if p.task.is_finished() {
            if p.started.elapsed() < PUPPET_RETRY {
                warn!(puppet = %nick, "puppet connection is down; dropping message");
                return Ok(());
            }
            info!(puppet = %nick, "reconnecting puppet");
            let fresh = self.spawn_puppet(nick, &p.remote_id.clone(), &p.realname.clone());
            self.puppets.insert(key.clone(), fresh);
        }
        let p = &self.puppets[&key];
        let _ = p.tx.send(PuppetCmd::Say {
            text: text.to_owned(),
            action,
        });
        Ok(())
    }

    async fn puppet_quit(&mut self, nick: &str, reason: &str) -> Result<()> {
        if let Some(p) = self.puppets.remove(&casefold(nick)) {
            let _ = p.tx.send(PuppetCmd::Quit {
                reason: reason.to_owned(),
            });
        }
        Ok(())
    }

    async fn say(&mut self, text: &str) -> Result<()> {
        self.control_lines(privmsgs(&self.cfg.channel, text, false))
    }

    async fn notice(&mut self, target: &str, text: &str) -> Result<()> {
        self.control_lines(notices(target, text))
    }

    async fn set_topic(&mut self, topic: &str) -> Result<()> {
        self.control_lines(vec![format!(
            "TOPIC {} :{}",
            self.cfg.channel,
            proto::sanitize(topic)
        )])
    }

    fn channel_nick(&self, name: &str) -> Option<String> {
        self.members
            .lock()
            .expect("members lock")
            .get(&casefold(name))
            .cloned()
    }

    async fn set_moderated(&mut self, on: bool) -> Result<()> {
        self.control_lines(vec![format!(
            "MODE {} {}m",
            self.cfg.channel,
            if on { '+' } else { '-' }
        )])
    }
}

#[cfg(test)]
pub(crate) mod tests;
