# twitch-irc-bridge — design

Status: **draft**. Decisions below are agreed unless listed under *Open questions*.

## Goals

- Bridge Twitch chat for our biweekly stream with a dedicated channel on our community IRC server (Ergo).
- Twitch viewers appear on IRC as individual, clearly marked, restricted "puppet" users.
- IRC users appear on Twitch through a single relay bot account.
- The bridge is only active while we are live, so nobody can use it to get the bot in trouble between streams.
- Everything that varies by deployment is configurable.

## Non-goals

- Bridging any IRC channel other than the configured stream channel.
- Twitch chat history/backfill (Twitch provides none to bots; the bridge only sees messages while connected).
- Making IRC users into Twitch accounts (not possible; IRC → Twitch is always a relay bot).

## Background: Twitch chat

A Twitch channel's chat is a single, persistent room — it is not created per broadcast. Messages are delivered live
only; there is no history API. Chat is archived publicly as VOD "chat replay". Twitch recommends EventSub
(`channel.chat.message`) for reading and the Helix Send Chat Message API for sending. Bots sending via the API with
`user:bot` + broadcaster `channel:bot` can show the Chat Bot badge.

## Architecture

```
            EventSub WebSocket (read)                       one IRC connection per active puppet
Twitch  ─────────────────────────────►  ┌────────────┐  ───────────────────────────────────────►  Ergo
 chat   ◄─────────────────────────────  │   bridge   │  ◄───────────────────────────────────────  #stream
            Helix Send Chat Message     │   (Rust)   │      + one "control" connection (relay
                                        └────────────┘        listener, topic/mode management)
```

- **IRC side: option B (puppet client connections).** Ergo does not support server-to-server linking, so the
  pseudo-server approach is unavailable. Each Twitch viewer who *speaks* gets its own IRC client connection.
- **Twitch side:** one bot account (`[twitch].bot_login`) posts all IRC messages.
- The IRC side is behind a trait so a single-connection relay mode (option A) can be shipped first or used as a
  fallback without touching the Twitch side.

### Puppet authentication on Ergo

Our Ergo requires login (GitHub/Google). Puppets cannot do that, so they need their own path. Preferred:

1. **Bridge-minted JWTs (preferred).** Ergo ≥ 2.14 accepts JWT bearer tokens via SASL (`accounts.jwt-auth`). The
   bridge holds its own signing key, configured as a separate token issuer in Ergo, and mints a short-lived token
   per puppet. Ergo auto-creates the puppet account. No IP-based exemptions, so nothing is weakened for real users.
   Puppet account names end in the puppet suffix (e.g. `neuralnomad|tw`), which GitHub/Google-derived names cannot
   contain, so the namespace is naturally reserved.
2. **Fallback: IP-based `require-sasl` exemption** for the bridge's source address. Only safe if that address is
   not shared with any other client path. **Behind a Cloudflare Tunnel, real users may also appear to come from
   localhost — in that case this option must not be used.**

### Puppet lifecycle

- Created lazily on a viewer's first message while live; joins only the stream channel.
- Never joins other channels, never changes nick except to follow a Twitch rename, rejects PMs with an
  automatic "this user is on Twitch" notice.
- Quits after `idle_timeout`, or when the bridge goes offline.
- Capped at `max_puppets`; beyond that, messages fall back to relay-bot formatting.

## Naming

### Twitch → IRC (puppet nicks)

1. Base name: display name if it differs from the login only by case, otherwise the login.
2. Apply `nick_template` (default `{name}|tw`).
3. Leading digit → prefix `_`.
4. Truncate base to fit the server's `NICKLEN` (read from `RPL_ISUPPORT`).
5. Collision between truncated puppets → append `_2`, `_3`, …
6. Twitch rename (same user ID) → `NICK` change on next message.

| Twitch display / login / ID | IRC nick |
|---|---|
| `NeuralNomad` / `neuralnomad` / 412983377 | `NeuralNomad\|tw` |
| `42kilobytes` / `42kilobytes` / 98110234 | `_42kilobytes\|tw` |
| `データ太郎` / `datataro_jp` / 730552190 | `datataro_jp\|tw` |
| `the_quantum_gradient_desc` (NICKLEN 20) | `the_quantum_gradi\|tw` |

Realname: `Twitch viewer: https://twitch.tv/{login}`. The Twitch user ID is the bridge's primary key for every
viewer (bans, renames, account mapping).

### IRC → Twitch

```
[🤖] ourshow_irc: [irc] mihok: yes, we do. reconnect handling is the hard part
[🤖] ourshow_irc: [irc] * mihok drops the repo link in IRC
```

Mentions of puppets (`NeuralNomad|tw: hi`) are rewritten to `@NeuralNomad hi`.

## Lifecycle: live only

- `trigger = "auto"`: the process stays running and subscribes to EventSub `stream.online` / `stream.offline`
  (WebSocket transport, outbound only — nothing exposed publicly). Relaying starts on `stream.online` and stops
  `grace_period` after `stream.offline`.
- `trigger = "manual"`: an IRC op starts/stops it.
- While live: `live_topic` set, `join_notice` sent to everyone joining.
- While offline: `offline_topic` set, channel optionally `+m`, nothing relayed, all puppets quit.

## Moderation

- Twitch ban/timeout → puppet is kicked and that Twitch ID is blocked for the ban duration.
- IRC ops: `!tw block <nick>` / `!tw unblock <nick>` (by Twitch ID) and `!irc mute <nick>` (stop relaying an IRC
  user to Twitch). Bridge-level blocks are needed because all puppets may share one host/cloak.
- Twitch message deletions cannot be unsent on IRC; optionally post a notice.
- Outbound to Twitch: rate limit, 500-char cap (split or truncate), links optionally stripped, loop prevention
  (ignore the bridge's own messages on both sides).

## Configuration

```toml
[twitch]
channel   = "ourshow"
bot_login = "ourshow_irc"

[irc]
server  = "irc.example.org:6697"
channel = "#stream"

[lifecycle]
trigger           = "auto"   # or "manual"
grace_period      = "10m"
offline_moderated = true
live_topic    = "🔴 LIVE: everything said here is shown publicly on twitch.tv/ourshow and kept in the VOD chat replay"
offline_topic = "📴 Bridge offline: nothing here is relayed"
join_notice   = "Heads up: while live, messages in #stream are public on Twitch and archived."

[puppets]
nick_template     = "{name}|tw"
name_source       = "display_if_ascii"   # or "login"
ident_template    = "tw{id}"
realname_template = "Twitch viewer: https://twitch.tv/{login}"
idle_timeout      = "20m"
max_puppets       = 200
quit_message      = "Stream ended"
auth              = "jwt"                # or "exempt"

[relay]
to_twitch_format        = "[irc] {nick}: {text}"
to_twitch_action_format = "[irc] * {nick} {text}"
rewrite_mentions        = true
max_len                 = 500
overflow                = "split"        # or "truncate"
rate_limit              = "20/30s"
relay_links             = false
```

## Implementation notes (Rust)

- Async runtime: `tokio`.
- Twitch: `twitch_api` / `twitch_oauth2` for Helix + auth, EventSub over WebSocket (`tokio-tungstenite`).
- IRC: a lightweight client per puppet; needs SASL (bearer/JWT) and IRCv3 caps.
- State (Twitch ID ↔ nick/account, blocks): small embedded store (SQLite).

## Open questions

1. **How is Ergo deployed on Cloudflare?** (Cloudflare Tunnel / Spectrum in front of a VM, Cloudflare Containers,
   …) This decides where the bridge runs (ideally next to Ergo, connecting directly rather than through
   Cloudflare) and whether IP-based exemptions are safe at all.
2. **How is GitHub/Google login wired into Ergo?** (`accounts.oauth2`, `accounts.jwt-auth`, `auth-script`, or a web
   gateway in front.) Determines how the bridge's JWT issuer slots in.
3. Does our Ergo config use `force-nick-equals-account`? If so, puppet account names must equal their nicks
   (supported by the naming scheme above, but renames then create new accounts).
