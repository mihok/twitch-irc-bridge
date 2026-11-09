//! Where the bridge keeps its on/off state between restarts, so a restart (a deploy, a
//! container moving hosts, a crash) doesn't turn a running bridge off.
//!
//! - `BRIDGE_STATE_URL`: GET/PUT JSON to this URL, with the Cloudflare Access service token
//!   (`CF_ACCESS_CLIENT_ID` / `CF_ACCESS_CLIENT_SECRET`). slashcheer-irc serves it at
//!   `/api/bridge/state` when it hosts the bridge, since the container's disk is wiped on
//!   every restart.
//! - `BRIDGE_STATE_FILE`: a JSON file (the Docker image points it into the `/data` volume).
//! - Neither: nothing is kept, and a restart starts the bridge off.

use std::path::PathBuf;

use anyhow::{Context, Result, bail};

use crate::lifecycle::Saved;

#[derive(Debug, Clone)]
pub enum StateStore {
    None,
    File(PathBuf),
    Http {
        http: reqwest::Client,
        url: String,
        access: Option<(String, String)>,
    },
}

impl StateStore {
    pub fn from_env() -> Self {
        let var = |name: &str| std::env::var(name).ok().filter(|v| !v.trim().is_empty());
        if let Some(url) = var("BRIDGE_STATE_URL") {
            let access = var("CF_ACCESS_CLIENT_ID").zip(var("CF_ACCESS_CLIENT_SECRET"));
            let http = reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(10))
                .build()
                .unwrap_or_default();
            return Self::Http { http, url, access };
        }
        match var("BRIDGE_STATE_FILE") {
            Some(path) => Self::File(path.into()),
            None => Self::None,
        }
    }

    /// Whether state survives a restart. If it does, stopping the process is treated as a
    /// restart (the channel stays as it is); if not, as the bridge going off.
    pub fn is_persistent(&self) -> bool {
        !matches!(self, Self::None)
    }

    pub async fn load(&self) -> Result<Option<Saved>> {
        match self {
            Self::None => Ok(None),
            Self::File(path) => match std::fs::read_to_string(path) {
                Ok(raw) => Ok(Some(
                    serde_json::from_str(&raw)
                        .with_context(|| format!("parsing {}", path.display()))?,
                )),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
                Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
            },
            Self::Http { http, url, access } => {
                let resp = with_access(http.get(url), access)
                    .send()
                    .await
                    .context("loading the bridge state")?;
                match resp.status() {
                    s if s == reqwest::StatusCode::NOT_FOUND
                        || s == reqwest::StatusCode::NO_CONTENT =>
                    {
                        Ok(None)
                    }
                    s if s.is_success() => {
                        let body: serde_json::Value =
                            resp.json().await.context("loading the bridge state")?;
                        if body.is_null() || body.get("state").is_some_and(|s| s.is_null()) {
                            return Ok(None);
                        }
                        let state = body.get("state").cloned().unwrap_or(body);
                        Ok(Some(
                            serde_json::from_value(state).context("parsing the bridge state")?,
                        ))
                    }
                    s => bail!("loading the bridge state: {s}"),
                }
            }
        }
    }

    pub async fn save(&self, saved: &Saved) -> Result<()> {
        match self {
            Self::None => Ok(()),
            Self::File(path) => {
                // Write then rename, so a crash mid-write can't leave half a file.
                let tmp = path.with_extension("tmp");
                std::fs::write(&tmp, serde_json::to_vec(saved)?)
                    .with_context(|| format!("writing {}", tmp.display()))?;
                std::fs::rename(&tmp, path).with_context(|| format!("writing {}", path.display()))
            }
            Self::Http { http, url, access } => {
                let resp = with_access(http.put(url).json(saved), access)
                    .send()
                    .await
                    .context("saving the bridge state")?;
                if !resp.status().is_success() {
                    bail!("saving the bridge state: {}", resp.status());
                }
                Ok(())
            }
        }
    }
}

fn with_access(
    req: reqwest::RequestBuilder,
    access: &Option<(String, String)>,
) -> reqwest::RequestBuilder {
    match access {
        Some((id, secret)) => req
            .header("cf-access-client-id", id)
            .header("cf-access-client-secret", secret),
        None => req,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lifecycle::SavedMode;

    #[tokio::test]
    async fn file_store_round_trip() {
        let dir = std::env::temp_dir().join(format!("tib-state-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let store = StateStore::File(dir.join("state.json"));
        assert_eq!(store.load().await.unwrap(), None);
        let saved = Saved {
            mode: SavedMode::Manual,
            until_ms: Some(42),
        };
        store.save(&saved).await.unwrap();
        assert_eq!(store.load().await.unwrap(), Some(saved));
        std::fs::remove_dir_all(dir).ok();
    }

    #[tokio::test]
    async fn http_store_round_trip() {
        use axum::{Json, Router, extract::State, http::HeaderMap, routing::get};
        use std::sync::{Arc, Mutex};
        type Slot = Arc<Mutex<Option<serde_json::Value>>>;
        let slot: Slot = Arc::default();
        async fn read(State(s): State<Slot>, h: HeaderMap) -> Json<serde_json::Value> {
            assert_eq!(h["cf-access-client-id"], "cid");
            Json(serde_json::json!({ "state": s.lock().unwrap().clone() }))
        }
        async fn write(
            State(s): State<Slot>,
            Json(v): Json<serde_json::Value>,
        ) -> Json<serde_json::Value> {
            *s.lock().unwrap() = Some(v);
            Json(serde_json::json!({ "ok": true }))
        }
        let app = Router::new()
            .route("/api/bridge/state", get(read).put(write))
            .with_state(slot);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/api/bridge/state", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let store = StateStore::Http {
            http: reqwest::Client::new(),
            url,
            access: Some(("cid".into(), "secret".into())),
        };
        assert_eq!(store.load().await.unwrap(), None);
        let saved = Saved {
            mode: SavedMode::Live,
            until_ms: None,
        };
        store.save(&saved).await.unwrap();
        assert_eq!(store.load().await.unwrap(), Some(saved));
    }
}
