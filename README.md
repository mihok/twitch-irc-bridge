# twitch-irc-bridge

Bridges our Twitch stream chat with a dedicated IRC channel on
[slashcheer-irc](https://github.com/mihok/slashcheer-irc), only while we're live. Twitch viewers show up on IRC as
`name|twitch` puppet users; IRC users show up on Twitch through one bot account.

See [DESIGN.md](DESIGN.md) for the design and current status.

## Setup

### 1. Twitch

Hosted by slashcheer-irc? Only register the app (step 1, with the redirect URL
`https://<irc host>/admin/twitch/callback`), then connect the bot account with **Connect Twitch bot** on the
site's `/admin/` page; steps 2–3 below are for self-hosting.


1. **Register an app** at <https://dev.twitch.tv/console/apps> (confidential client). Note its client id and secret.
2. **Authorize the bot account** (`twitch.bot_login`) for the app with the scopes
   `user:read:chat user:write:chat user:bot`, and keep the **refresh token**. Any OAuth authorization-code tool
   works, e.g. the [Twitch CLI](https://dev.twitch.tv/docs/cli/):
   ```bash
   twitch token -u -s 'user:read:chat user:write:chat user:bot'
   ```
   The Twitch CLI needs `http://localhost:3000` as a redirect URL on the app, and signs in as whoever is logged
   in to Twitch in your browser, so use the bot account there.
3. **Chat Bot badge** (`send_as = "app"`, the default): either make the bot a moderator in the channel, or have the
   broadcaster authorize the app once with the `channel:bot` scope.

### 2. slashcheer-irc

**Hosted by slashcheer-irc (recommended).** Set `twitch_bridge` and `twitch_bridge_credentials` in slashcheer-irc's
Terraform and apply: it runs this bridge as a second Cloudflare Container, creates its service token, binds it to
the control account and marks it as a bridge, so steps 1–2 below and section 3 are done for you. Only step 3 (channel
operator) is still manual. See "Twitch bridge" in slashcheer-irc's README.

**Self-hosted.** Needs puppet mode ([mihok/slashcheer-irc#1](https://github.com/mihok/slashcheer-irc/pull/1), merged).

1. Add a bot token (e.g. `twitch-bridge`) to `bot_service_tokens` and `bridge_bots`, and apply.
2. Bind it to the control account and mark it as a bridge:
   ```bash
   scripts/irc-admin.sh bind-bot twitch-bridge twbridge
   ```
   ```bash
   scripts/irc-admin.sh bind-bridge twitch-bridge '|twitch' '#stream'
   ```
3. Make the control account a channel operator of `#stream`, so it can set the topic and `+m`. As the channel
   founder:
   ```
   /CS AMODE #stream +o twbridge
   ```

`irc.control_nick` must equal that account, and `puppets.nick_template` must end in the suffix (`{name}|twitch`).

### 3. Run

Secrets come from environment variables, never from the config file. The config itself can also come from the
environment: if `BRIDGE_CONFIG` is set, its TOML is used instead of the file (for hosts that pass environment
variables but don't mount files, such as Cloudflare Containers).

| Variable | Used for |
|---|---|
| `TWITCH_CLIENT_ID`, `TWITCH_CLIENT_SECRET` | the Twitch app |
| `TWITCH_BOT_REFRESH_TOKEN` | the bot's refresh token |
| `TWITCH_BOT_REFRESH_TOKEN_FILE` | alternative: a file holding the refresh token; rotated tokens are saved back to it |
| `CF_ACCESS_CLIENT_ID`, `CF_ACCESS_CLIENT_SECRET` | the slashcheer-irc service token (`terraform output bot_service_tokens`) |
| `BRIDGE_CONFIG` | optional: the whole config as TOML, instead of the config file |
| `TWITCH_REFRESH_TOKEN_REPORT_URL` | optional: POST rotated refresh tokens here (`{"refresh_token": ...}`, with the Access token); slashcheer-irc sets it when it hosts the bridge |

```bash
cargo run --release -- check config.toml
```

```bash
cargo run --release -- run config.toml
```

The process stays connected but relays nothing until the stream goes live. To test it (or bridge an off-stream
moment), a channel operator in the IRC channel sends `!bridge start` (stops on its own after `manual_timeout`, 2h
by default; or `!bridge start 30m`, `!bridge start forever`), `!bridge stop`, and `!bridge status`. With
`lifecycle.trigger = "manual"` the bridge only ever runs when started this way. Logging is controlled by `RUST_LOG`,
e.g. `RUST_LOG=twitch_irc_bridge=debug`.

### When something isn't relayed

`!bridge status` (channel operators) says whether it's relaying, how the Twitch side is doing, and the last thing
that went wrong, e.g.:

```
relaying (started by hand; stops in 1h 59m); puppets: 1; Twitch: connected; last problem: puppet NeuralNomad|twitch couldn't connect: IRC server refused the connection: 403 This bridged user is banned.
```

- **Twitch: not connected (…)**: the bridge couldn't look up the channel and bot accounts (often a revoked refresh
  token). It keeps retrying in the background.
- **Twitch: not receiving chat (…)**: EventSub failed, so Twitch messages aren't arriving. It reconnects on its own.
- **sending as the bot (no Chat Bot badge…)**: Twitch refused the app token, because the broadcaster hasn't granted
  `channel:bot` and the bot isn't a moderator. Messages still go out, with the bot's own token.
- An IRC user whose message didn't reach Twitch gets a notice saying why (at most once a minute).
- A Twitch viewer whose puppet can't connect because of a fault (network, server error) is relayed by the control
  connection (`<Name> text`) instead. If the IRC server *refuses* the puppet (a banned viewer: 401/403, or the
  puppet limits: 429), nothing of theirs is relayed: those lines are dropped, and so are that viewer's messages for
  15 minutes, after which the bridge tries a puppet again.

### Restarts

The bridge saves whether it's on (and until when) on every change, and the next process picks it up. So a restart
(a deploy, a container moving hosts, a crash) doesn't turn a running bridge off, and SIGTERM leaves the channel's
topic and modes alone. The state goes to `BRIDGE_STATE_URL` (slashcheer-irc sets this when it hosts the bridge) or
`BRIDGE_STATE_FILE` (the Docker image uses `/data/state.json`). With neither set, nothing is kept: SIGTERM turns the
bridge off (offline topic, `+m`) and the next process starts off.

### Docker

The image is about 50 MB (distroless, non-root). It only needs outbound HTTPS/WSS to Twitch and the IRC host;
nothing listens inbound.

```bash
docker build -t twitch-irc-bridge .
```

Put the five secrets from the table above in `bridge.env`, and your config in `config.toml`:

```bash
docker run -d --name twitch-irc-bridge --restart unless-stopped --env-file bridge.env -v "$PWD/config.toml:/config/config.toml:ro" -v bridge-data:/data twitch-irc-bridge
```

- **Refresh token.** It's kept in the `/data` volume (`TWITCH_BOT_REFRESH_TOKEN_FILE=/data/refresh-token`). The
  first run seeds it from `TWITCH_BOT_REFRESH_TOKEN`, and after that the file wins, so rotated tokens survive
  restarts. To switch to a new token, delete the volume or the file.
- **Restarts.** `docker stop` sends SIGTERM: puppets quit, but the bridge stays on. Its state is in
  `/data/state.json`, so the next start resumes it. Use `!bridge stop` to turn it off.
- **Config check:** `docker run --rm -v "$PWD/config.toml:/config/config.toml:ro" twitch-irc-bridge check /config/config.toml`.

## Develop

```bash
cargo test
```

The tests run against a mock Twitch (OAuth, Helix, EventSub) and a fake slashcheer-irc websocket server, so they
need no network or credentials.

## Layout

```
src/bridge.rs      core: events in, actions out (transport-agnostic)
src/config.rs      TOML config + validation
src/naming.rs      Twitch user → IRC puppet nick
src/relay.rs       message formatting, mentions, splitting, link/format stripping
src/lifecycle.rs   live-only state machine
src/puppets.rs     puppet registry (who has which nick, idle expiry)
src/moderation.rs  Twitch blocks, IRC mutes
src/ratelimit.rs   outbound rate limit to Twitch
src/twitch/        Twitch backend: OAuth tokens, Helix, EventSub WebSocket
src/irc/           IRC backend: WebSocket connection, control and puppet sessions
src/e2e_tests.rs   full-stack test with both backends against mocks
```
