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
  in.
- There is no relay-bot mode: puppets are the only way Twitch viewers appear. The control connection only speaks
  for viewers when `max_puppets` is reached.

### Puppet mode in slashcheer-irc

Implemented in slashcheer-irc (see its README, "Bridges and puppet users"). Setup:

1. Add the bot to both `bot_service_tokens` and `bridge_bots` in Terraform, and apply.
2. Bind it and mark it as a bridge:
   ```bash
   scripts/irc-admin.sh bind-bot twitch-bridge twbridge
   ```
   ```bash
   scripts/irc-admin.sh bind-bridge twitch-bridge '|twitch' '#stream'
   ```

How it works:

- **Puppet login.** The bridge opens `/web/irc` with its service token plus `X-Slashcheer-Puppet: NeuralNomad|twitch`
  and `X-Slashcheer-Puppet-Id: <twitch user id>`. The Worker:
  - checks the suffix,
  - creates the account on first use,
  - has IrcGate log in as it.

  The bridge never sees the JWT key.
- **Reserved names.** Signup rejects names ending in a bridge's suffix. Suffixes can't overlap.
- **Own address per viewer.** Each puppet connects from a synthetic unique-local IPv6 /64, a hash of (bridge,
  Twitch ID). Each viewer gets their own cloak and their own Ergo per-IP limits, and cloak bans survive Twitch
  renames.
- **Restricted sessions.** Puppets can only join and talk in the bridge's channels: no private messages, services,
  other channels or nick changes.
- **Edge rate limit.** `bridge_bots` tokens skip the per-IP websocket connect rate limit.
- **Bans.** `irc-admin.sh ban NeuralNomad|twitch` bans that Twitch ID server-side, across renames.

### Where the bridge runs

It needs a long-running process with outbound access to Twitch and to the IRC host. The `Dockerfile` builds a small
image for either option:
- **Any small VM** with Docker: simplest. The refresh token persists in a volume.
- **A second Cloudflare Container** (the Ergo container has no egress, but a separate one can). **Chosen**:
  slashcheer-irc's Terraform builds this repo at a pinned commit and runs it next to Ergo (`twitch_bridge`). A
  Durable Object owns it and the cron keeps it running and registers the bridge in the directory. The config comes
  in as `BRIDGE_CONFIG`. Its disk isn't persistent, so the refresh token is seeded from the environment on every
  boot (Twitch rarely rotates refresh tokens for confidential clients; if it does, update the Terraform credential).

It connects through the public hostname like any bot, so it doesn't need to sit next to Ergo.

### Puppet lifecycle

- Created lazily on a viewer's first message while live; joins only the stream channel.
- Never joins other channels. Nick changes only follow a Twitch rename; under `force-nick-equals-account` the
  backend does that as quit + reconnect.
- Private messages to a puppet get an automatic notice: "this user is a Twitch viewer".
- Quits after `idle_timeout`, or when the bridge goes offline.
- Capped at `max_puppets`; beyond that, messages fall back to relay-bot formatting.
- A puppet nick belongs, server-side, to the Twitch ID that first used it. When someone else later has that Twitch
  name (renames free names up), slashcheer-irc answers the puppet login with **409** and the bridge reconnects
  the viewer under the next free nick (`NeuralNomad_2|twitch`), resending what they'd said. After 5 refused nicks
  for one viewer it relays them with relay-bot formatting until the stream ends.

## Naming

### Twitch → IRC (puppet nicks), `src/naming.rs`

1. Base name: display name if it differs from the login only by case, otherwise the login.
2. Apply `nick_template` (default `{name}|twitch`).
3. Leading digit → prefix `_`.
4. Truncate the base to fit NICKLEN (config, lowered by the server's ISUPPORT).
5. Collision between puppets → `_2`, `_3`, …
6. Twitch rename (same user ID) → puppet renames on their next message.

| Twitch display / login / ID | IRC nick |
|---|---|
| `NeuralNomad` / `neuralnomad` / 412983377 | `NeuralNomad\|twitch` |
| `42kilobytes` / `42kilobytes` / 98110234 | `_42kilobytes\|twitch` |
| `データ太郎` / `datataro_jp` / 730552190 | `datataro_jp\|twitch` |
| `the_quantum_gradient_desc` (NICKLEN 20) | `the_quantum_g\|twitch` |

Realname: `Twitch viewer: https://twitch.tv/{login}`. The Twitch user ID is the bridge's key for every viewer.

### IRC → Twitch, `src/relay.rs`

```
[🤖] ourshow_irc: [irc] mihok: yes, we do. reconnect handling is the hard part
[🤖] ourshow_irc: [irc] * mihok drops the repo link in IRC
```

- IRC formatting codes are stripped.
- Links are replaced with `<link>` unless `relay_links = true`.
- Puppet mentions (`NeuralNomad|twitch: hi`) become `@NeuralNomad hi`.
- Long messages are split on word boundaries, or truncated, to Twitch's 500 characters.

## Lifecycle: live only, `src/lifecycle.rs`

- `trigger = "auto"`: the process stays running and listens for EventSub `stream.online` / `stream.offline`
  (WebSocket transport, outbound only). Relaying starts on `stream.online` and stops `grace_period` after
  `stream.offline`. Going live again during the grace period cancels it.
- `trigger = "manual"`: only ops start and stop it. `!bridge start|stop` also works as an override in auto mode.
- On going live: `live_topic`, channel `-m`, and `join_notice` to everyone who joins.
- On going offline: all puppets quit, `offline_topic`, channel `+m` (if `offline_moderated`).

## Moderation, `src/moderation.rs`

- A Twitch ban or timeout removes the puppet. Twitch reports both as `channel.chat.clear_user_messages`, which the
  bot's own `user:read:chat` scope covers; Twitch itself stops the user chatting, so the bridge doesn't block them.
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
| Twitch backend (EventSub WebSocket + Helix send), `src/twitch/` | Implemented; tested against a mock Twitch |
| IRC backend (IRC over WebSocket + Access token, control and puppet connections), `src/irc/` | Implemented; tested against a fake websocket IRC server |
| Full stack (core + both backends) | `src/e2e_tests.rs`, against the same mocks |
| slashcheer-irc puppet mode | [mihok/slashcheer-irc#1](https://github.com/mihok/slashcheer-irc/pull/1), merged |
| Hosting | slashcheer-irc Terraform (`twitch_bridge`): second Cloudflare Container, config via `BRIDGE_CONFIG` |
| Against real Twitch and slashcheer-irc | Not yet run |

### Twitch details

- **Subscriptions**, over one EventSub WebSocket session with the bot's user token:
  - `channel.chat.message` and `channel.chat.clear_user_messages` (condition: broadcaster + bot)
  - `stream.online` and `stream.offline`
- **Startup and resubscribing.** After every fresh session the bridge asks Helix whether the stream is live, so a
  `stream.online` missed while disconnected isn't lost. `session_reconnect` hands over to the new URL without
  resubscribing.
- **Ignored messages:** duplicate notifications (by message id), and shared-chat messages from other channels.
- **Actions:** `/me` messages arrive as plain text; EventSub doesn't flag them.

## Open questions

1. ~~Where to host the bridge~~: a second Cloudflare Container, deployed by slashcheer-irc's Terraform.
2. Twitch app registration and bot account (name, who owns them).
