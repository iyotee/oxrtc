//! `/api/ws`, go2rtc's WebSocket API, over real TCP: the offer in both
//! dialects, the answer and the server's candidates, the client's candidates
//! before and after the answer, the error form, authentication, the origin,
//! the socket's end and the server's stop.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing, clippy::panic)]

mod common;

use std::sync::Arc;
use std::time::Duration;

use common::ws::{Handshake, Next, error_of, open, open_ok, serve};
use common::*;
use oxrtc_api::{AnswerError, ApiState, OriginPolicy, ServeLimits};
use oxrtc_core::Codec;
use serde_json::json;

const ANSWER: &str = "v=0\r\nm=video 9 UDP/TLS/RTP/SAVPF 96\r\n\
                      a=candidate:1 1 udp 2130706431 127.0.0.1 40000 typ host\r\n\
                      a=candidate:2 1 tcp 1671430143 127.0.0.1 40000 typ host tcptype passive\r\n\
                      a=end-of-candidates\r\n";
const OFFER: &str = "v=0\r\no=- 1 1 IN IP4 0.0.0.0\r\ns=-\r\n";
const CANDIDATE: &str = "candidate:7 1 udp 2130706431 192.168.1.9 50000 typ host";

fn state(answerer: &Arc<FakeAnswerer>) -> (ApiState, Arc<FakeFactory>) {
    let factory = Arc::new(FakeFactory::default());
    factory.add("cam1", Script::video(Codec::H264, 0));
    let hub = hub(factory.clone());
    hub.upsert("cam", &["fake:cam1"]).unwrap();
    (ApiState::new(hub, "1.9.15", auth()).with_webrtc(answerer.clone()), factory)
}

#[tokio::test]
async fn an_offer_is_answered_then_the_candidates_follow() {
    let answerer = FakeAnswerer::new(Ok(ANSWER.into()));
    let server = serve(state(&answerer).0, ServeLimits::default()).await;
    let auth = basic(USER, PASS);
    let handshake = Handshake { authorization: Some(&auth), user_agent: Some("go2rtc-player"), ..Handshake::default() };
    let mut client = open(server.addr, "?src=cam", handshake).await.unwrap();
    client.send(json!({"type": "webrtc/offer", "value": OFFER})).await;
    assert_eq!(
        client.json().await,
        json!({"type": "webrtc/answer", "value": ANSWER}),
        "the answer carries every candidate"
    );
    for line in [
        "candidate:1 1 udp 2130706431 127.0.0.1 40000 typ host",
        "candidate:2 1 tcp 1671430143 127.0.0.1 40000 typ host tcptype passive",
    ] {
        assert_eq!(
            client.json().await,
            json!({"type": "webrtc/candidate", "value": line}),
            "and each is sent as go2rtc does"
        );
    }
    assert_eq!(client.next(Duration::from_millis(200)).await, Next::Nothing);
    let seen = answerer.seen.lock().unwrap().clone();
    assert_eq!(
        seen,
        vec![("cam".to_owned(), OFFER.to_owned(), Some(client.local))],
        "the offer, from the socket's address"
    );
    assert_eq!(*answerer.agents.lock().unwrap(), vec![Some("go2rtc-player".to_owned())]);
}

#[tokio::test]
async fn home_assistants_dialect_is_answered_in_kind() {
    let answerer = FakeAnswerer::new(Ok(ANSWER.into()));
    let server = serve(state(&answerer).0, ServeLimits::default()).await;
    let mut client = open_ok(server.addr, "?src=cam").await;
    let offer = json!({"type": "webrtc", "value": {"type": "offer", "sdp": OFFER, "ice_servers": [{"urls": ["stun:stun.example:3478"]}]}});
    client.send(offer).await;
    assert_eq!(client.json().await, json!({"type": "webrtc", "value": {"type": "answer", "sdp": ANSWER}}));
    for _ in 0..2 {
        assert_eq!(client.json().await["type"], "webrtc/candidate", "the type Home Assistant's library reads");
    }
    // An object that is not an offer is said so, in that dialect.
    client.send(json!({"type": "webrtc", "value": {"type": "answer", "sdp": OFFER}})).await;
    let reply = client.json().await;
    assert!(error_of(&reply).starts_with("webrtc: bad offer"), "{reply}");
}

#[tokio::test]
async fn candidates_before_and_after_the_answer_reach_the_session() {
    let answerer = FakeAnswerer::slow(Ok(ANSWER.into()), Duration::from_millis(300));
    let server = serve(state(&answerer).0, ServeLimits::default()).await;
    let mut client = open_ok(server.addr, "?src=cam").await;
    // go2rtc's player can trickle its first candidate before its offer.
    client.send(json!({"type": "webrtc/candidate", "value": CANDIDATE})).await;
    client.send(json!({"type": "webrtc/offer", "value": OFFER})).await;
    client.send(json!({"type": "webrtc/candidate", "value": format!("a={CANDIDATE}x")})).await;
    client.send(json!({"type": "webrtc/candidate", "value": ""})).await;
    assert!(answerer.patched.lock().unwrap().is_empty(), "nothing before the session exists");
    assert_eq!(client.json().await["type"], "webrtc/answer");
    client.send(json!({"type": "webrtc/candidate", "value": format!("{CANDIDATE}y")})).await;
    assert!(eventually(Duration::from_secs(2), || answerer.patched.lock().unwrap().len() == 3).await);
    let patched = answerer.patched.lock().unwrap().clone();
    let fragment = |line: String| (RESOURCE.to_owned(), format!("a={line}\r\n"));
    assert_eq!(
        patched,
        vec![fragment(CANDIDATE.into()), fragment(format!("{CANDIDATE}x")), fragment(format!("{CANDIDATE}y"))],
        "as PATCH bodies, in order; the empty one asks nothing"
    );
}

#[tokio::test]
async fn failures_use_go2rtcs_error_form_and_keep_the_socket() {
    let answerer =
        FakeAnswerer::new(Err(AnswerError::Unavailable("rtsp://admin:hunter2@10.0.0.9/live: refused".into())));
    let server = serve(state(&answerer).0, ServeLimits::default()).await;
    let mut unknown = open_ok(server.addr, "?src=nope").await;
    unknown.send(json!({"type": "webrtc/offer", "value": OFFER})).await;
    assert_eq!(
        unknown.json().await,
        json!({"type": "error", "value": "webrtc/offer: stream not found"}),
        "go2rtc's words"
    );
    let mut nameless = open_ok(server.addr, "").await;
    nameless.send(json!({"type": "webrtc/offer", "value": OFFER})).await;
    assert_eq!(error_of(&nameless.json().await), "webrtc/offer: stream not found");
    let mut client = open_ok(server.addr, "?src=cam").await;
    client.send(json!({"type": "webrtc/offer", "value": OFFER})).await;
    let reply = client.json().await;
    assert_eq!(error_of(&reply), "webrtc/offer: streams: rtsp://***@10.0.0.9/live: refused", "redacted");
    client.send(json!({"type": "webrtc/offer", "value": 12})).await;
    assert!(error_of(&client.json().await).starts_with("webrtc/offer: bad offer"));
    // The types of later items: go2rtc's player reads an `mjpeg` error as
    // nothing more to try (`mse` is served: ws_mse.rs).
    for kind in ["hls", "mjpeg", "mp4"] {
        client.send(json!({"type": kind, "value": "avc1.640029"})).await;
        assert_eq!(error_of(&client.json().await), format!("{kind}: not supported by this server yet"));
    }
    // A type go2rtc has no handler for is not answered.
    client.send(json!({"type": "foo", "value": "bar"})).await;
    assert_eq!(client.next(Duration::from_millis(200)).await, Next::Nothing);
    let codec = FakeAnswerer::new(Err(AnswerError::NoMatchingCodec("no matching codec".into())));
    let server = serve(state(&codec).0, ServeLimits::default()).await;
    let mut client = open_ok(server.addr, "?src=cam").await;
    client.send(json!({"type": "webrtc", "value": {"type": "offer", "sdp": OFFER}})).await;
    assert_eq!(error_of(&client.json().await), "webrtc: no matching codec");
}

#[tokio::test]
async fn what_is_not_a_message_closes_the_socket() {
    let answerer = FakeAnswerer::new(Ok(ANSWER.into()));
    let server = serve(state(&answerer).0, ServeLimits::default()).await;
    let mut client = open_ok(server.addr, "?src=cam").await;
    client.send_text("{not json").await;
    assert_eq!(client.closed().await.0, 1007);
    let mut client = open_ok(server.addr, "?src=cam").await;
    use futures_util::SinkExt;
    client.ws.send(tokio_tungstenite::tungstenite::Message::Binary(vec![1, 2, 3].into())).await.unwrap();
    assert_eq!(client.closed().await.0, 1003);
    assert!(answerer.seen.lock().unwrap().is_empty());
}

#[tokio::test]
async fn the_session_outlives_its_socket_unless_its_answer_was_never_sent() {
    // go2rtc's player closes the socket as soon as the picture plays: the
    // session must go on (go2rtc 1.9.15 sent 40 s of media after the close).
    let answerer = FakeAnswerer::new(Ok(ANSWER.into()));
    let server = serve(state(&answerer).0, ServeLimits::default()).await;
    let mut client = open_ok(server.addr, "?src=cam").await;
    client.send(json!({"type": "webrtc/offer", "value": OFFER})).await;
    assert_eq!(client.json().await["type"], "webrtc/answer");
    client.ws.close(None).await.unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(answerer.closed.lock().unwrap().is_empty(), "the session is not ended by its socket");
    // A socket gone before its answer: nobody can use the session, which is
    // ended at once (go2rtc listed such a consumer for 30 s).
    let slow = FakeAnswerer::slow(Ok(ANSWER.into()), Duration::from_millis(200));
    let server = serve(state(&slow).0, ServeLimits::default()).await;
    let mut client = open_ok(server.addr, "?src=cam").await;
    client.send(json!({"type": "webrtc/offer", "value": OFFER})).await;
    drop(client);
    assert!(
        eventually(Duration::from_secs(3), || slow.closed.lock().unwrap().as_slice() == [RESOURCE.to_owned()]).await
    );
}

#[tokio::test]
async fn the_handshake_is_authenticated_like_every_route() {
    let answerer = FakeAnswerer::new(Ok(ANSWER.into()));
    let server = serve(state(&answerer).0, ServeLimits::default()).await;
    assert_eq!(open(server.addr, "?src=cam", Handshake::default()).await.err(), Some(401));
    let wrong = basic(USER, "nope");
    let handshake = Handshake { authorization: Some(&wrong), ..Handshake::default() };
    assert_eq!(open(server.addr, "?src=cam", handshake).await.err(), Some(401));
    // A credential in the query string is not one (D39).
    let query = format!("?src=cam&token={TOKEN}");
    assert_eq!(open(server.addr, &query, Handshake::default()).await.err(), Some(401));
    let bearer = format!("Bearer {TOKEN}");
    let handshake = Handshake { authorization: Some(&bearer), ..Handshake::default() };
    assert!(open(server.addr, "?src=cam", handshake).await.is_ok());
}

#[tokio::test]
async fn a_page_of_another_host_is_refused() {
    let answerer = FakeAnswerer::new(Ok(ANSWER.into()));
    let server = serve(state(&answerer).0, ServeLimits::default()).await;
    let auth = basic(USER, PASS);
    let from = |origin| Handshake { authorization: Some(&auth), origin: Some(origin), ..Handshake::default() };
    assert_eq!(open(server.addr, "?src=cam", from("http://evil.example")).await.err(), Some(403));
    // The server's own origin is taken; a page of another port of its host
    // is another program's, refused as go2rtc refuses it when the `Host`
    // names a port (measured on 1.9.15).
    let own = format!("http://{}", server.addr);
    assert!(open(server.addr, "?src=cam", from(own.as_str())).await.is_ok());
    assert_eq!(open(server.addr, "?src=cam", from("http://127.0.0.1:8123")).await.err(), Some(403));
    let any = state(&answerer).0.with_ws_origin(OriginPolicy::from_config(Some("*")));
    let server = serve(any, ServeLimits::default()).await;
    assert!(open(server.addr, "?src=cam", from("http://evil.example")).await.is_ok());
}

#[tokio::test]
async fn the_servers_stop_closes_every_socket() {
    let answerer = FakeAnswerer::new(Ok(ANSWER.into()));
    let server = serve(state(&answerer).0, ServeLimits::default()).await;
    let mut client = open_ok(server.addr, "?src=cam").await;
    server.cancel.cancel();
    assert_eq!(client.closed().await.0, 1001);
    let ended = tokio::time::timeout(Duration::from_secs(5), server.task).await;
    assert!(ended.unwrap().unwrap().is_ok());
}
