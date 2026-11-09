//! IRC over WebSocket (`text.ircv3.net`: one line per text frame) to slashcheer-irc,
//! authenticated at the edge with a Cloudflare Access service token.

use anyhow::{Context, Result};
use futures_util::{SinkExt, StreamExt};
use tokio::net::TcpStream;
use tokio_tungstenite::{
    MaybeTlsStream, WebSocketStream, connect_async,
    tungstenite::{Message as WsMessage, client::IntoClientRequest, http::HeaderValue},
};

/// Cloudflare Access service token (`CF_ACCESS_CLIENT_ID` / `CF_ACCESS_CLIENT_SECRET`).
#[derive(Clone)]
pub struct AccessToken {
    pub client_id: String,
    pub client_secret: String,
}

impl std::fmt::Debug for AccessToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AccessToken")
            .field("client_id", &self.client_id)
            .finish_non_exhaustive()
    }
}

impl AccessToken {
    pub fn from_env() -> Result<Self> {
        Ok(Self {
            client_id: std::env::var("CF_ACCESS_CLIENT_ID")
                .context("CF_ACCESS_CLIENT_ID is not set")?,
            client_secret: std::env::var("CF_ACCESS_CLIENT_SECRET")
                .context("CF_ACCESS_CLIENT_SECRET is not set")?,
        })
    }
}

/// The IRC server answered the websocket upgrade with an HTTP error instead.
#[derive(Debug, thiserror::Error)]
#[error("IRC server refused the connection: {status} {body}")]
pub struct Refused {
    pub status: u16,
    pub body: String,
}

impl Refused {
    /// slashcheer-irc's answer when a puppet nick already belongs to another remote user
    /// (e.g. a Twitch name reused after a rename): try another nick.
    pub fn name_in_use(&self) -> bool {
        self.status == 409
    }

    /// The server said no to this login on purpose: the remote user is banned (403), the
    /// bridge isn't allowed (401/403), or its puppet limits are reached (429). The bridge
    /// must not relay that viewer some other way.
    pub fn is_refusal(&self) -> bool {
        matches!(self.status, 401 | 403 | 429)
    }
}

/// A puppet login: slashcheer-irc logs the connection in as `nick` for remote user `remote_id`.
#[derive(Debug, Clone)]
pub struct PuppetLogin {
    pub nick: String,
    pub remote_id: String,
}

pub struct Connection {
    ws: WebSocketStream<MaybeTlsStream<TcpStream>>,
    buffered: std::collections::VecDeque<String>,
}

impl Connection {
    pub async fn open(
        url: &str,
        token: &AccessToken,
        puppet: Option<&PuppetLogin>,
    ) -> Result<Self> {
        let mut req = url.into_client_request().context("bad IRC url")?;
        let h = req.headers_mut();
        h.insert(
            "sec-websocket-protocol",
            HeaderValue::from_static("text.ircv3.net"),
        );
        h.insert(
            "cf-access-client-id",
            HeaderValue::from_str(&token.client_id)?,
        );
        h.insert(
            "cf-access-client-secret",
            HeaderValue::from_str(&token.client_secret)?,
        );
        if let Some(p) = puppet {
            h.insert("x-slashcheer-puppet", HeaderValue::from_str(&p.nick)?);
            h.insert(
                "x-slashcheer-puppet-id",
                HeaderValue::from_str(&p.remote_id)?,
            );
        }
        let (ws, resp) = match connect_async(req).await {
            Ok(ok) => ok,
            Err(tokio_tungstenite::tungstenite::Error::Http(resp)) => {
                let body = resp
                    .body()
                    .as_deref()
                    .map(String::from_utf8_lossy)
                    .unwrap_or_default();
                return Err(Refused {
                    status: resp.status().as_u16(),
                    body: body.trim().to_owned(),
                }
                .into());
            }
            Err(e) => return Err(e).context("connecting to the IRC server"),
        };
        tracing::debug!(status = %resp.status(), puppet = puppet.map(|p| p.nick.as_str()), "IRC websocket open");
        Ok(Self {
            ws,
            buffered: Default::default(),
        })
    }

    pub async fn send(&mut self, line: &str) -> Result<()> {
        tracing::trace!(">> {line}");
        self.ws
            .send(WsMessage::text(line))
            .await
            .context("IRC send")
    }

    /// Next line from the server, or `None` when the connection closed.
    pub async fn recv(&mut self) -> Result<Option<String>> {
        loop {
            if let Some(line) = self.buffered.pop_front() {
                tracing::trace!("<< {line}");
                return Ok(Some(line));
            }
            let text = match self.ws.next().await {
                None | Some(Ok(WsMessage::Close(_))) => return Ok(None),
                Some(Err(e)) => return Err(e).context("IRC receive"),
                Some(Ok(WsMessage::Text(t))) => t.to_string(),
                Some(Ok(WsMessage::Binary(b))) => String::from_utf8_lossy(&b).into_owned(),
                Some(Ok(_)) => continue, // ping/pong frames are handled by tungstenite
            };
            self.buffered.extend(
                text.split(['\r', '\n'])
                    .filter(|l| !l.is_empty())
                    .map(String::from),
            );
        }
    }

    pub async fn close(&mut self) {
        let _ = self.ws.close(None).await;
    }
}

#[cfg(test)]
mod tests {
    use super::Refused;

    #[test]
    fn refusals_are_the_server_saying_no() {
        let r = |status| Refused {
            status,
            body: String::new(),
        };
        for status in [401, 403, 429] {
            assert!(r(status).is_refusal(), "{status}");
        }
        for status in [409, 500, 502, 503, 404] {
            assert!(!r(status).is_refusal(), "{status}");
        }
    }
}
