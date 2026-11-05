//! Minimal IRC line handling: parsing, formatting and splitting.

/// A parsed IRC line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Message {
    pub prefix: Option<String>,
    pub command: String,
    pub params: Vec<String>,
}

impl Message {
    /// Nick part of the prefix (`nick!user@host`).
    pub fn nick(&self) -> Option<&str> {
        self.prefix
            .as_deref()
            .map(|p| p.split('!').next().unwrap_or(p))
    }

    pub fn param(&self, i: usize) -> &str {
        self.params.get(i).map(String::as_str).unwrap_or("")
    }
}

/// Parses one line (tags are skipped; the bridge doesn't use them).
pub fn parse(line: &str) -> Option<Message> {
    let mut rest = line.trim_end_matches(['\r', '\n']);
    if let Some(tagged) = rest.strip_prefix('@') {
        rest = tagged.split_once(' ')?.1.trim_start();
    }
    let mut prefix = None;
    if let Some(p) = rest.strip_prefix(':') {
        let (p, r) = p.split_once(' ')?;
        prefix = Some(p.to_owned());
        rest = r.trim_start();
    }
    let mut params = Vec::new();
    let command = match rest.split_once(' ') {
        Some((cmd, r)) => {
            rest = r;
            cmd
        }
        None => std::mem::take(&mut rest),
    };
    if command.is_empty() {
        return None;
    }
    while !rest.is_empty() {
        rest = rest.trim_start_matches(' ');
        if let Some(trailing) = rest.strip_prefix(':') {
            params.push(trailing.to_owned());
            break;
        }
        match rest.split_once(' ') {
            Some((p, r)) => {
                params.push(p.to_owned());
                rest = r;
            }
            None => {
                if !rest.is_empty() {
                    params.push(rest.to_owned());
                }
                break;
            }
        }
    }
    Some(Message {
        prefix,
        command: command.to_ascii_uppercase(),
        params,
    })
}

/// Removes characters that would break or inject IRC lines.
pub fn sanitize(text: &str) -> String {
    text.chars()
        .map(|c| {
            if matches!(c, '\r' | '\n' | '\0') {
                ' '
            } else {
                c
            }
        })
        .collect()
}

/// Bytes available for the text of a PRIVMSG/NOTICE to `target`, leaving room for the
/// `:nick!user@host ` prefix the server adds when relaying (512-byte line limit).
pub fn text_budget(target: &str) -> usize {
    const LINE: usize = 512 - 2; // CRLF
    const PREFIX_RESERVE: usize = 1 + 32 + 1 + 20 + 1 + 64 + 1; // :nick!user@host + space
    LINE - PREFIX_RESERVE - "PRIVMSG  :".len() - target.len()
}

/// Splits `text` into pieces of at most `max_bytes` bytes, on spaces where possible and
/// never inside a UTF-8 character.
pub fn split(text: &str, max_bytes: usize) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = text.trim();
    while rest.len() > max_bytes {
        let mut cut = max_bytes;
        while !rest.is_char_boundary(cut) {
            cut -= 1;
        }
        // Break at the last space unless the cut already falls on one.
        if rest.as_bytes()[cut] != b' '
            && let Some(space) = rest[..cut].rfind(' ').filter(|&i| i > 0)
        {
            cut = space;
        }
        out.push(rest[..cut].trim_end().to_owned());
        rest = rest[cut..].trim_start();
    }
    if !rest.is_empty() {
        out.push(rest.to_owned());
    }
    out
}

/// PRIVMSG lines for `text` (CTCP ACTION when `action`), split to fit.
pub fn privmsgs(target: &str, text: &str, action: bool) -> Vec<String> {
    let overhead = if action { "\x01ACTION \x01".len() } else { 0 };
    split(&sanitize(text), text_budget(target) - overhead)
        .into_iter()
        .map(|chunk| {
            if action {
                format!("PRIVMSG {target} :\x01ACTION {chunk}\x01")
            } else {
                format!("PRIVMSG {target} :{chunk}")
            }
        })
        .collect()
}

pub fn notices(target: &str, text: &str) -> Vec<String> {
    split(&sanitize(text), text_budget(target))
        .into_iter()
        .map(|chunk| format!("NOTICE {target} :{chunk}"))
        .collect()
}

/// `PREFIX`/`CHANMODES` from ISUPPORT, used to follow channel-op status.
#[derive(Debug, Clone)]
pub struct ModeInfo {
    /// Prefix mode letters, highest first (e.g. `qaohv`), and their symbols (`~&@%+`).
    pub prefix_modes: String,
    pub prefix_symbols: String,
    /// Modes that take a parameter when set (`CHANMODES` A, B and C).
    pub param_on_set: String,
    /// Modes that take a parameter when unset (`CHANMODES` A and B).
    pub param_on_unset: String,
}

impl Default for ModeInfo {
    fn default() -> Self {
        // Ergo's defaults.
        Self {
            prefix_modes: "qaohv".into(),
            prefix_symbols: "~&@%+".into(),
            param_on_set: "Ibekfl".into(),
            param_on_unset: "Ibek".into(),
        }
    }
}

impl ModeInfo {
    /// Applies `PREFIX=(qaohv)~&@%+` / `CHANMODES=A,B,C,D` tokens from an RPL_ISUPPORT line.
    pub fn apply_isupport(&mut self, token: &str) {
        if let Some(v) = token.strip_prefix("PREFIX=") {
            if let Some((modes, symbols)) = v.strip_prefix('(').and_then(|v| v.split_once(')')) {
                self.prefix_modes = modes.into();
                self.prefix_symbols = symbols.into();
            }
        } else if let Some(v) = token.strip_prefix("CHANMODES=") {
            let groups: Vec<&str> = v.split(',').collect();
            if groups.len() >= 3 {
                self.param_on_unset = format!("{}{}", groups[0], groups[1]);
                self.param_on_set = format!("{}{}{}", groups[0], groups[1], groups[2]);
            }
        }
    }

    /// Prefix symbols that count as channel operator (op and above).
    pub fn op_symbols(&self) -> &str {
        let n = self.prefix_modes.find('o').map_or(1, |i| i + 1);
        &self.prefix_symbols[..n.min(self.prefix_symbols.len())]
    }

    fn op_modes(&self) -> &str {
        let n = self.prefix_modes.find('o').map_or(1, |i| i + 1);
        &self.prefix_modes[..n.min(self.prefix_modes.len())]
    }

    /// Walks a channel MODE change (`+o-v alice bob`) and returns `(nick, is_op_mode, adding)`
    /// for every prefix-mode change.
    pub fn prefix_changes<'a>(
        &self,
        modes: &str,
        args: &'a [String],
    ) -> Vec<(&'a str, bool, bool)> {
        let mut out = Vec::new();
        let mut args = args.iter();
        let mut adding = true;
        for c in modes.chars() {
            match c {
                '+' => adding = true,
                '-' => adding = false,
                c if self.prefix_modes.contains(c) => {
                    if let Some(nick) = args.next() {
                        out.push((nick.as_str(), self.op_modes().contains(c), adding));
                    }
                }
                c if (adding && self.param_on_set.contains(c))
                    || (!adding && self.param_on_unset.contains(c)) =>
                {
                    args.next();
                }
                _ => {}
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_lines() {
        let m = parse("@time=x :alice!~u@host PRIVMSG #stream :hello there").unwrap();
        assert_eq!(m.nick(), Some("alice"));
        assert_eq!(m.command, "PRIVMSG");
        assert_eq!(m.params, vec!["#stream", "hello there"]);
        assert_eq!(parse("PING :abc").unwrap().params, vec!["abc"]);
        assert_eq!(
            parse(":srv 001 me :Welcome").unwrap().params,
            vec!["me", "Welcome"]
        );
        assert_eq!(
            parse(":srv MODE #c +o  bob").unwrap().params,
            vec!["#c", "+o", "bob"]
        );
        assert!(parse("").is_none());
    }

    #[test]
    fn splits_on_spaces_and_char_boundaries() {
        assert_eq!(split("aaa bbb ccc", 7), vec!["aaa bbb", "ccc"]);
        assert_eq!(split("abcdefghij", 4), vec!["abcd", "efgh", "ij"]);
        let jp = "データ太郎".repeat(10);
        for piece in split(&jp, 10) {
            assert!(piece.len() <= 10);
        }
        assert_eq!(split(&jp, 10).concat(), jp);
    }

    #[test]
    fn long_twitch_message_fits_irc_lines() {
        let text = "データ ".repeat(200); // ~2000 bytes
        let lines = privmsgs("#stream", &text, false);
        assert!(lines.len() > 1);
        for l in &lines {
            assert!(l.len() + 100 <= 510, "{} bytes", l.len());
        }
    }

    #[test]
    fn sanitizes_newlines() {
        assert_eq!(
            privmsgs("#s", "a\r\nQUIT :x", false),
            vec!["PRIVMSG #s :a  QUIT :x"]
        );
    }

    #[test]
    fn actions_are_ctcp() {
        assert_eq!(
            privmsgs("#s", "waves", true),
            vec!["PRIVMSG #s :\x01ACTION waves\x01"]
        );
    }

    #[test]
    fn mode_changes_track_ops() {
        let mut info = ModeInfo::default();
        info.apply_isupport("PREFIX=(qaohv)~&@%+");
        info.apply_isupport("CHANMODES=Ibe,k,fl,CEMRUimnstu");
        let args: Vec<String> = ["*!*@spam", "alice", "bob", "5"].map(String::from).to_vec();
        assert_eq!(
            info.prefix_changes("+bo-v+l", &args),
            vec![("alice", true, true), ("bob", false, false)]
        );
        assert_eq!(info.op_symbols(), "~&@");
    }
}
