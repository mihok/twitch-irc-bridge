//! Full stack: the bridge core with the real Twitch and IRC backends, against a mock Twitch
//! (OAuth, Helix, EventSub) and a fake slashcheer-irc websocket server.

use std::{sync::atomic::Ordering, time::Duration};

use serde_json::json;
use tokio::sync::mpsc;

use crate::{
    bridge::{self, Bridge},
    config::Config,
    irc::{
        IrcClient,
        tests::{fake_server, irc_config, token},
    },
    twitch::{
        TwitchClient,
        tests::{chat_event, creds, harness, notification, send_ws, welcome},
    },
};

#[tokio::test]
async fn twitch_and_irc_bridged_through_a_live_stream() {
    let mut twitch = harness().await;
    let (irc_url, mut irc_conns) = fake_server().await;

    let mut cfg = Config::parse(include_str!("../config.example.toml")).unwrap();
    cfg.twitch = twitch.cfg.clone();
    cfg.irc = irc_config(&irc_url);
    cfg.lifecycle.grace_period = Duration::ZERO;

    let (tx, rx) = mpsc::channel(64);
    let irc = IrcClient::start(cfg.irc.clone(), token(), tx.clone());
    let tw = TwitchClient::start(cfg.twitch.clone(), creds(None), tx)
        .await
        .unwrap();
    tokio::spawn(bridge::run(
        Bridge::new(cfg, tw, irc),
        rx,
        Duration::from_millis(100),
    ));

    // Control connection joins #stream; mihok is an op.
    let mut control = irc_conns.recv().await.unwrap();
    control.expect("NICK").await;
    control.send(":srv 001 twbridge :Welcome").await;
    control.expect("JOIN").await;
    control.send(":twbridge!~u@h JOIN #stream").await;
    control
        .send(":srv 353 twbridge = #stream :@mihok sarahk twbridge")
        .await;

    // EventSub session; not live yet.
    let mut es = twitch.ws_conns.recv().await.unwrap();
    send_ws(&mut es, welcome("S1", 10)).await;

    // Stream goes live: topic and -m.
    send_ws(
        &mut es,
        notification(
            "stream.online",
            "n1",
            json!({ "broadcaster_user_id": "111" }),
        ),
    )
    .await;
    assert!(
        control
            .expect("TOPIC")
            .await
            .starts_with("TOPIC #stream :🔴 LIVE")
    );
    assert_eq!(control.expect("MODE").await, "MODE #stream -m");

    // A Twitch viewer speaks: a puppet connection appears and says it.
    send_ws(
        &mut es,
        notification(
            "channel.chat.message",
            "n2",
            chat_event("has anyone run MCP over websockets?", None),
        ),
    )
    .await;
    let mut puppet = irc_conns.recv().await.unwrap();
    assert_eq!(
        puppet.header("x-slashcheer-puppet"),
        Some("NeuralNomad|twitch")
    );
    assert_eq!(puppet.header("x-slashcheer-puppet-id"), Some("412983377"));
    puppet.expect("NICK").await;
    puppet.send(":srv 001 NeuralNomad|twitch :Welcome").await;
    puppet.expect("JOIN").await;
    puppet.send(":NeuralNomad|twitch!~u@h JOIN #stream").await;
    assert_eq!(
        puppet.expect("PRIVMSG").await,
        "PRIVMSG #stream :has anyone run MCP over websockets?"
    );

    // IRC replies, mentioning the puppet: relayed to Twitch as an @mention by the bot.
    control
        .send(":sarahk!~u@h PRIVMSG #stream :NeuralNomad|twitch: yes, in prod")
        .await;
    let sent = wait_for(|| twitch.mock.lines("send")).await;
    assert_eq!(
        sent,
        [
            r#"send Bearer app-token {"broadcaster_id":"111","message":"[irc] sarahk: @NeuralNomad yes, in prod","sender_id":"222"}"#
        ]
    );

    // An op checks status.
    control
        .send(":mihok!~u@h PRIVMSG #stream :!bridge status")
        .await;
    assert!(control.expect("NOTICE mihok").await.contains("puppets: 1"));

    // Stream ends (grace period 0): puppet quits, topic and +m.
    twitch.mock.live.store(false, Ordering::SeqCst);
    send_ws(
        &mut es,
        notification(
            "stream.offline",
            "n3",
            json!({ "broadcaster_user_id": "111" }),
        ),
    )
    .await;
    assert_eq!(puppet.expect("QUIT").await, "QUIT :Stream ended");
    assert!(
        control
            .expect("TOPIC")
            .await
            .starts_with("TOPIC #stream :📴")
    );
    assert_eq!(control.expect("MODE").await, "MODE #stream +m");
}

async fn wait_for(f: impl Fn() -> Vec<String>) -> Vec<String> {
    for _ in 0..50 {
        let v = f();
        if !v.is_empty() {
            return v;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("timed out");
}

/// The hosted setup (slashcheer-irc's generated config): manual trigger, the stream is
/// offline, and an op starts the bridge by hand before anyone talks.
#[tokio::test]
async fn manual_start_relays_both_ways() {
    let mut twitch = harness().await;
    let (irc_url, mut irc_conns) = fake_server().await;

    let mut cfg = Config::parse(include_str!("../config.example.toml")).unwrap();
    cfg.twitch = twitch.cfg.clone();
    cfg.irc = irc_config(&irc_url);
    cfg.lifecycle.trigger = crate::config::Trigger::Manual;

    let (tx, rx) = mpsc::channel(64);
    let irc = IrcClient::start(cfg.irc.clone(), token(), tx.clone());
    let tw = TwitchClient::start(cfg.twitch.clone(), creds(None), tx)
        .await
        .unwrap();
    tokio::spawn(bridge::run(
        Bridge::new(cfg, tw, irc),
        rx,
        Duration::from_millis(100),
    ));

    let mut control = irc_conns.recv().await.unwrap();
    control.expect("NICK").await;
    control.send(":srv 001 twbridge :Welcome").await;
    control.expect("JOIN").await;
    control.send(":twbridge!~u@h JOIN #stream").await;
    control
        .send(":srv 353 twbridge = #stream :@mihok sarahk twbridge")
        .await;
    let mut es = twitch.ws_conns.recv().await.unwrap();
    send_ws(&mut es, welcome("S1", 10)).await;

    control
        .send(":mihok!~u@h PRIVMSG #stream :!bridge start")
        .await;
    assert!(control.expect("TOPIC").await.starts_with("TOPIC #stream :"));
    let notice = control.expect("NOTICE mihok").await;
    assert!(notice.contains("started by hand"), "{notice}");

    // IRC -> Twitch
    control
        .send(":sarahk!~u@h PRIVMSG #stream :hello from irc")
        .await;
    let sent = wait_for(|| twitch.mock.lines("send")).await;
    assert!(sent[0].contains("[irc] sarahk: hello from irc"), "{sent:?}");

    // Twitch -> IRC
    send_ws(
        &mut es,
        notification(
            "channel.chat.message",
            "n2",
            chat_event("hello from twitch", None),
        ),
    )
    .await;
    let mut puppet = irc_conns.recv().await.unwrap();
    puppet.expect("NICK").await;
    puppet.send(":srv 001 NeuralNomad|twitch :Welcome").await;
    puppet.expect("JOIN").await;
    puppet.send(":NeuralNomad|twitch!~u@h JOIN #stream").await;
    assert_eq!(
        puppet.expect("PRIVMSG").await,
        "PRIVMSG #stream :hello from twitch"
    );
}
