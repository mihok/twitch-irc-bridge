# twitch-irc-bridge container image.
#
#   docker build -t twitch-irc-bridge .
#   docker run -d --name twitch-irc-bridge --restart unless-stopped --env-file bridge.env \
#     -v "$PWD/config.toml:/config/config.toml:ro" -v bridge-data:/data \
#     twitch-irc-bridge
#
# bridge.env holds TWITCH_CLIENT_ID, TWITCH_CLIENT_SECRET, TWITCH_BOT_REFRESH_TOKEN,
# CF_ACCESS_CLIENT_ID and CF_ACCESS_CLIENT_SECRET (see README.md).
#
# Needs outbound HTTPS/WSS to Twitch and to the IRC host; nothing listens inbound.

# ---- build ------------------------------------------------------------------
FROM rust:1.97-slim-bookworm AS build
WORKDIR /src

# aws-lc-rs (rustls' crypto provider) is built from C sources with the image's gcc.
# Build dependencies first so source edits don't rebuild them.
COPY Cargo.toml Cargo.lock ./
RUN mkdir src && echo 'fn main() {}' > src/main.rs && touch src/lib.rs \
 && cargo build --release --locked \
 && rm -rf src

COPY src ./src
COPY config.example.toml ./
RUN touch src/main.rs src/lib.rs \
 && cargo build --release --locked \
 && ./target/release/twitch-irc-bridge check config.example.toml \
 && mkdir -p /out/data

# ---- run --------------------------------------------------------------------
# glibc + CA certificates, no shell, runs as uid 65532.
FROM gcr.io/distroless/cc-debian12:nonroot

COPY --from=build /src/target/release/twitch-irc-bridge /usr/local/bin/twitch-irc-bridge
COPY --from=build --chown=nonroot:nonroot /src/config.example.toml /config/config.toml

# The bot's refresh token lives in /data: seeded from TWITCH_BOT_REFRESH_TOKEN on first run,
# then kept up to date when Twitch rotates it. Owned by the runtime user so a fresh volume is
# writable.
COPY --from=build --chown=nonroot:nonroot /out/data /data
VOLUME ["/data"]
ENV RUST_LOG=info \
    TWITCH_BOT_REFRESH_TOKEN_FILE=/data/refresh-token

# SIGTERM makes the bridge quit its puppets and set the offline topic before exiting.
STOPSIGNAL SIGTERM
ENTRYPOINT ["/usr/local/bin/twitch-irc-bridge"]
CMD ["run", "/config/config.toml"]
