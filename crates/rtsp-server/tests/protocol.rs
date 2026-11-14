//! The RTSP surface against a raw client: media filters, errors, limits.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing, clippy::panic)]

mod common;

use std::sync::Arc;
use std::time::Duration;

use common::{Client, Clip, Factory, config, hub, start};
use oxrtc_core::Hub;
use oxrtc_rtsp_server::RtspServerConfig;
use proptest::prelude::*;
use tokio::io::AsyncWriteExt;

fn synthetic_hub() -> (Arc<Factory>, Arc<Hub>) {
    let factory = Arc::new(Factory::default());
    factory.clip("cam", Clip::synthetic());
    let hub = hub(factory.clone());
    hub.upsert("cam", &["clip:cam"]).unwrap();
    (factory, hub)
}

async fn describe(client: &mut Client, url: &str) -> common::Answer {
    client.request("DESCRIBE", url, &["Accept: application/sdp"], "").await.expect("no answer")
}

#[tokio::test]
async fn options_lists_the_methods() {
    let (_f, hub) = synthetic_hub();
    let server = start(hub, config()).await;
    let mut c = Client::connect(server.addr).await;
    let a = c.request("OPTIONS", &server.url("cam"), &[], "").await.unwrap();
    assert_eq!(a.status, 200);
    let public = a.header("Public").unwrap();
    for m in ["DESCRIBE", "SETUP", "PLAY", "TEARDOWN", "GET_PARAMETER", "ANNOUNCE", "RECORD"] {
        assert!(public.contains(m), "{public}");
    }
}

#[tokio::test]
async fn queries_select_the_described_media() {
    let (_f, hub) = synthetic_hub();
    let server = start(hub, config()).await;
    let mut c = Client::connect(server.addr).await;

    let all = describe(&mut c, &server.url("cam")).await;
    assert_eq!(all.status, 200, "{all:?}");
    assert_eq!(all.header("Content-Type").as_deref(), Some("application/sdp"));
    let video_at = all.body.find("m=video").unwrap();
    let audio_at = all.body.find("m=audio").unwrap();
    assert!(video_at < audio_at, "video first: {}", all.body);
    assert!(all.body.contains("sprop-parameter-sets="), "{}", all.body);
    assert!(all.body.contains("a=rtpmap:8 PCMA/8000"), "{}", all.body);

    let video = describe(&mut c, &server.url("cam?video")).await;
    assert_eq!(video.status, 200);
    assert!(video.body.contains("m=video") && !video.body.contains("m=audio"), "{}", video.body);

    let audio = describe(&mut c, &server.url("cam?audio=pcma")).await;
    assert_eq!(audio.status, 200);
    assert!(audio.body.contains("m=audio") && !audio.body.contains("m=video"), "{}", audio.body);

    let none = describe(&mut c, &server.url("cam?audio=opus")).await;
    assert_eq!(none.status, 404, "no source offers opus");
    let bad = describe(&mut c, &server.url("cam?video=vp9")).await;
    assert_eq!(bad.status, 400);
    let unknown = describe(&mut c, &server.url("nope")).await;
    assert_eq!(unknown.status, 404);
}

#[tokio::test]
async fn one_subscription_per_connection() {
    let (_f, hub) = synthetic_hub();
    let server = start(hub.clone(), config()).await;
    let mut c = Client::connect(server.addr).await;
    for _ in 0..3 {
        assert_eq!(describe(&mut c, &server.url("cam?video")).await.status, 200);
    }
    let consumers = hub.get("cam").unwrap().snapshot().consumers;
    assert_eq!(consumers.len(), 1, "{consumers:?}");
    assert_eq!(consumers[0].kind, "rtsp");
    assert!(consumers[0].remote.as_deref().unwrap().starts_with("127.0.0.1:"));
}

#[tokio::test]
async fn udp_is_refused_and_tcp_plays() {
    let (_f, hub) = synthetic_hub();
    let server = start(hub, config()).await;
    let mut c = Client::connect(server.addr).await;
    let d = describe(&mut c, &server.url("cam")).await;
    let base = d.header("Content-Base").unwrap();
    let setup = |t: &'static str| [t];
    let udp = c
        .request("SETUP", &format!("{base}trackID=0"), &setup("Transport: RTP/AVP;unicast;client_port=5000-5001"), "")
        .await
        .unwrap();
    assert_eq!(udp.status, 461);
    let tcp = c
        .request("SETUP", &format!("{base}trackID=0"), &setup("Transport: RTP/AVP/TCP;unicast;interleaved=0-1"), "")
        .await
        .unwrap();
    assert_eq!(tcp.status, 200, "{tcp:?}");
    assert!(tcp.header("Transport").unwrap().contains("interleaved=0-1"));
    // The audio track cannot take the video track's channels.
    let clash = c
        .request("SETUP", &format!("{base}trackID=1"), &setup("Transport: RTP/AVP/TCP;unicast;interleaved=1-2"), "")
        .await
        .unwrap();
    assert_eq!(clash.status, 400, "{clash:?}");
    let session = tcp.header("Session").unwrap();
    let id = session.split(';').next().unwrap().to_owned();
    let session_header = format!("Session: {id}");
    let wrong = c.request("PLAY", &base, &["Session: 0000"], "").await.unwrap();
    assert_eq!(wrong.status, 454);
    let play = c.request("PLAY", &base, &[session_header.as_str()], "").await.unwrap();
    assert_eq!(play.status, 200);
    assert!(play.header("RTP-Info").unwrap().contains("trackID=0;seq="), "{play:?}");
    // Interleaved RTP on channel 0 follows. It is read through the client's
    // buffer: bytes the PLAY answer already pulled in belong to it, and a
    // half-read frame would hide the next answer.
    let (channel, payload) = c.interleaved().await.unwrap();
    assert_eq!(channel, 0);
    assert_eq!(payload.first().map(|b| b >> 6), Some(2), "RTP version 2");
    let keepalive = c.request("GET_PARAMETER", &base, &[session_header.as_str()], "").await.unwrap();
    assert_eq!(keepalive.status, 200);
    let teardown = c.request("TEARDOWN", &base, &[session_header.as_str()], "").await.unwrap();
    assert_eq!(teardown.status, 200);
}

#[tokio::test]
async fn a_session_that_never_plays_is_dropped() {
    let (_f, hub) = synthetic_hub();
    let config = RtspServerConfig { setup_play_timeout: Duration::from_millis(300), ..config() };
    let server = start(hub.clone(), config).await;
    let mut c = Client::connect(server.addr).await;
    let d = describe(&mut c, &server.url("cam?video")).await;
    let base = d.header("Content-Base").unwrap();
    let s = c
        .request("SETUP", &format!("{base}trackID=0"), &["Transport: RTP/AVP/TCP;unicast;interleaved=0-1"], "")
        .await
        .unwrap();
    assert_eq!(s.status, 200);
    assert!(c.closed_within(Duration::from_secs(3)).await, "not dropped");
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(hub.get("cam").unwrap().snapshot().consumers.is_empty(), "subscription released");
}

#[tokio::test]
async fn a_silent_connection_is_closed() {
    let (_f, hub) = synthetic_hub();
    let config = RtspServerConfig { session_timeout: Duration::from_millis(300), ..config() };
    let server = start(hub, config).await;
    let mut c = Client::connect(server.addr).await;
    assert!(c.closed_within(Duration::from_secs(3)).await);
}

#[tokio::test]
async fn extreme_settings_mean_no_limit_rather_than_a_panic() {
    let (_f, hub) = synthetic_hub();
    let config = RtspServerConfig {
        max_connections: usize::MAX,
        setup_play_timeout: Duration::MAX,
        session_timeout: Duration::MAX,
        ..config()
    };
    let server = start(hub, config).await;
    let mut c = Client::connect(server.addr).await;
    assert_eq!(c.request("OPTIONS", "*", &[], "").await.unwrap().status, 200);
    assert_eq!(describe(&mut c, &server.url("cam?video")).await.status, 200);
    assert!(!server.task.is_finished(), "the server stopped");
}

#[tokio::test]
async fn connections_beyond_the_limit_are_closed() {
    let (_f, hub) = synthetic_hub();
    let config = RtspServerConfig { max_connections: 2, ..config() };
    let server = start(hub, config).await;
    let mut a = Client::connect(server.addr).await;
    let mut b = Client::connect(server.addr).await;
    assert_eq!(a.request("OPTIONS", "*", &[], "").await.unwrap().status, 200);
    assert_eq!(b.request("OPTIONS", "*", &[], "").await.unwrap().status, 200);
    let mut c = Client::connect(server.addr).await;
    assert!(c.closed_within(Duration::from_secs(2)).await, "third connection served");
    drop(a);
    tokio::time::sleep(Duration::from_millis(200)).await;
    let mut d = Client::connect(server.addr).await;
    assert_eq!(d.request("OPTIONS", "*", &[], "").await.unwrap().status, 200, "a freed slot is reused");
}

#[tokio::test]
async fn oversized_and_garbage_requests_close_the_connection() {
    let (_f, hub) = synthetic_hub();
    let config = RtspServerConfig { max_request_bytes: 4096, ..config() };
    let server = start(hub, config).await;

    let mut big = Client::connect(server.addr).await;
    let pad = format!("X-Pad: {}", "a".repeat(10_000));
    let a = big.request("OPTIONS", "*", &[pad.as_str()], "").await;
    assert_eq!(a.map(|a| a.status), Some(413));
    assert!(big.closed_within(Duration::from_secs(2)).await);

    let mut junk = Client::connect(server.addr).await;
    junk.stream.write_all(b"\x16\x03\x01\x00\xa5 hello there\r\n\r\n").await.unwrap();
    assert!(junk.closed_within(Duration::from_secs(2)).await);

    let mut fine = Client::connect(server.addr).await;
    assert_eq!(fine.request("OPTIONS", "*", &[], "").await.unwrap().status, 200, "server still serves");
}

#[tokio::test]
async fn shutdown_closes_connections() {
    let (_f, hub) = synthetic_hub();
    let server = start(hub, config()).await;
    let mut c = Client::connect(server.addr).await;
    assert_eq!(c.request("OPTIONS", "*", &[], "").await.unwrap().status, 200);
    server.cancel.cancel();
    assert!(c.closed_within(Duration::from_secs(3)).await);
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 24, ..ProptestConfig::default() })]

    /// Random bytes on a live connection never take the server down.
    #[test]
    fn random_bytes_never_break_the_server(bytes in proptest::collection::vec(any::<u8>(), 1..20_000)) {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        rt.block_on(async {
            let (_f, hub) = synthetic_hub();
            let config = RtspServerConfig { max_request_bytes: 2048, session_timeout: Duration::from_millis(500), ..config() };
            let server = start(hub, config).await;
            let mut c = Client::connect(server.addr).await;
            let _ = c.stream.write_all(&bytes).await;
            let _ = c.closed_within(Duration::from_secs(2)).await;
            let mut fine = Client::connect(server.addr).await;
            assert_eq!(fine.request("OPTIONS", "*", &[], "").await.unwrap().status, 200);
        });
    }
}
