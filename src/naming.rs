//! Turning Twitch identities into IRC puppet nicks.
//!
//! Rules (see DESIGN.md, "Naming"):
//! 1. Base name: display name if it only differs from the login by case, else the login.
//! 2. Apply the nick template (default `{name}|tw`).
//! 3. A nick can't start with a digit or `-`: prefix `_`.
//! 4. Truncate the base so the nick fits NICKLEN.
//! 5. Collisions get `_2`, `_3`, ... (the base is truncated further if needed).

use anyhow::{Result, bail};

use crate::config::NameSource;

/// A Twitch chatter. `id` is the stable key; `login` and `display_name` can change.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TwitchUser {
    pub id: String,
    pub login: String,
    pub display_name: String,
}

/// Characters allowed anywhere in an RFC 1459/2812 nick, besides ASCII letters.
const SPECIAL: &[char] = &['[', ']', '\\', '`', '_', '^', '{', '|', '}'];

fn valid_first(c: char) -> bool {
    c.is_ascii_alphabetic() || SPECIAL.contains(&c)
}

fn valid_rest(c: char) -> bool {
    valid_first(c) || c.is_ascii_digit() || c == '-'
}

/// Case-fold a nick for comparisons (RFC 1459 casemapping, a superset of ASCII).
pub fn casefold(nick: &str) -> String {
    nick.chars()
        .map(|c| match c {
            '[' => '{',
            ']' => '}',
            '\\' => '|',
            '~' => '^',
            c => c.to_ascii_lowercase(),
        })
        .collect()
}

/// Checks the literal (non-`{name}`) parts of a template are valid nick characters
/// and leave room for a name.
pub fn validate_template(template: &str, nicklen: usize) -> Result<()> {
    let literal: String = template.replace("{name}", "");
    if let Some(bad) = literal.chars().find(|c| !valid_rest(*c)) {
        bail!("nick template contains {bad:?}, which isn't valid in an IRC nick");
    }
    // Leave room for a leading `_`, a `_NN` collision suffix and at least 4 name characters.
    if literal.chars().count() + 1 + 3 + 4 > nicklen {
        bail!("nick template leaves too little room for a name with nicklen {nicklen}");
    }
    Ok(())
}

pub fn base_name(user: &TwitchUser, source: NameSource) -> String {
    let raw = match source {
        NameSource::DisplayIfAscii if user.display_name.eq_ignore_ascii_case(&user.login) => {
            &user.display_name
        }
        _ => &user.login,
    };
    // Twitch logins are [a-z0-9_]; sanitise anyway so the output is always a valid nick.
    let cleaned: String = raw
        .chars()
        .map(|c| if valid_rest(c) { c } else { '_' })
        .collect();
    if cleaned.is_empty() {
        format!("tw{}", user.id)
    } else {
        cleaned
    }
}

/// Builds the puppet nick. `taken` reports whether a (case-folded) nick is already in use
/// by someone else.
pub fn puppet_nick(
    user: &TwitchUser,
    source: NameSource,
    template: &str,
    nicklen: usize,
    taken: impl Fn(&str) -> bool,
) -> String {
    let base = base_name(user, source);
    for n in 1u32.. {
        let suffix = if n == 1 {
            String::new()
        } else {
            format!("_{n}")
        };
        let nick = render(&base, &suffix, template, nicklen);
        if !taken(&casefold(&nick)) {
            return nick;
        }
    }
    unreachable!()
}

fn render(base: &str, suffix: &str, template: &str, nicklen: usize) -> String {
    let literal_len = template.replace("{name}", "").chars().count();
    let starts_with_name = template.starts_with("{name}");
    let needs_prefix = starts_with_name && base.chars().next().is_some_and(|c| !valid_first(c));
    let prefix = if needs_prefix { "_" } else { "" };
    let budget = nicklen.saturating_sub(literal_len + prefix.len() + suffix.len());
    let truncated: String = base.chars().take(budget).collect();
    template.replace("{name}", &format!("{prefix}{truncated}{suffix}"))
}

/// Renders `{login}`, `{display}` and `{id}`.
pub fn render_user_template(template: &str, user: &TwitchUser) -> String {
    template
        .replace("{login}", &user.login)
        .replace("{display}", &user.display_name)
        .replace("{id}", &user.id)
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

    fn nick(u: &TwitchUser) -> String {
        puppet_nick(u, NameSource::DisplayIfAscii, "{name}|tw", 32, |_| false)
    }

    #[test]
    fn examples_from_design_doc() {
        assert_eq!(
            nick(&user("412983377", "neuralnomad", "NeuralNomad")),
            "NeuralNomad|tw"
        );
        assert_eq!(
            nick(&user("98110234", "42kilobytes", "42kilobytes")),
            "_42kilobytes|tw"
        );
        assert_eq!(
            nick(&user("730552190", "datataro_jp", "データ太郎")),
            "datataro_jp|tw"
        );
    }

    #[test]
    fn truncates_to_nicklen() {
        let u = user(
            "1",
            "the_quantum_gradient_desc",
            "the_quantum_gradient_desc",
        );
        assert_eq!(
            puppet_nick(&u, NameSource::Login, "{name}|tw", 20, |_| false),
            "the_quantum_gradi|tw"
        );
        assert_eq!(nick(&u), "the_quantum_gradient_desc|tw");
    }

    #[test]
    fn login_source_ignores_display_name() {
        let u = user("1", "neuralnomad", "NeuralNomad");
        assert_eq!(
            puppet_nick(&u, NameSource::Login, "{name}|tw", 32, |_| false),
            "neuralnomad|tw"
        );
    }

    #[test]
    fn prefix_template_needs_no_underscore() {
        let u = user("1", "42kilobytes", "42kilobytes");
        assert_eq!(
            puppet_nick(&u, NameSource::Login, "tw|{name}", 32, |_| false),
            "tw|42kilobytes"
        );
    }

    #[test]
    fn collisions_get_numbered_and_respect_nicklen() {
        let u = user(
            "1",
            "the_quantum_gradient_desc",
            "the_quantum_gradient_desc",
        );
        let taken = |n: &str| n == "the_quantum_gradi|tw";
        assert_eq!(
            puppet_nick(&u, NameSource::Login, "{name}|tw", 20, taken),
            "the_quantum_gra_2|tw"
        );
    }

    #[test]
    fn collision_check_is_case_insensitive() {
        let u = user("1", "neuralnomad", "NeuralNomad");
        let taken = |n: &str| n == casefold("neuralnomad|TW");
        assert_eq!(nick_with(&u, taken), "NeuralNomad_2|tw");
    }

    fn nick_with(u: &TwitchUser, taken: impl Fn(&str) -> bool) -> String {
        puppet_nick(u, NameSource::DisplayIfAscii, "{name}|tw", 32, taken)
    }

    #[test]
    fn casefold_uses_rfc1459() {
        assert_eq!(casefold("Foo[TW]\\"), "foo{tw}|");
    }

    #[test]
    fn template_validation() {
        assert!(validate_template("{name}|tw", 32).is_ok());
        assert!(validate_template("{name}@tw", 32).is_err());
        assert!(validate_template("{name}|twitchviewer", 16).is_err());
    }
}
