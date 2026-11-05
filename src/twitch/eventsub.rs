//! EventSub over WebSocket: reads chat, stream online/offline, and moderation events.
//!
//! Protocol: connect, receive `session_welcome` (session id + keepalive timeout), create
//! subscriptions for that session within 10 s, then receive `notification`s. Twitch sends
//! `session_keepalive` when idle; silence longer than the keepalive timeout means the
//! connection is dead. `session_reconnect` hands over to a new URL: open it, wait for its
//! welcome, then drop the old connection. Subscriptions carry over, so there's no
//! resubscribing.

use std::{collections::VecDeque, sync::Arc, time::Duration};

use anyhow::{Context, Result, bail};
use futures_util::StreamExt;
use serde_json::{Value, json};
use tokio::{net::TcpStream, sync::mpsc, time::timeout};
use tokio_tungstenite::{
    MaybeTlsStream, WebSocketStream, connect_async, tungstenite::Message as WsMessage,
};
use tracing::{debug, info, warn};

use super::helix::Helix;
use crate::{
    bridge::{Event, TwitchEvent},
    naming::TwitchUser,
};

type Ws = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// One parsed EventSub WebSocket message.
#[derive(Debug, PartialEq)]
pub enum Message {
    Welcome {
        session_id: String,
        keepalive: Duration,
    },
    Keepalive,
    Notification {
        message_id: String,
        event: Option<TwitchEvent>,
    },
    Reconnect {
        url: String,
    },
    Revocation {
        kind: String,
        status: String,
    },
    Other(String),
}

/// The bridge's subscriptions: (type, condition).
pub fn subscriptions(broadcaster_id: &str, bot_id: &str) -> Vec<(&'static str, Value)> {
    let chat = json!({ "broadcaster_user_id": broadcaster_id, "user_id": bot_id });
    let stream = json!({ "broadcaster_user_id": broadcaster_id });
    vec![
        ("channel.chat.message", chat.clone()),
        ("channel.chat.clear_user_messages", chat),
        ("stream.online", stream.clone()),
        ("stream.offline", stream),
    ]
}

pub fn parse(text: &str, broadcaster_id: &str) -> Result<Message> {
    let v: Value = serde_json::from_str(text).context("EventSub message isn't JSON")?;
    let meta = &v["metadata"];
    let payload = &v["payload"];
    let s = |v: &Value| v.as_str().unwrap_or_default().to_owned();
    Ok(match meta["message_type"].as_str().unwrap_or_default() {
        "session_welcome" => Message::Welcome {
            session_id: s(&payload["session"]["id"]),
            keepalive: Duration::from_secs(
                payload["session"]["keepalive_timeout_seconds"]
                    .as_u64()
                    .unwrap_or(10),
            ),
        },
        "session_keepalive" => Message::Keepalive,
        "session_reconnect" => Message::Reconnect {
            url: s(&payload["session"]["reconnect_url"]),
        },
        "revocation" => Message::Revocation {
            kind: s(&payload["subscription"]["type"]),
            status: s(&payload["subscription"]["status"]),
        },
        "notification" => Message::Notification {
            message_id: s(&meta["message_id"]),
            event: event(
                meta["subscription_type"].as_str().unwrap_or_default(),
                &payload["event"],
                broadcaster_id,
            ),
        },
        other => Message::Other(other.to_owned()),
    })
}

fn event(kind: &str, e: &Value, broadcaster_id: &str) -> Option<TwitchEvent> {
    let s = |k: &str| e[k].as_str().map(str::to_owned);
    match kind {
        "channel.chat.message" => {
            // Shared chat: messages from the other channels in the session aren't ours to bridge.
            if let Some(source) = e["source_broadcaster_user_id"].as_str()
                && source != broadcaster_id
            {
                return None;
            }
            let text = e["message"]["text"].as_str()?.trim();
            if text.is_empty() {
                return None;
            }
            Some(TwitchEvent::Chat {
                user: TwitchUser {
                    id: s("chatter_user_id")?,
                    login: s("chatter_user_login")?,
                    display_name: s("chatter_user_name")?,
                },
                text: text.to_owned(),
                action: false,
            })
        }
        "channel.chat.clear_user_messages" => Some(TwitchEvent::UserCleared {
            user_id: s("target_user_id")?,
        }),
        "stream.online" => Some(TwitchEvent::StreamOnline),
        "stream.offline" => Some(TwitchEvent::StreamOffline),
        _ => None,
    }
}

pub struct EventSub {
    pub default_url: String,
    pub helix: Arc<Helix>,
    pub broadcaster_id: String,
    pub bot_id: String,
    pub events: mpsc::Sender<Event>,
}

impl EventSub {
    /// Runs forever, reconnecting as needed.
    pub async fn run(self) {
        let mut backoff = Duration::from_secs(1);
        let mut seen: VecDeque<String> = VecDeque::new();
        loop {
            match self.session(&mut seen).await {
                Ok(()) => backoff = Duration::from_secs(1),
                Err(e) => {
                    warn!("EventSub: {e:#}; reconnecting in {backoff:?}");
                    tokio::time::sleep(backoff).await;
                    backoff = (backoff * 2).min(Duration::from_secs(60));
                }
            }
        }
    }

    async fn open(url: &str) -> Result<(Ws, String, Duration)> {
        let (mut ws, _) = connect_async(url)
            .await
            .with_context(|| format!("connecting to {url}"))?;
        let first = timeout(Duration::from_secs(10), next_text(&mut ws))
            .await
            .context("no welcome from EventSub")??
            .context("EventSub closed before welcome")?;
        match parse(&first, "")? {
            Message::Welcome {
                session_id,
                keepalive,
            } => Ok((ws, session_id, keepalive)),
            other => bail!("expected session_welcome, got {other:?}"),
        }
    }

    /// One fresh session: connect, subscribe, sync live state, then read until it breaks.
    async fn session(&self, seen: &mut VecDeque<String>) -> Result<()> {
        let (mut ws, session_id, mut keepalive) = Self::open(&self.default_url).await?;
        info!("EventSub connected");
        for (kind, condition) in subscriptions(&self.broadcaster_id, &self.bot_id) {
            self.helix.subscribe(&session_id, kind, condition).await?;
        }
        // stream.online/offline only fire on changes; catch up on the current state.
        let live = self.helix.is_live(&self.broadcaster_id).await?;
        info!(live, "subscribed to Twitch events");
        self.emit(if live {
            TwitchEvent::StreamOnline
        } else {
            TwitchEvent::StreamOffline
        })
        .await;

        loop {
            // Twitch sends a keepalive within this window when there's nothing else.
            let text = match timeout(keepalive + Duration::from_secs(5), next_text(&mut ws)).await {
                Err(_) => bail!("no message within the keepalive window"),
                Ok(r) => r?.context("EventSub closed the connection")?,
            };
            match parse(&text, &self.broadcaster_id)? {
                Message::Notification { message_id, event } => {
                    // Twitch may deliver a notification more than once.
                    if seen.contains(&message_id) {
                        continue;
                    }
                    if seen.len() >= 256 {
                        seen.pop_front();
                    }
                    seen.push_back(message_id);
                    if let Some(e) = event {
                        self.emit(e).await;
                    }
                }
                Message::Keepalive => {}
                Message::Reconnect { url } => {
                    info!("EventSub asked us to reconnect");
                    let (new_ws, _, new_keepalive) = Self::open(&url).await?;
                    let _ = ws.close(None).await;
                    ws = new_ws;
                    keepalive = new_keepalive;
                }
                Message::Revocation { kind, status } => {
                    warn!(
                        "Twitch revoked the {kind} subscription ({status}); check the bot's authorization"
                    );
                }
                Message::Welcome { .. } | Message::Other(_) => {
                    debug!("ignoring EventSub message: {text}")
                }
            }
        }
    }

    async fn emit(&self, e: TwitchEvent) {
        let _ = self.events.send(Event::Twitch(e)).await;
    }
}

async fn next_text(ws: &mut Ws) -> Result<Option<String>> {
    loop {
        match ws.next().await {
            None | Some(Ok(WsMessage::Close(_))) => return Ok(None),
            Some(Err(e)) => return Err(e.into()),
            Some(Ok(WsMessage::Text(t))) => return Ok(Some(t.to_string())),
            Some(Ok(_)) => {}
        }
    }
}
