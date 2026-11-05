# twitch-irc-bridge

Bridges our Twitch stream chat with a dedicated IRC channel on
[slashcheer-irc](https://github.com/mihok/slashcheer-irc), only while we're live. Twitch viewers show up on IRC as
`name|tw` puppet users; IRC users show up on Twitch through one bot account.

See [DESIGN.md](DESIGN.md) for the design and current status.

## Setup

### 1. Twitch

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

This needs puppet mode from [mihok/slashcheer-irc#1](https://github.com/mihok/slashcheer-irc/pull/1).

1. Add a bot token (e.g. `twitch-bridge`) to `bot_service_tokens` and `bridge_bots`, and apply.
2. Bind it to the control account and mark it as a bridge:
   ```bash
   scripts/irc-admin.sh bind-bot twitch-bridge twbridge
   ```
   ```bash
   scripts/irc-admin.sh bind-bridge twitch-bridge '|tw' '#stream'
   ```
3. Make the control account a channel operator of `#stream`, so it can set the topic and `+m`. As the channel
   founder:
   ```
   /CS AMODE #stream +o twbridge
   ```

`irc.control_nick` must equal that account, and `puppets.nick_template` must end in the suffix (`{name}|tw`).

### 3. Run

Secrets come from environment variables, never from the config file:

| Variable | Used for |
|---|---|
| `TWITCH_CLIENT_ID`, `TWITCH_CLIENT_SECRET` | the Twitch app |
| `TWITCH_BOT_REFRESH_TOKEN` | the bot's refresh token |
| `TWITCH_BOT_REFRESH_TOKEN_FILE` | alternative: a file holding the refresh token; rotated tokens are saved back to it |
| `CF_ACCESS_CLIENT_ID`, `CF_ACCESS_CLIENT_SECRET` | the slashcheer-irc service token (`terraform output bot_service_tokens`) |

```bash
cargo run --release -- check config.toml
```

```bash
cargo run --release -- run config.toml
```

The process stays connected but relays nothing until the stream goes live. Logging is controlled by `RUST_LOG`,
e.g. `RUST_LOG=twitch_irc_bridge=debug`.

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
