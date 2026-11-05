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

/// Rewrites mentions of puppets (`NeuralNomad|tw: hi`, `thanks NeuralNomad|tw!`) to Twitch
/// `@NeuralNomad` mentions. `puppets` maps case-folded puppet nick → Twitch display name.
pub fn rewrite_mentions(text: &str, puppets: &HashMap<String, String>) -> String {
    let mut words: Vec<String> = Vec::new();
    for (i, word) in text.split(' ').enumerate() {
        let trimmed = word.trim_end_matches([':', ',', '.', '!', '?', ';']);
        let trailing = &word[trimmed.len()..];
        match puppets.get(&crate::naming::casefold(trimmed)) {
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
        assert_eq!(
            rewrite_mentions("NeuralNomad|tw: try it", &map),
            "@NeuralNomad try it"
        );
        assert_eq!(
            rewrite_mentions("thanks neuralnomad|TW!", &map),
            "thanks @NeuralNomad!"
        );
        assert_eq!(rewrite_mentions("nobody here", &map), "nobody here");
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
}
