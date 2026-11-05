# twitch-irc-bridge: design

Status: **draft**. Decisions below are agreed unless listed under *Open questions*.

## Goals

- Bridge Twitch chat for our biweekly stream with a dedicated channel (`#stream` by default) on our community IRC
  server, [slashcheer-irc](https://github.com/mihok/slashcheer-irc) (Ergo on Cloudflare).
- Twitch viewers appear on IRC as individual, clearly marked, restricted "puppet" users.
- IRC users appear on Twitch through a single relay bot account.
- The bridge only relays while we are live, so nobody can use it to get the bot in trouble between streams.
- Everything that varies by deployment is configurable (`config.example.toml`).

## Non-goals

- Bridging any IRC channel other than the configured stream channel.
- Twitch chat history/backfill. Twitch provides none to bots; the bridge only sees messages while connected.
- Making IRC users into Twitch accounts. That's impossible, so IRC → Twitch is always a relay bot.

## Background: Twitch chat

A Twitch channel's chat is a single, persistent room. It is not created per broadcast. Messages are delivered live
only; there is no history API. Chat is archived publicly as VOD "chat replay". Twitch recommends EventSub
(`channel.chat.message`) for reading and the Helix Send Chat Message API for sending. Bots sending via the API with
`user:bot` and the broadcaster's `channel:bot` can show the Chat Bot badge.

## Background: slashcheer-irc

Facts from the slashcheer-irc repo that shape this design:

- Ergo runs in a Cloudflare Container. The only way in is **IRC over WebSocket** (`wss://<host>/web/irc`) through
  the Worker. There is no raw TCP.
- Cloudflare Access fronts everything. Bots and bridges get an **Access service token** (`bot_service_tokens`),
  bound to one IRC account with `irc-admin.sh bind-bot <bot> <account>`.
- The Worker's `IrcGate` logs every connection in as the account bound to its identity (SASL `IRCV3BEARER`, 60 s
  JWT minted by the Worker). Ergo has `require-sasl`, `jwt-auth.autocreate: false` and
  **`force-nick-equals-account: true`**, so every distinct nick needs its own account.
- Ergo: `nicklen 32`; idents are coerced to `~u`; IPs are cloaked per /32 (IPv4) or /64 (IPv6); per-IP limits are
  8 concurrent connections and 24 connections per 10 minutes. The Worker sets the client IP Ergo sees.
- WAF: `websocket_connects_per_10s` (default 10) per client IP.
- The container has **no internet egress**, so the bridge can't run inside the Ergo container.

## Architecture

```
               EventSub WebSocket (read)                     wss://<host>/web/irc  + Access service token
  Twitch  ─────────────────────────────►  ┌────────────┐  ─────────────────────────────────────────────►  Worker ─► Ergo
   chat   ◄─────────────────────────────  │   bridge   │      control connection (bound bot account)       IrcGate   #stream
               Helix Send Chat Message    │   (Rust)   │      + one connection per active puppet
                                          └────────────┘
```

- **IRC side: puppet client connections.** Ergo can't link servers, so each Twitch viewer who *speaks* gets their
  own IRC-over-WebSocket connection. A **control connection** (the bot account bound to the bridge's service token)
  sits in the channel, needs channel-op status, and handles topic, `+m`, notices and op commands.
- **Twitch side:** one bot account (`twitch.bot_login`) posts all IRC messages.
- The core (`src/bridge.rs`) is transport-agnostic: backends implement `TwitchOut` / `IrcOut` and feed `Event`s
  in. With `puppets.enabled = false` the bridge runs as a single relay bot. That mode works with slashcheer-irc
  **today** (only `bind-bot` needed), so it's the first milestone.

### slashcheer-irc changes needed for puppets

Puppets can't use the existing bot path: one service token is bound to exactly one account, and nick must equal
account. Proposed changes in slashcheer-irc:

1. **Puppet mode for a bridge token.** Mark a service token as a bridge, e.g. `irc-admin.sh bind-bridge
   twitchbridge twbridge '|tw'`. Such a token may open `/web/irc` with a header like `X-Slashcheer-Puppet:
   NeuralNomad|tw`. The Worker:
   - checks that the nick matches the bridge's reserved suffix,
   - creates the account through the admin API if it doesn't exist,
   - has IrcGate log in as it.

   The bridge never sees the JWT key.
2. **Reserve the namespace.** The signup regex (`ACCOUNT_RE`) currently allows `|`, so a person could claim
   `foo|tw`. Signup must reject names matching any bridge suffix.
3. **A distinct client IP per puppet.** Otherwise all puppets share the bridge's IP, so they would:
   - share one cloak, which makes host bans useless,
   - hit Ergo's 8-concurrent / 24-per-10-minutes limits.

   For puppet connections the Worker should set a synthetic address with the Twitch ID in the /64 prefix, e.g.
   ID 412983377 → `fd74:0:189d:a051::1`. Ergo then cloaks and rate-limits each viewer separately, and bans by
   cloak keep working when they rename.
4. **WAF.** Exempt the bridge token from `websocket_connects_per_10s`, or the bridge creates puppets slowly. Puppets
   are lazy, so bursts are small.

### Where the bridge runs

It needs a long-running process with outbound access to Twitch and to the IRC host. Options:
- a second Cloudflare Container (the Ergo container has no egress, but a separate one can),
- any small VM.

It connects through the public hostname like any bot, so it doesn't need to sit next to Ergo.

### Puppet lifecycle

- Created lazily on a viewer's first message while live; joins only the stream channel.
- Never joins other channels. Nick changes only follow a Twitch rename; under `force-nick-equals-account` the
  backend does that as quit + reconnect.
- Private messages to a puppet get an automatic notice: "this user is a Twitch viewer".
- Quits after `idle_timeout`, or when the bridge goes offline.
- Capped at `max_puppets`; beyond that, messages fall back to relay-bot formatting.

## Naming

### Twitch → IRC (puppet nicks), `src/naming.rs`

1. Base name: display name if it differs from the login only by case, otherwise the login.
2. Apply `nick_template` (default `{name}|tw`).
3. Leading digit → prefix `_`.
4. Truncate the base to fit NICKLEN (config, lowered by the server's ISUPPORT).
5. Collision between puppets → `_2`, `_3`, …
6. Twitch rename (same user ID) → puppet renames on their next message.

| Twitch display / login / ID | IRC nick |
|---|---|
| `NeuralNomad` / `neuralnomad` / 412983377 | `NeuralNomad\|tw` |
| `42kilobytes` / `42kilobytes` / 98110234 | `_42kilobytes\|tw` |
| `データ太郎` / `datataro_jp` / 730552190 | `datataro_jp\|tw` |
| `the_quantum_gradient_desc` (NICKLEN 20) | `the_quantum_gradi\|tw` |

Realname: `Twitch viewer: https://twitch.tv/{login}`. The Twitch user ID is the bridge's key for every viewer.

### IRC → Twitch, `src/relay.rs`

```
[🤖] ourshow_irc: [irc] mihok: yes, we do. reconnect handling is the hard part
[🤖] ourshow_irc: [irc] * mihok drops the repo link in IRC
```

- IRC formatting codes are stripped.
- Links are replaced with `<link>` unless `relay_links = true`.
- Puppet mentions (`NeuralNomad|tw: hi`) become `@NeuralNomad hi`.
- Long messages are split on word boundaries, or truncated, to Twitch's 500 characters.

## Lifecycle: live only, `src/lifecycle.rs`

- `trigger = "auto"`: the process stays running and listens for EventSub `stream.online` / `stream.offline`
  (WebSocket transport, outbound only). Relaying starts on `stream.online` and stops `grace_period` after
  `stream.offline`. Going live again during the grace period cancels it.
- `trigger = "manual"`: only ops start and stop it. `!bridge start|stop` also works as an override in auto mode.
- On going live: `live_topic`, channel `-m`, and `join_notice` to everyone who joins.
- On going offline: all puppets quit, `offline_topic`, channel `+m` (if `offline_moderated`).

## Moderation, `src/moderation.rs`

- A Twitch ban or timeout removes the puppet and blocks that Twitch ID for the duration.
- IRC op commands, answered by notice:
  - `!bridge start|stop|status`
  - `!tw block <puppet>` / `!tw unblock <twitch-id>`
  - `!irc mute|unmute <nick>` (stop relaying an IRC user to Twitch)
- Rate limiting to Twitch is a sliding window (`rate_limit`, default `20/30s`). The sender is told when a message
  is dropped.
- Loop prevention: the bot's own Twitch messages and the bridge's own IRC nicks are ignored.
- Message deletions on Twitch can't be unsent on IRC (not handled yet).

## Implementation status

| Piece | Status |
|---|---|
| Config, naming, relay formatting, rate limit, lifecycle, moderation, bridge core | Implemented, unit tested |
| Twitch backend (EventSub WebSocket + Helix send) | Stub (`src/twitch.rs`) |
| IRC backend (IRC over WebSocket + Access token, control connection) | Stub (`src/irc.rs`) |
| Puppet connections | Blocked on the slashcheer-irc changes above |

## Open questions

1. Agree the slashcheer-irc puppet-mode changes, especially the header-based puppet login and synthetic per-puppet
   IPs.
2. Where to host the bridge: a second Cloudflare Container or a VM.
3. Twitch app registration and bot account (name, who owns them).
