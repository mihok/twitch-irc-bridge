use std::{
    collections::HashMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Duration,
};

use axum::{
    Form, Json, Router,
    extract::{RawQuery, State},
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
    routing::{get, post},
};
use futures_util::SinkExt;
use serde_json::{Value, json};
use tokio::{net::TcpListener, sync::mpsc, time::timeout};
use tokio_tungstenite::{WebSocketStream, tungstenite::Message as WsMessage};

use super::{eventsub, *};
use crate::{bridge::TwitchEvent, naming::TwitchUser};

// ---- parsing ----------------------------------------------------------------

pub(crate) fn notification(kind: &str, id: &str, event: Value) -> String {
    json!({
        "metadata": { "message_id": id, "message_type": "notification", "subscription_type": kind, "subscription_version": "1" },
        "payload": { "subscription": { "type": kind }, "event": event },
    })
    .to_string()
}

pub(crate) fn chat_event(text: &str, source: Option<&str>) -> Value {
    json!({
        "broadcaster_user_id": "111",
        "chatter_user_id": "412983377",
        "chatter_user_login": "neuralnomad",
        "chatter_user_name": "NeuralNomad",
        "message_id": "abc",
        "message": { "text": text, "fragments": [] },
        "message_type": "text",
        "source_broadcaster_user_id": source,
    })
}

pub(crate) fn welcome(session: &str, keepalive: u64) -> String {
    json!({
        "metadata": { "message_id": "w", "message_type": "session_welcome" },
        "payload": { "session": { "id": session, "status": "connected", "keepalive_timeout_seconds": keepalive, "reconnect_url": null } },
    })
    .to_string()
}

fn chat(text: &str) -> TwitchEvent {
    TwitchEvent::Chat {
        user: TwitchUser {
            id: "412983377".into(),
            login: "neuralnomad".into(),
            display_name: "NeuralNomad".into(),
        },
        text: text.into(),
        action: false,
        reply_to: None,
    }
}

#[test]
fn parses_twitch_replies() {
    use eventsub::{Message, parse};
    let mut e = chat_event("@ourshow_irc thanks!", None);
    e["reply"] = json!({
        "parent_message_id": "p1",
        "parent_message_body": "[irc] mihok: try the websocket transport",
        "parent_user_id": "222",
        "parent_user_login": "ourshow_irc",
        "parent_user_name": "ourshow_irc",
        "thread_message_id": "p1",
        "thread_user_id": "222",
        "thread_user_login": "ourshow_irc",
        "thread_user_name": "ourshow_irc",
    });
    let Message::Notification {
        event: Some(TwitchEvent::Chat { reply_to, .. }),
        ..
    } = parse(&notification("channel.chat.message", "n9", e), "111").unwrap()
    else {
        panic!()
    };
    assert_eq!(
        reply_to,
        Some(crate::bridge::ReplyParent {
            login: "ourshow_irc".into(),
            body: "[irc] mihok: try the websocket transport".into(),
        })
    );
    // "reply": null on ordinary messages.
    let mut e = chat_event("hi", None);
    e["reply"] = Value::Null;
    let Message::Notification {
        event: Some(TwitchEvent::Chat { reply_to, .. }),
        ..
    } = parse(&notification("channel.chat.message", "n10", e), "111").unwrap()
    else {
        panic!()
    };
    assert_eq!(reply_to, None);
}

#[test]
fn parses_eventsub_messages() {
    use eventsub::{Message, parse};
    assert_eq!(
        parse(&welcome("S1", 10), "111").unwrap(),
        Message::Welcome {
            session_id: "S1".into(),
            keepalive: Duration::from_secs(10)
        }
    );
    assert_eq!(
        parse(
            &notification("channel.chat.message", "n1", chat_event("hello", None)),
            "111"
        )
        .unwrap(),
        Message::Notification {
            message_id: "n1".into(),
            event: Some(chat("hello"))
        }
    );
    // Shared chat from another channel is ignored; from our own channel it isn't.
    let shared = parse(
        &notification("channel.chat.message", "n2", chat_event("hi", Some("999"))),
        "111",
    )
    .unwrap();
    assert_eq!(
        shared,
        Message::Notification {
            message_id: "n2".into(),
            event: None
        }
    );
    let own = parse(
        &notification("channel.chat.message", "n3", chat_event("hi", Some("111"))),
        "111",
    )
    .unwrap();
    assert_eq!(
        own,
        Message::Notification {
            message_id: "n3".into(),
            event: Some(chat("hi"))
        }
    );
    assert_eq!(
        parse(
            &notification(
                "channel.chat.clear_user_messages",
                "n4",
                json!({ "target_user_id": "412983377" })
            ),
            "111"
        )
        .unwrap(),
        Message::Notification {
            message_id: "n4".into(),
            event: Some(TwitchEvent::UserCleared {
                user_id: "412983377".into()
            })
        }
    );
    assert_eq!(
        parse(r#"{"metadata":{"message_type":"session_reconnect"},"payload":{"session":{"reconnect_url":"wss://x"}}}"#, "111").unwrap(),
        Message::Reconnect { url: "wss://x".into() }
    );
    assert_eq!(
        parse(
            r#"{"metadata":{"message_type":"session_keepalive"},"payload":{}}"#,
            "111"
        )
        .unwrap(),
        Message::Keepalive
    );
}

// ---- mock Twitch ---------------------------------------------------------------

#[derive(Default)]
pub(crate) struct Mock {
    log: Mutex<Vec<String>>,
    pub(crate) live: AtomicBool,
    users_401_once: AtomicBool,
    token_calls: AtomicUsize,
    /// Refuse app-token sends like Twitch does without channel:bot / moderator.
    pub(crate) app_send_403: AtomicBool,
    /// Answer sends with is_sent: false (AutoMod holding the message).
    pub(crate) drop_sends: AtomicBool,
}

impl Mock {
    fn log(&self, line: String) {
        self.log.lock().unwrap().push(line);
    }

    pub(crate) fn lines(&self, prefix: &str) -> Vec<String> {
        self.log
            .lock()
            .unwrap()
            .iter()
            .filter(|l| l.starts_with(prefix))
            .cloned()
            .collect()
    }
}

fn bearer(h: &HeaderMap) -> String {
    let auth = h
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();
    let cid = h
        .get("client-id")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();
    assert_eq!(cid, "client-id", "Helix calls carry the client id");
    auth.to_owned()
}

async fn token(
    State(m): State<Arc<Mock>>,
    Form(f): Form<HashMap<String, String>>,
) -> impl IntoResponse {
    m.token_calls.fetch_add(1, Ordering::SeqCst);
    assert_eq!(f["client_id"], "client-id");
    assert_eq!(f["client_secret"], "client-secret");
    match f["grant_type"].as_str() {
        "refresh_token" if f["refresh_token"] == "refresh-1" || f["refresh_token"] == "refresh-2" => {
            Json(json!({ "access_token": "user-token", "expires_in": 14400, "refresh_token": "refresh-2", "token_type": "bearer" })).into_response()
        }
        "client_credentials" => Json(json!({ "access_token": "app-token", "expires_in": 5000000, "token_type": "bearer" })).into_response(),
        _ => (StatusCode::BAD_REQUEST, "invalid refresh token").into_response(),
    }
}

async fn users(
    State(m): State<Arc<Mock>>,
    h: HeaderMap,
    RawQuery(q): RawQuery,
) -> impl IntoResponse {
    if m.users_401_once.swap(false, Ordering::SeqCst) {
        return (StatusCode::UNAUTHORIZED, "expired").into_response();
    }
    assert_eq!(bearer(&h), "Bearer user-token");
    assert_eq!(q.as_deref(), Some("login=ourshow&login=ourshow_irc"));
    Json(json!({ "data": [
        { "id": "111", "login": "ourshow", "display_name": "OurShow" },
        { "id": "222", "login": "ourshow_irc", "display_name": "ourshow_irc" },
    ]}))
    .into_response()
}

async fn streams(State(m): State<Arc<Mock>>, h: HeaderMap) -> Json<Value> {
    bearer(&h);
    let live = m.live.load(Ordering::SeqCst);
    Json(json!({ "data": if live { json!([{ "id": "1", "type": "live" }]) } else { json!([]) } }))
}

async fn subscribe(
    State(m): State<Arc<Mock>>,
    h: HeaderMap,
    Json(b): Json<Value>,
) -> impl IntoResponse {
    assert_eq!(
        bearer(&h),
        "Bearer user-token",
        "WebSocket subscriptions need a user token"
    );
    assert_eq!(b["transport"]["method"], "websocket");
    m.log(format!(
        "subscribe {} {} {}",
        b["type"].as_str().unwrap(),
        b["transport"]["session_id"].as_str().unwrap(),
        b["condition"]
    ));
    (
        StatusCode::ACCEPTED,
        Json(json!({ "data": [], "total": 1 })),
    )
}

async fn send(
    State(m): State<Arc<Mock>>,
    h: HeaderMap,
    Json(b): Json<Value>,
) -> axum::response::Response {
    let auth = bearer(&h);
    m.log(format!("send {auth} {b}"));
    if auth == "Bearer app-token" && m.app_send_403.load(Ordering::SeqCst) {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({ "error": "Forbidden", "status": 403, "message": "The sender must have authorized the app with the user:bot scope and the broadcaster must have authorized with channel:bot, or the sender must be a moderator." })),
        )
            .into_response();
    }
    if m.drop_sends.load(Ordering::SeqCst) {
        return Json(json!({ "data": [{ "message_id": "", "is_sent": false, "drop_reason": { "code": "automod_held", "message": "Your message is being checked by mods" } }] })).into_response();
    }
    Json(json!({ "data": [{ "message_id": "m1", "is_sent": true, "drop_reason": null }] }))
        .into_response()
}

pub(crate) struct Harness {
    pub(crate) mock: Arc<Mock>,
    pub(crate) cfg: TwitchConfig,
    pub(crate) ws_conns: mpsc::Receiver<WebSocketStream<tokio::net::TcpStream>>,
    pub(crate) ws_url: String,
}

pub(crate) async fn harness() -> Harness {
    let mock = Arc::new(Mock::default());
    let app = Router::new()
        .route("/oauth2/token", post(token))
        .route("/helix/users", get(users))
        .route("/helix/streams", get(streams))
        .route("/helix/eventsub/subscriptions", post(subscribe))
        .route("/helix/chat/messages", post(send))
        .with_state(mock.clone());
    let http = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let http_addr = http.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(http, app).await.unwrap() });

    let ws = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let ws_url = format!("ws://{}/ws", ws.local_addr().unwrap());
    let (tx, ws_conns) = mpsc::channel(8);
    tokio::spawn(async move {
        while let Ok((stream, _)) = ws.accept().await {
            if let Ok(conn) = tokio_tungstenite::accept_async(stream).await {
                let _ = tx.send(conn).await;
            }
        }
    });

    let cfg = TwitchConfig {
        channel: "ourshow".into(),
        bot_login: "ourshow_irc".into(),
        send_as: SendAs::App,
        api_url: format!("http://{http_addr}/helix"),
        auth_url: format!("http://{http_addr}/oauth2"),
        eventsub_url: ws_url.clone(),
    };
    Harness {
        mock,
        cfg,
        ws_conns,
        ws_url,
    }
}

pub(crate) fn creds(file: Option<std::path::PathBuf>) -> Credentials {
    Credentials {
        client_id: "client-id".into(),
        client_secret: "client-secret".into(),
        refresh_token: "refresh-1".into(),
        refresh_token_file: file,
        report_url: None,
        report_access: None,
    }
}

#[tokio::test]
async fn rotated_refresh_token_is_reported() {
    let h = harness().await;
    let got: Arc<Mutex<Vec<(String, Value)>>> = Default::default();
    let seen = got.clone();
    let app = Router::new().route(
        "/api/bridge/twitch-token",
        post(move |headers: HeaderMap, Json(body): Json<Value>| {
            let seen = seen.clone();
            async move {
                let id = headers["cf-access-client-id"].to_str().unwrap().to_owned();
                seen.lock().unwrap().push((id, body));
                "ok"
            }
        }),
    );
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    let mut c = creds(None);
    c.report_url = Some(format!("http://{addr}/api/bridge/twitch-token"));
    c.report_access = Some(("bridge.access".into(), "s3cret".into()));
    let tokens = auth::Tokens::new(reqwest::Client::new(), &h.cfg.auth_url, c);
    // The mock answers refresh-1 with refresh-2: a rotation.
    tokens.user_token().await.unwrap();
    assert_eq!(
        *got.lock().unwrap(),
        [(
            "bridge.access".to_string(),
            json!({ "refresh_token": "refresh-2" })
        )]
    );
}

async fn next_event(rx: &mut mpsc::Receiver<Event>) -> TwitchEvent {
    match timeout(Duration::from_secs(5), rx.recv())
        .await
        .expect("timed out")
        .expect("closed")
    {
        Event::Twitch(e) => e,
        other => panic!("{other:?}"),
    }
}

pub(crate) async fn send_ws(ws: &mut WebSocketStream<tokio::net::TcpStream>, text: String) {
    ws.send(WsMessage::text(text)).await.unwrap();
}

#[tokio::test]
async fn end_to_end_with_mock_twitch() {
    let mut h = harness().await;
    h.mock.live.store(true, Ordering::SeqCst);
    h.mock.users_401_once.store(true, Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!("tib-test-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let token_file = dir.join("refresh-token");
    std::fs::write(&token_file, "refresh-1").unwrap();

    let (events_tx, mut events) = mpsc::channel(16);
    // The first users call gets a 401; the client refreshes the token and retries.
    let mut client = TwitchClient::start(h.cfg.clone(), creds(Some(token_file.clone())), events_tx)
        .await
        .unwrap();

    let mut ws = h.ws_conns.recv().await.unwrap();
    assert_eq!(
        std::fs::read_to_string(&token_file).unwrap(),
        "refresh-2",
        "rotated refresh token is saved"
    );
    send_ws(&mut ws, welcome("S1", 10)).await;
    // Already live when the bridge starts: reported from Helix after subscribing.
    assert_eq!(next_event(&mut events).await, TwitchEvent::StreamOnline);
    let subs = h.mock.lines("subscribe");
    assert_eq!(
        subs,
        [
            r#"subscribe channel.chat.message S1 {"broadcaster_user_id":"111","user_id":"222"}"#,
            r#"subscribe channel.chat.clear_user_messages S1 {"broadcaster_user_id":"111","user_id":"222"}"#,
            r#"subscribe stream.online S1 {"broadcaster_user_id":"111"}"#,
            r#"subscribe stream.offline S1 {"broadcaster_user_id":"111"}"#,
        ]
    );

    send_ws(
        &mut ws,
        notification("channel.chat.message", "n1", chat_event("hello", None)),
    )
    .await;
    assert_eq!(next_event(&mut events).await, chat("hello"));
    // Duplicate delivery and shared chat from another channel are dropped.
    send_ws(
        &mut ws,
        notification("channel.chat.message", "n1", chat_event("hello", None)),
    )
    .await;
    send_ws(
        &mut ws,
        notification(
            "channel.chat.message",
            "n2",
            chat_event("elsewhere", Some("999")),
        ),
    )
    .await;
    send_ws(
        &mut ws,
        notification(
            "channel.chat.clear_user_messages",
            "n3",
            json!({ "target_user_id": "412983377" }),
        ),
    )
    .await;
    assert_eq!(
        next_event(&mut events).await,
        TwitchEvent::UserCleared {
            user_id: "412983377".into()
        }
    );

    // Reconnect handover: new connection, no resubscribing, events keep flowing.
    let reconnect = json!({
        "metadata": { "message_id": "r", "message_type": "session_reconnect" },
        "payload": { "session": { "id": "S1", "status": "reconnecting", "reconnect_url": format!("{}?reconnect", h.ws_url) } },
    });
    send_ws(&mut ws, reconnect.to_string()).await;
    let mut ws2 = h.ws_conns.recv().await.unwrap();
    send_ws(&mut ws2, welcome("S1", 10)).await;
    send_ws(
        &mut ws2,
        notification(
            "stream.offline",
            "n4",
            json!({ "broadcaster_user_id": "111" }),
        ),
    )
    .await;
    assert_eq!(next_event(&mut events).await, TwitchEvent::StreamOffline);
    assert_eq!(h.mock.lines("subscribe").len(), 4);

    client.send_chat("[irc] mihok: hello").await.unwrap();
    assert_eq!(
        h.mock.lines("send"),
        [
            r#"send Bearer app-token {"broadcaster_id":"111","message":"[irc] mihok: hello","sender_id":"222"}"#
        ]
    );
    std::fs::remove_dir_all(dir).ok();
}

#[tokio::test]
async fn resubscribes_after_connection_loss() {
    let mut h = harness().await;
    let (events_tx, mut events) = mpsc::channel(16);
    let _client = TwitchClient::start(h.cfg.clone(), creds(None), events_tx)
        .await
        .unwrap();
    let mut ws = h.ws_conns.recv().await.unwrap();
    send_ws(&mut ws, welcome("S1", 10)).await;
    assert_eq!(next_event(&mut events).await, TwitchEvent::StreamOffline);
    ws.close(None).await.unwrap();
    drop(ws);

    let mut ws = timeout(Duration::from_secs(5), h.ws_conns.recv())
        .await
        .unwrap()
        .unwrap();
    h.mock.live.store(true, Ordering::SeqCst);
    send_ws(&mut ws, welcome("S2", 10)).await;
    // A fresh session resubscribes and re-syncs live state, so a missed stream.online isn't lost.
    assert_eq!(next_event(&mut events).await, TwitchEvent::StreamOnline);
    assert_eq!(
        h.mock
            .lines("subscribe")
            .iter()
            .filter(|l| l.contains(" S2 "))
            .count(),
        4
    );
}

#[tokio::test]
async fn user_send_mode_uses_the_bot_token() {
    let mut h = harness().await;
    h.cfg.send_as = SendAs::User;
    let (events_tx, _events) = mpsc::channel(16);
    let mut client = TwitchClient::start(h.cfg.clone(), creds(None), events_tx)
        .await
        .unwrap();
    client.send_chat("hi").await.unwrap();
    assert!(h.mock.lines("send")[0].starts_with("send Bearer user-token "));
    drop(h.ws_conns);
}

#[tokio::test]
async fn bad_refresh_token_is_reported_without_crashing() {
    let h = harness().await;
    let mut bad = creds(None);
    bad.refresh_token = "revoked".into();
    let (events_tx, _events) = mpsc::channel(16);
    // Startup doesn't fail (the process would crash-loop); the problem shows in the status,
    // and sending says why it can't.
    let mut client = TwitchClient::start(h.cfg.clone(), bad, events_tx)
        .await
        .unwrap();
    let mut status = String::new();
    for _ in 0..50 {
        status = client.status();
        if status.contains("TWITCH_BOT_REFRESH_TOKEN") {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(status.starts_with("not connected ("), "{status}");
    assert!(status.contains("TWITCH_BOT_REFRESH_TOKEN"), "{status}");
    let err = client.send_chat("hi").await.unwrap_err();
    assert!(
        format!("{err:#}").contains("not connected to Twitch"),
        "{err:#}"
    );
}

#[tokio::test]
async fn refused_app_token_falls_back_to_the_bot_token() {
    let h = harness().await;
    h.mock.app_send_403.store(true, Ordering::SeqCst);
    let (events_tx, _events) = mpsc::channel(16);
    let mut client = TwitchClient::start(h.cfg.clone(), creds(None), events_tx)
        .await
        .unwrap();
    client.send_chat("first").await.unwrap();
    client.send_chat("second").await.unwrap();
    let sent = h.mock.lines("send");
    // The app token is tried once; after the 403 everything goes with the bot's own token.
    assert_eq!(sent.len(), 3, "{sent:?}");
    assert!(sent[0].starts_with("send Bearer app-token "));
    assert!(sent[1].starts_with("send Bearer user-token ") && sent[1].contains("first"));
    assert!(sent[2].starts_with("send Bearer user-token ") && sent[2].contains("second"));
    assert!(
        client.status().contains("sending as the bot"),
        "{}",
        client.status()
    );
    drop(h.ws_conns);
}

#[tokio::test]
async fn message_twitch_drops_is_an_error() {
    let h = harness().await;
    h.mock.drop_sends.store(true, Ordering::SeqCst);
    let (events_tx, _events) = mpsc::channel(16);
    let mut client = TwitchClient::start(h.cfg.clone(), creds(None), events_tx)
        .await
        .unwrap();
    let err = client.send_chat("hi").await.unwrap_err();
    assert!(
        format!("{err:#}").contains("Twitch didn't post it: Your message is being checked by mods"),
        "{err:#}"
    );
}

#[tokio::test]
async fn first_run_seeds_the_refresh_token_file() {
    let h = harness().await;
    let dir = std::env::temp_dir().join(format!("tib-seed-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("refresh-token");
    let _ = std::fs::remove_file(&file);
    // No rotation (the mock returns the same token), but the missing file is still written.
    let mut c = creds(Some(file.clone()));
    c.refresh_token = "refresh-2".into();
    let (events_tx, _events) = mpsc::channel(16);
    let _client = TwitchClient::start(h.cfg.clone(), c, events_tx)
        .await
        .unwrap();
    // The account lookup (and so the first token refresh) runs in the background.
    for _ in 0..50 {
        if std::fs::read_to_string(&file).is_ok() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert_eq!(std::fs::read_to_string(&file).unwrap(), "refresh-2");
    std::fs::remove_dir_all(dir).ok();
}
