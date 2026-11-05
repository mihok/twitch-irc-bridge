# twitch-irc-bridge

Bridges our Twitch stream chat with a dedicated IRC channel on
[slashcheer-irc](https://github.com/mihok/slashcheer-irc), only while we're live. Twitch viewers show up on IRC as
`name|tw` puppet users; IRC users show up on Twitch through one bot account.

See [DESIGN.md](DESIGN.md) for the design and current status. The core logic is implemented and tested; the Twitch
and IRC network backends are still stubs.

## Develop

```bash
cargo test
```

```bash
cargo run -- check config.example.toml
```

`run` starts the bridge once the backends exist:

```bash
cargo run -- run config.toml
```

Secrets come from environment variables, never from the config file:

| Variable | Used for |
|---|---|
| `TWITCH_CLIENT_ID`, `TWITCH_CLIENT_SECRET`, `TWITCH_BOT_REFRESH_TOKEN` | Twitch app and bot account |
| `CF_ACCESS_CLIENT_ID`, `CF_ACCESS_CLIENT_SECRET` | Cloudflare Access service token for slashcheer-irc |

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
src/twitch.rs      Twitch backend (stub)
src/irc.rs         IRC backend (stub)
```
