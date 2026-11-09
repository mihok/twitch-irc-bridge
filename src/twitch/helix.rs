//! The few Helix endpoints the bridge uses.

use std::sync::Arc;

use anyhow::{Context, Result, bail};
use reqwest::{Method, StatusCode};
use serde::{Deserialize, de::DeserializeOwned};
use serde_json::{Value, json};

use super::auth::Tokens;

#[derive(Debug, Clone, Deserialize)]
pub struct User {
    pub id: String,
    pub login: String,
    pub display_name: String,
}

#[derive(Deserialize)]
struct Data<T> {
    data: Vec<T>,
}

#[derive(Debug, Deserialize)]
struct SendResult {
    is_sent: bool,
    drop_reason: Option<DropReason>,
}

#[derive(Debug, Deserialize)]
struct DropReason {
    code: String,
    message: String,
}

/// Helix answered with an error status.
#[derive(Debug, thiserror::Error)]
#[error("{what}: {status}: {body}")]
pub struct HttpError {
    pub status: u16,
    pub what: String,
    pub body: String,
}

pub struct Helix {
    http: reqwest::Client,
    api_url: String,
    tokens: Arc<Tokens>,
}

impl Helix {
    pub fn new(http: reqwest::Client, api_url: &str, tokens: Arc<Tokens>) -> Self {
        Self {
            http,
            api_url: api_url.trim_end_matches('/').to_owned(),
            tokens,
        }
    }

    /// Sends a request with the user (or app) token, retrying once with a fresh token on 401.
    async fn call<T: DeserializeOwned>(
        &self,
        method: Method,
        path: &str,
        query: &[(&str, &str)],
        body: Option<&Value>,
        app: bool,
    ) -> Result<T> {
        for attempt in 0..2 {
            let token = if app {
                self.tokens.app_token().await?
            } else {
                self.tokens.user_token().await?
            };
            let mut req = self
                .http
                .request(method.clone(), format!("{}{path}", self.api_url))
                .query(query)
                .bearer_auth(token)
                .header("client-id", self.tokens.client_id());
            if let Some(b) = body {
                req = req.json(b);
            }
            let resp = req
                .send()
                .await
                .with_context(|| format!("{method} {path}"))?;
            let status = resp.status();
            if status == StatusCode::UNAUTHORIZED && attempt == 0 {
                self.tokens.invalidate(app).await;
                continue;
            }
            if !status.is_success() {
                return Err(HttpError {
                    status: status.as_u16(),
                    what: format!("{method} {path}"),
                    body: resp.text().await.unwrap_or_default(),
                }
                .into());
            }
            let text = resp.text().await?;
            return serde_json::from_str(if text.is_empty() { "null" } else { &text })
                .with_context(|| format!("{method} {path}: unexpected response"));
        }
        unreachable!()
    }

    pub async fn users(&self, logins: &[&str]) -> Result<Vec<User>> {
        let query: Vec<(&str, &str)> = logins.iter().map(|l| ("login", *l)).collect();
        Ok(self
            .call::<Data<User>>(Method::GET, "/users", &query, None, false)
            .await?
            .data)
    }

    pub async fn is_live(&self, broadcaster_id: &str) -> Result<bool> {
        let streams: Data<Value> = self
            .call(
                Method::GET,
                "/streams",
                &[("user_id", broadcaster_id)],
                None,
                false,
            )
            .await?;
        Ok(!streams.data.is_empty())
    }

    pub async fn subscribe(&self, session_id: &str, kind: &str, condition: Value) -> Result<()> {
        let body = json!({
            "type": kind,
            "version": "1",
            "condition": condition,
            "transport": { "method": "websocket", "session_id": session_id },
        });
        self.call::<Value>(
            Method::POST,
            "/eventsub/subscriptions",
            &[],
            Some(&body),
            false,
        )
        .await
        .with_context(|| format!("subscribing to {kind}"))?;
        Ok(())
    }

    pub async fn send_message(
        &self,
        broadcaster_id: &str,
        sender_id: &str,
        text: &str,
        as_app: bool,
    ) -> Result<()> {
        let body =
            json!({ "broadcaster_id": broadcaster_id, "sender_id": sender_id, "message": text });
        let result: Data<SendResult> = self
            .call(Method::POST, "/chat/messages", &[], Some(&body), as_app)
            .await?;
        if let Some(r) = result.data.first().filter(|r| !r.is_sent) {
            let reason = r
                .drop_reason
                .as_ref()
                .map(|d| format!("{} ({})", d.message, d.code))
                .unwrap_or_else(|| "no reason given".into());
            bail!("Twitch didn't post it: {reason}");
        }
        Ok(())
    }
}
