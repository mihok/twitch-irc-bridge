//! Book-keeping for puppet users: which Twitch viewer has which IRC nick, and when
//! they last spoke. The IRC backend owns the actual connections.

use std::{
    collections::HashMap,
    time::{Duration, Instant},
};

use crate::{
    config::{NameSource, PuppetConfig},
    naming::{self, TwitchUser},
};

#[derive(Debug, Clone)]
pub struct Puppet {
    pub user: TwitchUser,
    pub nick: String,
    pub realname: String,
    pub last_active: Instant,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Ensure {
    Existing {
        nick: String,
    },
    Created {
        nick: String,
        realname: String,
    },
    /// The viewer renamed on Twitch since their puppet was created.
    Renamed {
        old: String,
        new: String,
        realname: String,
    },
    /// `max_puppets` reached; fall back to relay-bot formatting.
    Full,
}

#[derive(Debug)]
pub struct PuppetRegistry {
    template: String,
    name_source: NameSource,
    realname_template: String,
    max: usize,
    nicklen: usize,
    /// Case-folded nicks that puppets must never take (e.g. the control connection).
    reserved: Vec<String>,
    by_id: HashMap<String, Puppet>,
}

impl PuppetRegistry {
    pub fn new(cfg: &PuppetConfig, nicklen: usize, reserved: &[&str]) -> Self {
        Self {
            template: cfg.nick_template.clone(),
            name_source: cfg.name_source,
            realname_template: cfg.realname_template.clone(),
            max: cfg.max_puppets,
            nicklen,
            reserved: reserved.iter().map(|n| naming::casefold(n)).collect(),
            by_id: HashMap::new(),
        }
    }

    /// Lowers the nick length limit, e.g. after reading the server's ISUPPORT.
    pub fn set_nicklen(&mut self, nicklen: usize) {
        self.nicklen = self.nicklen.min(nicklen);
    }

    fn nick_for(&self, user: &TwitchUser) -> String {
        let taken = |folded: &str| {
            self.reserved.iter().any(|r| r == folded)
                || self
                    .by_id
                    .values()
                    .any(|p| p.user.id != user.id && naming::casefold(&p.nick) == folded)
        };
        naming::puppet_nick(user, self.name_source, &self.template, self.nicklen, taken)
    }

    pub fn ensure(&mut self, user: &TwitchUser, now: Instant) -> Ensure {
        if let Some(existing) = self.by_id.get(&user.id) {
            let same_names = existing.user.login == user.login
                && existing.user.display_name == user.display_name;
            let old = existing.nick.clone();
            let nick = if same_names {
                old.clone()
            } else {
                self.nick_for(user)
            };
            let p = self.by_id.get_mut(&user.id).expect("present");
            p.last_active = now;
            p.user = user.clone();
            if nick == old {
                return Ensure::Existing { nick };
            }
            p.nick = nick.clone();
            p.realname = naming::render_user_template(&self.realname_template, user);
            return Ensure::Renamed {
                old,
                new: nick,
                realname: p.realname.clone(),
            };
        }
        if self.by_id.len() >= self.max {
            return Ensure::Full;
        }
        let nick = self.nick_for(user);
        let realname = naming::render_user_template(&self.realname_template, user);
        self.by_id.insert(
            user.id.clone(),
            Puppet {
                user: user.clone(),
                nick: nick.clone(),
                realname: realname.clone(),
                last_active: now,
            },
        );
        Ensure::Created { nick, realname }
    }

    pub fn get(&self, twitch_id: &str) -> Option<&Puppet> {
        self.by_id.get(twitch_id)
    }

    pub fn by_nick(&self, nick: &str) -> Option<&Puppet> {
        let folded = naming::casefold(nick);
        self.by_id
            .values()
            .find(|p| naming::casefold(&p.nick) == folded)
    }

    pub fn remove(&mut self, twitch_id: &str) -> Option<Puppet> {
        self.by_id.remove(twitch_id)
    }

    /// Removes and returns the nicks of puppets idle for at least `idle`.
    pub fn expire(&mut self, now: Instant, idle: Duration) -> Vec<String> {
        let ids: Vec<String> = self
            .by_id
            .values()
            .filter(|p| now.duration_since(p.last_active) >= idle)
            .map(|p| p.user.id.clone())
            .collect();
        ids.iter()
            .filter_map(|id| self.by_id.remove(id))
            .map(|p| p.nick)
            .collect()
    }

    pub fn drain(&mut self) -> Vec<String> {
        self.by_id.drain().map(|(_, p)| p.nick).collect()
    }

    /// Case-folded puppet nick → Twitch display name, for mention rewriting.
    pub fn mention_map(&self) -> HashMap<String, String> {
        self.by_id
            .values()
            .map(|p| (naming::casefold(&p.nick), p.user.display_name.clone()))
            .collect()
    }

    pub fn len(&self) -> usize {
        self.by_id.len()
    }

    pub fn is_empty(&self) -> bool {
        self.by_id.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn user(id: &str, login: &str, display: &str) -> TwitchUser {
        TwitchUser {
            id: id.into(),
            login: login.into(),
            display_name: display.into(),
        }
    }

    fn registry(max: usize) -> PuppetRegistry {
        let cfg = PuppetConfig {
            max_puppets: max,
            ..Default::default()
        };
        PuppetRegistry::new(&cfg, 32, &["twbridge"])
    }

    #[test]
    fn create_then_reuse() {
        let mut r = registry(10);
        let t0 = Instant::now();
        let u = user("1", "neuralnomad", "NeuralNomad");
        assert_eq!(
            r.ensure(&u, t0),
            Ensure::Created {
                nick: "NeuralNomad|twitch".into(),
                realname: "Twitch viewer: https://twitch.tv/neuralnomad".into()
            }
        );
        assert_eq!(
            r.ensure(&u, t0),
            Ensure::Existing {
                nick: "NeuralNomad|twitch".into()
            }
        );
    }

    #[test]
    fn twitch_rename_renames_puppet() {
        let mut r = registry(10);
        let t0 = Instant::now();
        r.ensure(&user("1", "neuralnomad", "NeuralNomad"), t0);
        assert_eq!(
            r.ensure(&user("1", "gradientghost", "GradientGhost"), t0),
            Ensure::Renamed {
                old: "NeuralNomad|twitch".into(),
                new: "GradientGhost|twitch".into(),
                realname: "Twitch viewer: https://twitch.tv/gradientghost".into(),
            }
        );
    }

    #[test]
    fn distinct_viewers_never_share_a_nick() {
        let mut r = registry(10);
        let t0 = Instant::now();
        r.ensure(&user("1", "neuralnomad", "NeuralNomad"), t0);
        // A different account that maps to the same nick (e.g. after truncation or a rename race).
        let Ensure::Created { nick, .. } = r.ensure(&user("2", "neuralnomad", "NeuralNomad"), t0)
        else {
            panic!()
        };
        assert_eq!(nick, "NeuralNomad_2|twitch");
    }

    #[test]
    fn full_registry_falls_back() {
        let mut r = registry(1);
        let t0 = Instant::now();
        r.ensure(&user("1", "a_viewer", "a_viewer"), t0);
        assert_eq!(
            r.ensure(&user("2", "b_viewer", "b_viewer"), t0),
            Ensure::Full
        );
    }

    #[test]
    fn idle_puppets_expire() {
        let mut r = registry(10);
        let t0 = Instant::now();
        r.ensure(&user("1", "a_viewer", "a_viewer"), t0);
        r.ensure(
            &user("2", "b_viewer", "b_viewer"),
            t0 + Duration::from_secs(60),
        );
        let gone = r.expire(t0 + Duration::from_secs(90), Duration::from_secs(60));
        assert_eq!(gone, vec!["a_viewer|twitch".to_string()]);
        assert_eq!(r.len(), 1);
    }
}
