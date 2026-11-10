//! Message transformation between IRC and Twitch.

use std::collections::HashMap;

use crate::config::Overflow;

/// Strips mIRC formatting control codes (bold, colour, italics, ...).
pub fn strip_irc_formatting(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\x02' | '\x0f' | '\x11' | '\x16' | '\x1d' | '\x1e' | '\x1f' => {}
            '\x03' => {
                // \x03[fg[,bg]] with fg/bg of 1-2 digits.
                let mut fg = false;
                for _ in 0..2 {
                    if chars.peek().is_some_and(char::is_ascii_digit) {
                        chars.next();
                        fg = true;
                    }
                }
                let mut look = chars.clone();
                // A background colour only counts after a foreground colour.
                if fg && look.next() == Some(',') && look.peek().is_some_and(char::is_ascii_digit) {
                    chars.next();
                    for _ in 0..2 {
                        if chars.peek().is_some_and(char::is_ascii_digit) {
                            chars.next();
                        }
                    }
                }
            }
            c => out.push(c),
        }
    }
    out
}

/// Splits a CTCP ACTION (`/me`) out of an IRC PRIVMSG body.
pub fn parse_action(text: &str) -> (bool, &str) {
    match text.strip_prefix("\x01ACTION ") {
        Some(rest) => (true, rest.strip_suffix('\x01').unwrap_or(rest)),
        None => (false, text),
    }
}

fn is_link(word: &str) -> bool {
    let w = word.to_ascii_lowercase();
    w.starts_with("http://") || w.starts_with("https://") || w.starts_with("www.")
}

pub fn strip_links(text: &str) -> String {
    text.split(' ')
        .map(|w| if is_link(w) { "<link>" } else { w })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Rewrites mentions of puppets (`NeuralNomad|twitch: hi`, `@NeuralNomad|twitch hi`,
/// `thanks NeuralNomad|twitch!`) to Twitch `@NeuralNomad` mentions, so the IRC-side suffix
/// never reaches Twitch. `puppets` maps case-folded puppet nick → the name Twitch notifies on
/// (the login, or the display name when it only differs in case). A nick that isn't in
/// `puppets` but fits `nick_template` (e.g. a puppet from before a restart) has the name
/// taken back out of the template.
pub fn rewrite_mentions(
    text: &str,
    puppets: &HashMap<String, String>,
    nick_template: &str,
) -> String {
    let mut words: Vec<String> = Vec::new();
    for (i, word) in text.split(' ').enumerate() {
        let trimmed = word.trim_end_matches([':', ',', '.', '!', '?', ';']);
        let trailing = &word[trimmed.len()..];
        let nick = trimmed.strip_prefix('@').unwrap_or(trimmed);
        let name = puppets
            .get(&crate::naming::casefold(nick))
            .cloned()
            .or_else(|| name_from_puppet_nick(nick, nick_template));
        match name {
            Some(display) => {
                // "Nick: hello" addressing → "@Nick hello".
                let trailing = if i == 0 && matches!(trailing, ":" | ",") {
                    ""
                } else {
                    trailing
                };
                words.push(format!("@{display}{trailing}"));
            }
            None => words.push(word.to_owned()),
        }
    }
    words.join(" ")
}

/// The Twitch name inside a puppet nick, by the nick template: `NeuralNomad|twitch` →
/// `NeuralNomad`, and `_42kilobytes|twitch` → `42kilobytes` (the `_` that's added because IRC
/// nicks can't start with a digit). `None` if `nick` doesn't fit the template.
fn name_from_puppet_nick(nick: &str, template: &str) -> Option<String> {
    let (before, after) = template.split_once("{name}")?;
    if before.is_empty() && after.is_empty() {
        return None; // every word would "fit"
    }
    let folded = crate::naming::casefold(nick);
    let (fb, fa) = (
        crate::naming::casefold(before),
        crate::naming::casefold(after),
    );
    if !folded.starts_with(&fb) || !folded.ends_with(&fa) || folded.len() <= fb.len() + fa.len() {
        return None;
    }
    let name = &nick[before.len()..nick.len() - after.len()];
    let name = match name.strip_prefix('_') {
        Some(rest) if rest.starts_with(|c: char| c.is_ascii_digit()) => rest,
        _ => name,
    };
    Some(name.to_owned())
}

/// The IRC nick in a message the bridge posted to Twitch (`[irc] mihok: hi`), given the
/// `to_twitch_format` templates it may have used. For turning a Twitch *reply* to that message
/// into a mention of the IRC user, instead of the bot it was posted by.
pub fn relayed_irc_nick(body: &str, templates: &[&str]) -> Option<String> {
    templates.iter().find_map(|template| {
        let template = template.replace("{name}", "{nick}");
        let (before, rest) = template.split_once("{nick}")?;
        let (between, _) = rest.split_once("{text}")?;
        let after_prefix = body.strip_prefix(before)?;
        let nick = if between.is_empty() {
            after_prefix.split(' ').next()?
        } else {
            after_prefix.split_once(between)?.0
        };
        (!nick.is_empty() && !nick.contains(' ')).then(|| nick.to_owned())
    })
}

/// Rewrites Twitch-style `@name` mentions of people in the IRC channel to their exact IRC
/// nick (`@Mihok hi` → `mihok: hi`), so IRC clients highlight them: many only match the nick
/// as written, and some not with a leading `@`. `irc_nick` looks up a channel member
/// case-insensitively; anything else (e.g. other Twitch viewers) is left as is.
pub fn rewrite_twitch_mentions(text: &str, irc_nick: impl Fn(&str) -> Option<String>) -> String {
    let mut words: Vec<String> = Vec::new();
    for (i, word) in text.split(' ').enumerate() {
        let Some(name) = word.strip_prefix('@') else {
            words.push(word.to_owned());
            continue;
        };
        let trimmed = name.trim_end_matches([':', ',', '.', '!', '?', ';']);
        let trailing = &name[trimmed.len()..];
        match irc_nick(trimmed) {
            // Addressing at the start ("@mihok hi") becomes IRC-style "mihok: hi".
            Some(nick) if i == 0 && (trailing.is_empty() || matches!(trailing, ":" | ",")) => {
                words.push(format!("{nick}:"))
            }
            Some(nick) => words.push(format!("{nick}{trailing}")),
            None => words.push(word.to_owned()),
        }
    }
    words.join(" ")
}

/// Renders `{nick}`/`{name}` and `{text}` into a template.
pub fn render(template: &str, nick: &str, text: &str) -> String {
    template
        .replace("{nick}", nick)
        .replace("{name}", nick)
        .replace("{text}", text)
}

/// Renders `text` with `template`, splitting or truncating so each line is at most
/// `max_len` characters (Twitch counts characters, not bytes).
pub fn render_bounded(
    template: &str,
    nick: &str,
    text: &str,
    max_len: usize,
    overflow: Overflow,
) -> Vec<String> {
    let overhead = render(template, nick, "").chars().count();
    let budget = max_len.saturating_sub(overhead).max(1);
    if text.chars().count() <= budget {
        return vec![render(template, nick, text)];
    }
    match overflow {
        Overflow::Truncate => {
            let cut: String = text.chars().take(budget.saturating_sub(1)).collect();
            vec![render(template, nick, &format!("{}…", cut.trim_end()))]
        }
        Overflow::Split => split_words(text, budget)
            .iter()
            .map(|c| render(template, nick, c))
            .collect(),
    }
}

/// Splits on spaces where possible, hard-splitting words longer than `budget`.
fn split_words(text: &str, budget: usize) -> Vec<String> {
    let mut chunks = Vec::new();
    let mut current = String::new();
    for word in text.split(' ') {
        let mut word: Vec<char> = word.chars().collect();
        loop {
            let cur_len = current.chars().count();
            let sep = usize::from(cur_len > 0);
            if cur_len + sep + word.len() <= budget {
                if sep == 1 {
                    current.push(' ');
                }
                current.extend(word);
                break;
            }
            if cur_len > 0 {
                chunks.push(std::mem::take(&mut current));
                continue;
            }
            // Word alone is longer than the budget.
            let rest = word.split_off(budget);
            chunks.push(word.into_iter().collect());
            word = rest;
        }
    }
    if !current.is_empty() {
        chunks.push(current);
    }
    chunks
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_formatting() {
        assert_eq!(
            strip_irc_formatting("\x02bold\x02 \x0304,12red\x03 \x0312blue\x0f"),
            "bold red blue"
        );
        assert_eq!(strip_irc_formatting("\x03,5 keep comma"), ",5 keep comma");
    }

    #[test]
    fn parses_actions() {
        assert_eq!(parse_action("\x01ACTION waves\x01"), (true, "waves"));
        assert_eq!(parse_action("hello"), (false, "hello"));
    }

    #[test]
    fn rewrites_mentions() {
        let map = HashMap::from([("neuralnomad|tw".to_string(), "NeuralNomad".to_string())]);
        let t = "{name}|tw";
        assert_eq!(
            rewrite_mentions("NeuralNomad|tw: try it", &map, t),
            "@NeuralNomad try it"
        );
        assert_eq!(
            rewrite_mentions("thanks neuralnomad|TW!", &map, t),
            "thanks @NeuralNomad!"
        );
        assert_eq!(rewrite_mentions("nobody here", &map, t), "nobody here");
    }

    #[test]
    fn strips_links() {
        assert_eq!(strip_links("see https://x.y/z now"), "see <link> now");
    }

    #[test]
    fn short_messages_are_not_split() {
        let out = render_bounded(
            "[irc] {nick}: {text}",
            "mihok",
            "hello",
            500,
            Overflow::Split,
        );
        assert_eq!(out, vec!["[irc] mihok: hello"]);
    }

    #[test]
    fn long_messages_split_on_words_within_limit() {
        let text = "word ".repeat(200);
        let out = render_bounded(
            "[irc] {nick}: {text}",
            "mihok",
            text.trim(),
            100,
            Overflow::Split,
        );
        assert!(out.len() > 1);
        for line in &out {
            assert!(line.chars().count() <= 100, "{line}");
            assert!(line.starts_with("[irc] mihok: word"));
        }
        let rejoined: Vec<&str> = out
            .iter()
            .map(|l| l.trim_start_matches("[irc] mihok: "))
            .collect();
        assert_eq!(rejoined.join(" "), text.trim());
    }

    #[test]
    fn long_words_are_hard_split() {
        let out = render_bounded("{text}", "n", &"x".repeat(120), 50, Overflow::Split);
        assert_eq!(
            out.iter().map(|l| l.len()).collect::<Vec<_>>(),
            vec![50, 50, 20]
        );
    }

    #[test]
    fn truncation_adds_ellipsis() {
        let out = render_bounded(
            "{nick}: {text}",
            "n",
            &"a ".repeat(100),
            60,
            Overflow::Truncate,
        );
        assert_eq!(out.len(), 1);
        assert!(out[0].chars().count() <= 60);
        assert!(out[0].ends_with('…'));
    }

    #[test]
    fn relayed_irc_nick_reads_the_bridges_own_format() {
        let t = ["[irc] {nick}: {text}", "[irc] * {nick} {text}"];
        assert_eq!(
            relayed_irc_nick("[irc] mihok: hi there", &t).as_deref(),
            Some("mihok")
        );
        assert_eq!(
            relayed_irc_nick("[irc] * sarah|away waves", &t).as_deref(),
            Some("sarah|away")
        );
        assert_eq!(relayed_irc_nick("just a viewer message", &t), None);
        assert_eq!(
            relayed_irc_nick("<{name}> x", &["<{name}> {text}"]),
            Some("{name}".into())
        );
    }

    #[test]
    fn twitch_mentions_of_irc_users_use_their_exact_nick() {
        let members = |n: &str| match n.to_ascii_lowercase().as_str() {
            "mihok" => Some("mihok".to_owned()),
            "sarahk" => Some("SarahK".to_owned()),
            _ => None,
        };
        assert_eq!(rewrite_twitch_mentions("@Mihok hi", members), "mihok: hi");
        assert_eq!(rewrite_twitch_mentions("@mihok, hi", members), "mihok: hi");
        assert_eq!(
            rewrite_twitch_mentions("thanks @sarahk!", members),
            "thanks SarahK!"
        );
        assert_eq!(
            rewrite_twitch_mentions("@SomeViewer gg", members),
            "@SomeViewer gg"
        );
        assert_eq!(
            rewrite_twitch_mentions("email me@x.org", members),
            "email me@x.org"
        );
        assert_eq!(rewrite_twitch_mentions("@mihok", members), "mihok:");
    }

    #[test]
    fn at_mentions_of_puppets_lose_the_suffix() {
        let map = HashMap::from([("neuralnomad|twitch".to_string(), "NeuralNomad".to_string())]);
        let t = "{name}|twitch";
        // IRC clients that complete mentions with "@".
        assert_eq!(
            rewrite_mentions("@NeuralNomad|twitch try it", &map, t),
            "@NeuralNomad try it"
        );
        assert_eq!(
            rewrite_mentions("@NeuralNomad|twitch: try it", &map, t),
            "@NeuralNomad try it"
        );
        assert_eq!(
            rewrite_mentions("nice one @neuralnomad|TWITCH!", &map, t),
            "nice one @NeuralNomad!"
        );
        // Puppets the bridge doesn't know (e.g. from before a restart): name from the nick.
        assert_eq!(
            rewrite_mentions(
                "@SomeViewer|twitch hi, _42kilobytes|twitch: hey",
                &HashMap::new(),
                t
            ),
            "@SomeViewer hi, @42kilobytes: hey"
        );
        // Not puppets: unchanged, @ included.
        assert_eq!(
            rewrite_mentions("@mihok |twitch is the suffix", &HashMap::new(), t),
            "@mihok |twitch is the suffix"
        );
        // A prefix template works too.
        assert_eq!(
            rewrite_mentions("@tw|NeuralNomad hi", &HashMap::new(), "tw|{name}"),
            "@NeuralNomad hi"
        );
    }
}
