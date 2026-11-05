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
fn puppet_session_reports_direct_messages_only() {
    let mut s = PuppetSession::new("NeuralNomad|tw", "#stream", "Twitch viewer");
    assert_eq!(
        s.register(),
        ["NICK NeuralNomad|tw", "USER twitch 0 * :Twitch viewer"]
    );
    assert_eq!(
        sends(&s.on_line(":srv 001 NeuralNomad|tw :hi")),
        ["JOIN #stream"]
    );
    s.on_line(":NeuralNomad|tw!~u@h JOIN #stream");
    assert!(s.joined());
    assert!(
        s.on_line(":alice!~u@h PRIVMSG #stream :channel talk")
            .is_empty()
    );
    assert!(
        s.on_line(":alice!~u@h PRIVMSG NeuralNomad|tw :\x01VERSION\x01")
            .is_empty()
    );
    assert_eq!(
        s.on_line(":alice!~u@h PRIVMSG neuralnomad|TW :psst"),
        vec![Action::Emit(IrcEvent::PuppetDirectMessage {
            from: "alice".into(),
            puppet: "NeuralNomad|tw".into()
        })]
    );
    assert!(matches!(
        s.on_line(":srv 433 * NeuralNomad|tw :in use").pop(),
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
            "NeuralNomad|tw",
            "412983377",
            "Twitch viewer: https://twitch.tv/neuralnomad",
        )
        .await
        .unwrap();
    client
        .puppet_say(
            "NeuralNomad|tw",
            "has anyone run MCP over websockets?",
            false,
        )
        .await
        .unwrap();

    let mut srv = conns.recv().await.unwrap();
    assert_eq!(srv.header("x-slashcheer-puppet"), Some("NeuralNomad|tw"));
    assert_eq!(srv.header("x-slashcheer-puppet-id"), Some("412983377"));
    assert_eq!(srv.header("cf-access-client-id"), Some("cid"));
    assert_eq!(srv.expect("NICK").await, "NICK NeuralNomad|tw");
    assert_eq!(
        srv.expect("USER").await,
        "USER twitch 0 * :Twitch viewer: https://twitch.tv/neuralnomad"
    );
    srv.send(":srv 001 NeuralNomad|tw :Welcome").await;
    assert_eq!(srv.expect("JOIN").await, "JOIN #stream");
    srv.send(":NeuralNomad|tw!~u@h JOIN #stream").await;
    assert_eq!(
        srv.expect("PRIVMSG").await,
        "PRIVMSG #stream :has anyone run MCP over websockets?"
    );

    client
        .puppet_say("NeuralNomad|tw", "waves", true)
        .await
        .unwrap();
    assert_eq!(
        srv.expect("PRIVMSG").await,
        "PRIVMSG #stream :\x01ACTION waves\x01"
    );

    srv.send(":alice!~u@h PRIVMSG NeuralNomad|tw :psst").await;
    assert_eq!(
        next_event(&mut events).await,
        IrcEvent::PuppetDirectMessage {
            from: "alice".into(),
            puppet: "NeuralNomad|tw".into()
        }
    );

    client
        .puppet_quit("NeuralNomad|tw", "Stream ended")
        .await
        .unwrap();
    assert_eq!(srv.expect("QUIT").await, "QUIT :Stream ended");
    assert!(
        client
            .puppet_say("NeuralNomad|tw", "gone", false)
            .await
            .is_err()
    );
}

#[tokio::test]
async fn refused_puppet_is_not_retried_immediately() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (events_tx, _events) = mpsc::channel(16);
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
    client.puppet_connect("Spammer|tw", "1", "x").await.unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;
    let before = attempts.load(std::sync::atomic::Ordering::SeqCst);
    for _ in 0..5 {
        client
            .puppet_say("Spammer|tw", "spam", false)
            .await
            .unwrap();
    }
    tokio::time::sleep(Duration::from_millis(300)).await;
    // Only the control connection keeps retrying (with backoff); the puppet isn't re-opened.
    assert!(attempts.load(std::sync::atomic::Ordering::SeqCst) <= before + 1);
}
