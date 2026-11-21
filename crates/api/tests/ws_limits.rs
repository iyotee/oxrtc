//! The bounds of `/api/ws`: message size, message rate, candidates held
//! before an answer, offers per socket, sockets per address, the ping and the
//! idle timeout, and the listener's request-head clock, which must not run on
//! a socket's frames.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing, clippy::panic)]

mod common;

use std::sync::Arc;
use std::time::Duration;

use common::ws::{Next, error_of, open_ok, serve};
use common::*;
use oxrtc_api::{ApiState, ServeLimits, WsLimits};
use oxrtc_core::Codec;
use serde_json::json;

const OFFER: &str = "v=0\r\no=- 1 1 IN IP4 0.0.0.0\r\ns=-\r\n";

fn state(answerer: &Arc<FakeAnswerer>, limits: WsLimits) -> ApiState {
    let factory = Arc::new(FakeFactory::default());
    factory.add("cam1", Script::video(Codec::H264, 0));
    let hub = hub(factory);
    hub.upsert("cam", &["fake:cam1"]).unwrap();
    ApiState::new(hub, "1.9.15", auth()).with_webrtc(answerer.clone()).with_ws_limits(limits)
}

fn candidate(n: usize) -> serde_json::Value {
    json!({"type": "webrtc/candidate", "value": format!("candidate:{n} 1 udp 1 192.168.1.9 {} typ host", 50000 + n)})
}

#[tokio::test]
async fn an_oversized_message_closes_the_socket_unread() {
    let answerer = FakeAnswerer::new(Ok("v=0".into()));
    let limits = WsLimits { max_message_bytes: 1024, ..WsLimits::default() };
    let server = serve(state(&answerer, limits), ServeLimits::default()).await;
    let mut client = open_ok(server.addr, "?src=cam").await;
    let big = json!({"type": "webrtc/offer", "value": format!("v=0\r\n{}", "a=x\r\n".repeat(400))});
    let _ = client.try_send_text(big.to_string()).await;
    let next = client.next(Duration::from_secs(5)).await;
    assert!(matches!(next, Next::Closed(1008, _) | Next::Ended), "{next:?}");
    assert!(answerer.seen.lock().unwrap().is_empty(), "the offer never reached WebRTC");
    // The default takes a browser's offer with room to spare.
    assert!(WsLimits::default().max_message_bytes >= 64 * 1024);
}

#[tokio::test]
async fn too_many_messages_in_a_second_close_the_socket() {
    let answerer = FakeAnswerer::new(Ok("v=0".into()));
    let limits = WsLimits { max_messages_per_second: 10, ..WsLimits::default() };
    let server = serve(state(&answerer, limits), ServeLimits::default()).await;
    let mut client = open_ok(server.addr, "?src=cam").await;
    for _ in 0..10 {
        client.send(json!({"type": "foo"})).await;
    }
    assert_eq!(client.next(Duration::from_millis(300)).await, Next::Nothing, "ten are taken");
    tokio::time::sleep(Duration::from_millis(1100)).await;
    for _ in 0..11 {
        if !client.try_send_text(json!({"type": "foo"}).to_string()).await {
            break;
        }
    }
    assert_eq!(client.closed().await, (1008, "too many messages".to_owned()));
}

#[tokio::test]
async fn pings_count_against_the_message_rate() {
    // A ping costs the server a read and a pong; a client that flooded them
    // was never counted, and kept its socket open at any rate.
    use futures_util::SinkExt;
    use tokio_tungstenite::tungstenite::Message;
    let answerer = FakeAnswerer::new(Ok("v=0".into()));
    let limits = WsLimits { max_messages_per_second: 10, ..WsLimits::default() };
    let server = serve(state(&answerer, limits), ServeLimits::default()).await;
    let mut client = open_ok(server.addr, "?src=cam").await;
    for _ in 0..10 {
        client.ws.send(Message::Ping(Default::default())).await.unwrap();
    }
    assert_eq!(client.next(Duration::from_millis(300)).await, Next::Nothing, "ten are taken");
    tokio::time::sleep(Duration::from_millis(1100)).await;
    for _ in 0..200 {
        if client.ws.send(Message::Ping(Default::default())).await.is_err() {
            break;
        }
    }
    assert_eq!(client.closed().await, (1008, "too many messages".to_owned()));
}

#[tokio::test]
async fn a_candidate_longer_than_any_real_one_is_refused_not_held() {
    // 64 candidates of a message's size each held before an answer were
    // 4 MiB a socket, 256 MiB from one address's 64 sockets.
    let answerer = FakeAnswerer::slow(Ok("v=0".into()), Duration::from_millis(300));
    let server = serve(state(&answerer, WsLimits::default()), ServeLimits::default()).await;
    let mut client = open_ok(server.addr, "?src=cam").await;
    client.send(json!({"type": "webrtc/offer", "value": OFFER})).await;
    let long = format!("candidate:1 1 udp 1 192.168.1.9 50000 typ host {}", "x".repeat(60 * 1024));
    client.send(json!({"type": "webrtc/candidate", "value": long})).await;
    assert_eq!(error_of(&client.json().await), "webrtc/candidate: the candidate is too long");
    // A browser's longest (an IPv6 one with Chrome's extensions) is taken.
    let real = "candidate:1467250027 1 udp 2122262783 2001:db8:85a3:8d3:1319:8a2e:370:7348 41234 typ host \
                generation 0 ufrag EsAw network-id 2 network-cost 10";
    client.send(json!({"type": "webrtc/candidate", "value": real})).await;
    assert_eq!(client.json().await["type"], "webrtc/answer");
    assert!(eventually(Duration::from_secs(2), || answerer.patched.lock().unwrap().len() == 1).await);
    assert_eq!(answerer.patched.lock().unwrap()[0].1, format!("a={real}\r\n"));
    assert!(WsLimits::default().max_candidate_bytes >= 512);
}

#[tokio::test]
async fn candidates_held_for_an_answer_and_offers_are_bounded() {
    let answerer = FakeAnswerer::slow(Ok("v=0".into()), Duration::from_millis(400));
    let limits = WsLimits { max_early_candidates: 3, max_offers: 2, ..WsLimits::default() };
    let server = serve(state(&answerer, limits), ServeLimits::default()).await;
    let mut client = open_ok(server.addr, "?src=cam").await;
    client.send(json!({"type": "webrtc/offer", "value": OFFER})).await;
    client.send(json!({"type": "webrtc/offer", "value": OFFER})).await;
    assert_eq!(error_of(&client.json().await), "webrtc/offer: an offer is already being answered");
    for n in 0..4 {
        client.send(candidate(n)).await;
    }
    assert_eq!(error_of(&client.json().await), "webrtc/candidate: too many candidates before the answer");
    assert_eq!(client.json().await["type"], "webrtc/answer");
    assert!(eventually(Duration::from_secs(2), || answerer.patched.lock().unwrap().len() == 3).await, "the three held");
    client.send(json!({"type": "webrtc/offer", "value": OFFER})).await;
    assert_eq!(client.json().await["type"], "webrtc/answer");
    client.send(json!({"type": "webrtc/offer", "value": OFFER})).await;
    assert_eq!(error_of(&client.json().await), "webrtc/offer: too many offers on one socket");
    assert_eq!(answerer.seen.lock().unwrap().len(), 2);
}

#[tokio::test]
async fn sockets_are_bounded_per_address() {
    let answerer = FakeAnswerer::new(Ok("v=0".into()));
    let limits = WsLimits { max_sockets_per_ip: 2, ..WsLimits::default() };
    let server = serve(state(&answerer, limits), ServeLimits::default()).await;
    let first = open_ok(server.addr, "?src=cam").await;
    let _second = open_ok(server.addr, "?src=cam").await;
    let auth = basic(USER, PASS);
    let handshake = common::ws::Handshake { authorization: Some(&auth), ..Default::default() };
    assert_eq!(common::ws::open(server.addr, "?src=cam", handshake.clone()).await.err(), Some(503));
    drop(first);
    let reopened = async {
        loop {
            if common::ws::open(server.addr, "?src=cam", handshake.clone()).await.is_ok() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    };
    tokio::time::timeout(Duration::from_secs(3), reopened).await.expect("a closed socket gives its slot back");
    // Plain requests keep their own slots: the default leaves half the
    // listener's connections to them.
    assert_eq!(WsLimits::default().max_sockets, oxrtc_api::MAX_CONNECTIONS / 2);
}

#[tokio::test]
async fn a_socket_that_answers_no_ping_is_closed() {
    let answerer = FakeAnswerer::new(Ok("v=0".into()));
    let limits = WsLimits {
        ping_every: Duration::from_millis(100),
        idle_timeout: Duration::from_millis(500),
        ..WsLimits::default()
    };
    let server = serve(state(&answerer, limits), ServeLimits::default()).await;
    // A client that reads answers the pings (tungstenite does it as it reads).
    let mut alive = open_ok(server.addr, "?src=cam").await;
    assert_eq!(alive.next(Duration::from_millis(1500)).await, Next::Nothing, "pings answered: still open");
    alive.send(json!({"type": "mse"})).await;
    assert_eq!(alive.json().await["type"], "error");
    // One that reads nothing sends no pong.
    let mut silent = open_ok(server.addr, "?src=cam").await;
    tokio::time::sleep(Duration::from_millis(900)).await;
    assert_eq!(silent.closed().await, (1000, "idle".to_owned()));
}

#[tokio::test]
async fn the_request_head_clock_does_not_run_on_frames() {
    // The listener cuts a connection whose request head takes longer than
    // its bound; frames after the upgrade are no request head.
    let answerer = FakeAnswerer::new(Ok("v=0".into()));
    let listener = ServeLimits { head_timeout: Duration::from_millis(200), ..ServeLimits::default() };
    let server = serve(state(&answerer, WsLimits::default()), listener).await;
    let mut client = open_ok(server.addr, "?src=cam").await;
    for _ in 0..3 {
        client.send(json!({"type": "foo"})).await;
        tokio::time::sleep(Duration::from_millis(400)).await;
    }
    client.send(json!({"type": "mse"})).await;
    let reply = client.json().await;
    assert_eq!(error_of(&reply), "mse: not supported by this server yet", "the socket is still served");
}
