//! Bridge-level moderation. Needed because puppets can share one host/cloak, so
//! IRC host bans can't target an individual Twitch viewer reliably.

use std::{
    collections::{HashMap, HashSet},
    time::{Duration, Instant},
};

use crate::naming::casefold;

#[derive(Debug, Default)]
pub struct Moderation {
    /// Twitch user ID → block expiry (`None` = permanent).
    blocked: HashMap<String, Option<Instant>>,
    /// Case-folded IRC nicks whose messages are not relayed to Twitch.
    muted: HashSet<String>,
}

impl Moderation {
    pub fn block_twitch(&mut self, user_id: &str, duration: Option<Duration>, now: Instant) {
        self.blocked
            .insert(user_id.to_owned(), duration.map(|d| now + d));
    }

    pub fn unblock_twitch(&mut self, user_id: &str) -> bool {
        self.blocked.remove(user_id).is_some()
    }

    pub fn is_twitch_blocked(&self, user_id: &str, now: Instant) -> bool {
        matches!(self.blocked.get(user_id), Some(None))
            || matches!(self.blocked.get(user_id), Some(Some(t)) if now < *t)
    }

    pub fn mute_irc(&mut self, nick: &str) {
        self.muted.insert(casefold(nick));
    }

    pub fn unmute_irc(&mut self, nick: &str) -> bool {
        self.muted.remove(&casefold(nick))
    }

    pub fn is_irc_muted(&self, nick: &str) -> bool {
        self.muted.contains(&casefold(nick))
    }

    /// Drops expired timeouts.
    pub fn sweep(&mut self, now: Instant) {
        self.blocked
            .retain(|_, until| until.is_none_or(|t| now < t));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timeouts_expire_bans_do_not() {
        let t0 = Instant::now();
        let mut m = Moderation::default();
        m.block_twitch("1", Some(Duration::from_secs(60)), t0);
        m.block_twitch("2", None, t0);
        assert!(m.is_twitch_blocked("1", t0));
        assert!(!m.is_twitch_blocked("1", t0 + Duration::from_secs(60)));
        assert!(m.is_twitch_blocked("2", t0 + Duration::from_secs(3600)));
    }

    #[test]
    fn mutes_are_case_insensitive() {
        let mut m = Moderation::default();
        m.mute_irc("Sarahk");
        assert!(m.is_irc_muted("sarahK"));
        assert!(m.unmute_irc("SARAHK"));
        assert!(!m.is_irc_muted("sarahk"));
    }
}
