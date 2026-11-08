//! Book-keeping for puppet users: which Twitch viewer has which IRC nick, and when
//! they last spoke. The IRC backend owns the actual connections.

use std::{
    collections::{HashMap, HashSet},
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
    /// The server refused every nick we tried for this viewer (see
    /// [`PuppetRegistry::refuse_nick`]); relay-bot formatting until the stream ends.
    Fallback,
}

/// What to do after the server refused a puppet's nick.
#[derive(Debug, PartialEq, Eq)]
pub enum Refusal {
    /// Reconnect the viewer under this nick.
    Renamed { nick: String, realname: String },
    /// Too many refusals; relay this viewer through the control connection instead.
    GaveUp { display_name: String },
    /// The refusal is for a puppet that's gone or already renamed; nothing to do.
    Stale,
}

/// How many puppet nicks (including idle ones) to remember for mention rewriting.
const MAX_KNOWN: usize = 5000;

/// The name Twitch notifies on for `@name`: the display name when it's the login in other
/// case (`NeuralNomad`), otherwise the login (`データ太郎` → `datataro_jp`).
pub fn twitch_mention_name(user: &TwitchUser) -> &str {
    if user.display_name.eq_ignore_ascii_case(&user.login) {
        &user.display_name
    } else {
        &user.login
    }
}

/// Nicks the server may refuse for one viewer before the bridge stops trying.
pub(crate) const MAX_NICK_REFUSALS: usize = 5;

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
    /// Per Twitch id: case-folded nicks the server refused because another remote user owns
    /// them. Kept for the process's lifetime: server-side ownership doesn't change.
    refused: HashMap<String, HashSet<String>>,
    /// Twitch ids relayed via the control connection after too many refusals (this stream).
    gave_up: HashSet<String>,
    /// Case-folded puppet nick → Twitch mention name, for every puppet seen, including ones
    /// that idled out: IRC users still mention them, and Twitch should still notify them.
    known: HashMap<String, String>,
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
            refused: HashMap::new(),
            gave_up: HashSet::new(),
            known: HashMap::new(),
        }
    }

    /// Lowers the nick length limit, e.g. after reading the server's ISUPPORT.
    pub fn set_nicklen(&mut self, nicklen: usize) {
        self.nicklen = self.nicklen.min(nicklen);
    }

    fn nick_for(&self, user: &TwitchUser) -> String {
        let refused = self.refused.get(&user.id);
        let taken = |folded: &str| {
            self.reserved.iter().any(|r| r == folded)
                || refused.is_some_and(|r| r.contains(folded))
                || self
                    .by_id
                    .values()
                    .any(|p| p.user.id != user.id && naming::casefold(&p.nick) == folded)
        };
        naming::puppet_nick(user, self.name_source, &self.template, self.nicklen, taken)
    }

    pub fn ensure(&mut self, user: &TwitchUser, now: Instant) -> Ensure {
        if self.gave_up.contains(&user.id) {
            return Ensure::Fallback;
        }
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
            let realname = p.realname.clone();
            self.remember(&nick, user);
            return Ensure::Renamed {
                old,
                new: nick,
                realname,
            };
        }
        if self.by_id.len() >= self.max {
            return Ensure::Full;
        }
        let nick = self.nick_for(user);
        let realname = naming::render_user_template(&self.realname_template, user);
        self.remember(&nick, user);
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

    /// The server refused `nick` for this viewer because another remote user owns it. Picks
    /// the next free nick (`name_2|twitch`, ...), or gives up after [`MAX_NICK_REFUSALS`].
    pub fn refuse_nick(&mut self, twitch_id: &str, nick: &str) -> Refusal {
        let folded = naming::casefold(nick);
        let Some(p) = self.by_id.get(twitch_id) else {
            return Refusal::Stale;
        };
        if naming::casefold(&p.nick) != folded {
            return Refusal::Stale;
        }
        let refused = self.refused.entry(twitch_id.to_owned()).or_default();
        refused.insert(folded);
        if refused.len() >= MAX_NICK_REFUSALS {
            let p = self.by_id.remove(twitch_id).expect("present");
            self.gave_up.insert(twitch_id.to_owned());
            return Refusal::GaveUp {
                display_name: p.user.display_name,
            };
        }
        let user = p.user.clone();
        let new = self.nick_for(&user);
        self.remember(&new, &user);
        let p = self.by_id.get_mut(twitch_id).expect("present");
        p.nick = new.clone();
        Refusal::Renamed {
            nick: new,
            realname: p.realname.clone(),
        }
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
        // Next stream, try puppets again for viewers we gave up on (refused nicks stay).
        self.gave_up.clear();
        self.by_id.drain().map(|(_, p)| p.nick).collect()
    }

    /// Case-folded puppet nick → Twitch mention name, for mention rewriting.
    pub fn mention_map(&self) -> &HashMap<String, String> {
        &self.known
    }

    fn remember(&mut self, nick: &str, user: &TwitchUser) {
        if self.known.len() >= MAX_KNOWN {
            // Rare; keep the active ones.
            self.known.clear();
            let active: Vec<(String, String)> = self
                .by_id
                .values()
                .map(|p| {
                    (
                        naming::casefold(&p.nick),
                        twitch_mention_name(&p.user).to_owned(),
                    )
                })
                .collect();
            self.known.extend(active);
        }
        self.known
            .insert(naming::casefold(nick), twitch_mention_name(user).to_owned());
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

    #[test]
    fn refused_nick_moves_to_the_next_free_one_for_that_viewer_only() {
        let mut r = registry(10);
        let t0 = Instant::now();
        let viewer = user("2", "neuralnomad", "NeuralNomad");
        let Ensure::Created { nick, .. } = r.ensure(&viewer, t0) else {
            panic!()
        };
        assert_eq!(nick, "NeuralNomad|twitch");
        // On the server, NeuralNomad|twitch belongs to whoever had that Twitch name before.
        let Refusal::Renamed { nick: new, .. } = r.refuse_nick("2", "NeuralNomad|twitch") else {
            panic!()
        };
        assert_eq!(new, "NeuralNomad_2|twitch");
        assert_eq!(r.get("2").unwrap().nick, new);
        assert_eq!(
            r.ensure(&viewer, t0),
            Ensure::Existing { nick: new.clone() }
        );
        // A late or repeated refusal for the old nick changes nothing.
        assert_eq!(r.refuse_nick("2", "NeuralNomad|twitch"), Refusal::Stale);
        assert_eq!(r.refuse_nick("9", "x|twitch"), Refusal::Stale);
        // The refusal is per viewer: the name's real owner still gets it.
        let owner = user("1", "neuralnomad", "NeuralNomad");
        r.remove("2");
        assert_eq!(
            r.ensure(&owner, t0),
            Ensure::Created {
                nick: "NeuralNomad|twitch".into(),
                realname: "Twitch viewer: https://twitch.tv/neuralnomad".into()
            }
        );
    }

    #[test]
    fn repeated_refusals_fall_back_to_the_control_connection_until_the_stream_ends() {
        let mut r = registry(10);
        let t0 = Instant::now();
        let viewer = user("2", "neuralnomad", "NeuralNomad");
        r.ensure(&viewer, t0);
        let mut nick = r.get("2").unwrap().nick.clone();
        for _ in 1..MAX_NICK_REFUSALS {
            let Refusal::Renamed { nick: next, .. } = r.refuse_nick("2", &nick) else {
                panic!()
            };
            nick = next;
        }
        assert_eq!(
            r.refuse_nick("2", &nick),
            Refusal::GaveUp {
                display_name: "NeuralNomad".into()
            }
        );
        assert!(r.get("2").is_none());
        assert_eq!(r.ensure(&viewer, t0), Ensure::Fallback);
        r.drain();
        // Next stream: try again, skipping the nicks the server refused.
        let Ensure::Created { nick, .. } = r.ensure(&viewer, t0) else {
            panic!()
        };
        assert_eq!(
            nick,
            format!("NeuralNomad_{}|twitch", MAX_NICK_REFUSALS + 1)
        );
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
