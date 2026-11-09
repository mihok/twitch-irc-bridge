// The handshake callback signature is fixed by tungstenite.
#![allow(clippy::result_large_err)]

use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use tokio::{net::TcpListener, sync::mpsc, time::timeout};
use tokio_tungstenite::{
    WebSocketStream,
    tungstenite::{
        Message as WsMessage,
        handshake::server::{Request, Response},
        http::{HeaderMap, HeaderValue},
    },
};

use super::*;

// ---- session state machines ------------------------------------------------

fn sends(actions: &[Action]) -> Vec<&str> {
    actions
        .iter()
        .filter_map(|a| match a {
            Action::Send(l) => Some(l.as_str()),
            _ => None,
        })
        .collect()
}

#[test]
fn control_session_joins_and_tracks_ops() {
    let mut s = ControlSession::new("twbridge", "#stream");
    assert_eq!(s.register()[0], "NICK twbridge");
    assert_eq!(
        sends(&s.on_line(":srv 001 twbridge :Welcome")),
        ["JOIN #stream"]
    );
    assert_eq!(
        s.on_line(":srv 005 twbridge NICKLEN=20 PREFIX=(qaohv)~&@%+ :are supported"),
        vec![Action::Emit(IrcEvent::NickLen(20))]
    );
    s.on_line(":twbridge!~u@h JOIN #stream");
    assert!(s.joined());
    s.on_line(":srv 353 twbridge = #stream :~mihok sarahk +carol @twbridge");

    let msg = |s: &mut ControlSession, nick: &str| match s
        .on_line(&format!(":{nick}!~u@h PRIVMSG #stream :hi"))
        .pop()
    {
        Some(Action::Emit(IrcEvent::Message { is_op, .. })) => is_op,
        other => panic!("{other:?}"),
    };
    assert!(msg(&mut s, "mihok"));
    assert!(!msg(&mut s, "sarahk"));
    s.on_line(":mihok!~u@h MODE #stream +o-o sarahk mihok");
    assert!(msg(&mut s, "sarahk"));
    assert!(!msg(&mut s, "mihok"));
    s.on_line(":sarahk!~u@h NICK sarah");
    assert!(msg(&mut s, "sarah"));
    s.on_line(":sarah!~u@h QUIT :bye");
    assert!(!msg(&mut s, "sarah"));
}

#[test]
fn control_session_reports_joins_ignores_itself_and_rejoins_after_kick() {
    let mut s = ControlSession::new("twbridge", "#stream");
    s.on_line(":srv 001 twbridge :Welcome");
    s.on_line(":twbridge!~u@h JOIN #stream");
    assert_eq!(
        s.on_line(":alice!~u@h JOIN #stream"),
        vec![Action::Emit(IrcEvent::Join {
            nick: "alice".into()
        })]
    );
    assert!(s.on_line(":twbridge!~u@h PRIVMSG #stream :echo").is_empty());
    assert!(
        s.on_line(":alice!~u@h PRIVMSG #other :elsewhere")
            .is_empty()
    );
    assert_eq!(
        sends(&s.on_line(":op!~u@h KICK #stream twbridge :out")),
        ["JOIN #stream"]
    );
    assert!(!s.joined());
    assert_eq!(sends(&s.on_line("PING :123")), ["PONG :123"]);
}

#[test]
fn control_session_tracks_channel_members() {
    let members = Members::default();
    let mut s = ControlSession::with_members("twbridge", "#stream", members.clone());
    let nick = |n: &str| members.lock().unwrap().get(&casefold(n)).cloned();
    s.on_line(":twbridge!~u@h JOIN #stream");
    s.on_line(":srv 353 twbridge = #stream :~mihok SarahK +carol @twbridge");
    assert_eq!(nick("MIHOK").as_deref(), Some("mihok"));
    assert_eq!(nick("sarahk").as_deref(), Some("SarahK"));
    assert_eq!(
        nick("twbridge"),
        None,
        "the bridge itself isn't a mention target"
    );
    s.on_line(":dave!~u@h JOIN #stream");
    s.on_line(":SarahK!~u@h NICK sarah");
    s.on_line(":carol!~u@h PART #stream");
    s.on_line(":mihok!~u@h QUIT :bye");
    s.on_line(":srv KICK #stream dave :out");
    let mut left: Vec<String> = members.lock().unwrap().values().cloned().collect();
    left.sort();
    assert_eq!(left, ["sarah"]);
    // A new connection starts from scratch.
    let _ = ControlSession::with_members("twbridge", "#stream", members.clone());
    assert!(members.lock().unwrap().is_empty());
}

#[test]
fn puppet_session_reports_direct_messages_only() {
    let mut s = PuppetSession::new("NeuralNomad|twitch", "#stream", "Twitch viewer");
    assert_eq!(
        s.register(),
        ["NICK NeuralNomad|twitch", "USER twitch 0 * :Twitch viewer"]
    );
    assert_eq!(
        sends(&s.on_line(":srv 001 NeuralNomad|twitch :hi")),
        ["JOIN #stream"]
    );
    s.on_line(":NeuralNomad|twitch!~u@h JOIN #stream");
    assert!(s.joined());
    assert!(
        s.on_line(":alice!~u@h PRIVMSG #stream :channel talk")
            .is_empty()
    );
    assert!(
        s.on_line(":alice!~u@h PRIVMSG NeuralNomad|twitch :\x01VERSION\x01")
            .is_empty()
    );
    assert_eq!(
        s.on_line(":alice!~u@h PRIVMSG neuralnomad|TWITCH :psst"),
        vec![Action::Emit(IrcEvent::PuppetDirectMessage {
            from: "alice".into(),
            puppet: "NeuralNomad|twitch".into()
        })]
    );
    assert!(matches!(
        s.on_line(":srv 433 * NeuralNomad|twitch :in use").pop(),
        Some(Action::Close(_))
    ));
}

// ---- end to end against a fake websocket IRC server ---------------------

pub(crate) struct FakeConn {
    headers: HeaderMap,
    ws: WebSocketStream<tokio::net::TcpStream>,
}

impl FakeConn {
    pub(crate) async fn send(&mut self, line: &str) {
        self.ws.send(WsMessage::text(line)).await.unwrap();
    }

    /// Next line from the client, skipping lines that don't start with `command`.
    pub(crate) async fn expect(&mut self, command: &str) -> String {
        loop {
            let msg = timeout(Duration::from_secs(5), self.ws.next())
                .await
                .expect("timed out")
                .expect("closed")
                .unwrap();
            if let WsMessage::Text(t) = msg
                && t.starts_with(command)
            {
                return t.to_string();
            }
        }
    }

    pub(crate) fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).and_then(|v| v.to_str().ok())
    }
}

pub(crate) async fn fake_server() -> (String, mpsc::Receiver<FakeConn>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("ws://{}/web/irc", listener.local_addr().unwrap());
    let (tx, rx) = mpsc::channel(8);
    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            let tx = tx.clone();
            tokio::spawn(async move {
                let mut headers = HeaderMap::new();
                let ws = tokio_tungstenite::accept_hdr_async(
                    stream,
                    |req: &Request, mut resp: Response| {
                        headers = req.headers().clone();
                        resp.headers_mut().insert(
                            "sec-websocket-protocol",
                            HeaderValue::from_static("text.ircv3.net"),
                        );
                        Ok(resp)
                    },
                )
                .await
                .unwrap();
                let _ = tx.send(FakeConn { headers, ws }).await;
            });
        }
    });
    (url, rx)
}

pub(crate) fn irc_config(url: &str) -> IrcConfig {
    IrcConfig {
        url: url.into(),
        channel: "#stream".into(),
        control_nick: "twbridge".into(),
        nicklen: 32,
    }
}

pub(crate) fn token() -> AccessToken {
    AccessToken {
        client_id: "cid".into(),
        client_secret: "secret".into(),
    }
}

async fn next_event(rx: &mut mpsc::Receiver<Event>) -> IrcEvent {
    match timeout(Duration::from_secs(5), rx.recv())
        .await
        .expect("timed out")
        .expect("closed")
    {
        Event::Irc(e) => e,
        other => panic!("{other:?}"),
    }
}

#[tokio::test]
async fn control_connection_end_to_end() {
    let (url, mut conns) = fake_server().await;
    let (events_tx, mut events) = mpsc::channel(16);
    let mut client = IrcClient::start(irc_config(&url), token(), events_tx);
    // Queued until the control connection has joined.
    client.set_topic("LIVE").await.unwrap();

    let mut srv = conns.recv().await.unwrap();
    assert_eq!(srv.header("cf-access-client-id"), Some("cid"));
    assert_eq!(srv.header("cf-access-client-secret"), Some("secret"));
    assert_eq!(srv.header("x-slashcheer-puppet"), None);
    assert_eq!(srv.expect("NICK").await, "NICK twbridge");
    srv.send(":srv 001 twbridge :Welcome").await;
    srv.send(":srv 005 twbridge NICKLEN=30 :are supported")
        .await;
    assert_eq!(next_event(&mut events).await, IrcEvent::NickLen(30));
    assert_eq!(srv.expect("JOIN").await, "JOIN #stream");
    srv.send(":twbridge!~u@h JOIN #stream").await;
    assert_eq!(srv.expect("TOPIC").await, "TOPIC #stream :LIVE");
    srv.send(":srv 353 twbridge = #stream :@mihok twbridge")
        .await;
    srv.send(":mihok!~u@h PRIVMSG #stream :!bridge status")
        .await;
    assert_eq!(
        next_event(&mut events).await,
        IrcEvent::Message {
            nick: "mihok".into(),
            text: "!bridge status".into(),
            is_op: true
        }
    );

    client.set_moderated(true).await.unwrap();
    assert_eq!(srv.expect("MODE").await, "MODE #stream +m");
    client.notice("mihok", "State: Live").await.unwrap();
    assert_eq!(srv.expect("NOTICE").await, "NOTICE mihok :State: Live");
    srv.send("PING :abc").await;
    assert_eq!(srv.expect("PONG").await, "PONG :abc");
}

#[tokio::test]
async fn control_connection_reconnects() {
    let (url, mut conns) = fake_server().await;
    let (events_tx, _events) = mpsc::channel(16);
    let _client = IrcClient::start(irc_config(&url), token(), events_tx);
    let mut first = conns.recv().await.unwrap();
    first.expect("NICK").await;
    first.ws.close(None).await.unwrap();
    drop(first);
    let mut second = timeout(Duration::from_secs(5), conns.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(second.expect("NICK").await, "NICK twbridge");
}

#[tokio::test]
async fn puppet_connection_end_to_end() {
    let (url, mut conns) = fake_server().await;
    let (events_tx, mut events) = mpsc::channel(16);
    let mut client = IrcClient::start(irc_config(&url), token(), events_tx);
    let _control = conns.recv().await.unwrap();

    client
        .puppet_connect(
            "NeuralNomad|twitch",
            "412983377",
            "Twitch viewer: https://twitch.tv/neuralnomad",
        )
        .await
        .unwrap();
    client
        .puppet_say(
            "NeuralNomad|twitch",
            "has anyone run MCP over websockets?",
            false,
        )
        .await
        .unwrap();

    let mut srv = conns.recv().await.unwrap();
    assert_eq!(
        srv.header("x-slashcheer-puppet"),
        Some("NeuralNomad|twitch")
    );
    assert_eq!(srv.header("x-slashcheer-puppet-id"), Some("412983377"));
    assert_eq!(srv.header("cf-access-client-id"), Some("cid"));
    assert_eq!(srv.expect("NICK").await, "NICK NeuralNomad|twitch");
    assert_eq!(
        srv.expect("USER").await,
        "USER twitch 0 * :Twitch viewer: https://twitch.tv/neuralnomad"
    );
    srv.send(":srv 001 NeuralNomad|twitch :Welcome").await;
    assert_eq!(srv.expect("JOIN").await, "JOIN #stream");
    srv.send(":NeuralNomad|twitch!~u@h JOIN #stream").await;
    assert_eq!(
        srv.expect("PRIVMSG").await,
        "PRIVMSG #stream :has anyone run MCP over websockets?"
    );

    client
        .puppet_say("NeuralNomad|twitch", "waves", true)
        .await
        .unwrap();
    assert_eq!(
        srv.expect("PRIVMSG").await,
        "PRIVMSG #stream :\x01ACTION waves\x01"
    );

    srv.send(":alice!~u@h PRIVMSG NeuralNomad|twitch :psst")
        .await;
    assert_eq!(
        next_event(&mut events).await,
        IrcEvent::PuppetDirectMessage {
            from: "alice".into(),
            puppet: "NeuralNomad|twitch".into()
        }
    );

    client
        .puppet_quit("NeuralNomad|twitch", "Stream ended")
        .await
        .unwrap();
    assert_eq!(srv.expect("QUIT").await, "QUIT :Stream ended");
    assert!(
        client
            .puppet_say("NeuralNomad|twitch", "gone", false)
            .await
            .is_err()
    );
}

#[tokio::test]
async fn refused_puppet_is_reported_and_not_retried_immediately() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (events_tx, mut events) = mpsc::channel(16);
    let mut client = IrcClient::start(
        irc_config(&format!("ws://{addr}/web/irc")),
        token(),
        events_tx,
    );
    // Answer every websocket attempt with a 403, like slashcheer-irc does for a banned puppet.
    let attempts = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let counter = attempts.clone();
    tokio::spawn(async move {
        while let Ok((mut stream, _)) = listener.accept().await {
            counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            use tokio::io::AsyncWriteExt;
            let _ = stream
                .write_all(b"HTTP/1.1 403 Forbidden\r\ncontent-length: 29\r\nconnection: close\r\n\r\nThis bridged user is banned.\n")
                .await;
        }
    });
    client
        .puppet_connect("Spammer|twitch", "1", "x")
        .await
        .unwrap();
    client
        .puppet_say("Spammer|twitch", "first", false)
        .await
        .unwrap();
    // The failure comes back with what the puppet didn't get to say, so the bridge can
    // relay it another way.
    match next_event(&mut events).await {
        IrcEvent::PuppetFailed {
            nick,
            remote_id,
            reason,
            unsent,
        } => {
            assert_eq!((nick.as_str(), remote_id.as_str()), ("Spammer|twitch", "1"));
            assert!(reason.contains("403"), "{reason}");
            assert_eq!(unsent, vec![("first".to_string(), false)]);
        }
        other => panic!("{other:?}"),
    }
    let before = attempts.load(std::sync::atomic::Ordering::SeqCst);
    for _ in 0..5 {
        // Down, and not retried yet: the caller is told, rather than the line vanishing.
        assert!(
            client
                .puppet_say("Spammer|twitch", "spam", false)
                .await
                .is_err()
        );
    }
    tokio::time::sleep(Duration::from_millis(300)).await;
    // Only the control connection keeps retrying (with backoff); the puppet isn't re-opened.
    assert!(attempts.load(std::sync::atomic::Ordering::SeqCst) <= before + 1);
}

#[tokio::test]
async fn puppet_nick_owned_by_someone_else_is_reported_with_unsent_messages() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (events_tx, mut events) = mpsc::channel(16);
    let mut client = IrcClient::start(
        irc_config(&format!("ws://{addr}/web/irc")),
        token(),
        events_tx,
    );
    // Answer puppet attempts with a 409, like slashcheer-irc for a name owned by another
    // remote user. Hold the answer briefly so the messages are queued before it fails.
    tokio::spawn(async move {
        while let Ok((mut stream, _)) = listener.accept().await {
            tokio::spawn(async move {
                use tokio::io::{AsyncReadExt, AsyncWriteExt};
                let mut buf = [0u8; 4096];
                let n = stream.read(&mut buf).await.unwrap_or(0);
                if !String::from_utf8_lossy(&buf[..n])
                    .to_lowercase()
                    .contains("x-slashcheer-puppet:")
                {
                    return; // the control connection: just hang
                }
                tokio::time::sleep(Duration::from_millis(200)).await;
                let _ = stream
                    .write_all(b"HTTP/1.1 409 Conflict\r\ncontent-length: 48\r\nconnection: close\r\n\r\nThat puppet name belongs to another remote user.")
                    .await;
            });
        }
    });
    client
        .puppet_connect("NeuralNomad|twitch", "98110234", "x")
        .await
        .unwrap();
    client
        .puppet_say("NeuralNomad|twitch", "hello", false)
        .await
        .unwrap();
    client
        .puppet_say("NeuralNomad|twitch", "waves", true)
        .await
        .unwrap();
    assert_eq!(
        next_event(&mut events).await,
        IrcEvent::PuppetNickRefused {
            nick: "NeuralNomad|twitch".into(),
            remote_id: "98110234".into(),
            unsent: vec![("hello".into(), false), ("waves".into(), true)],
        }
    );
}
