//! Publications on one-time tokens, with a raw client.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing, clippy::panic)]

mod common;

use std::sync::Arc;
use std::time::Duration;

use common::{Client, Factory, config, hub, start};
use oxrtc_core::{Codec, ConsumerTag, Event, MediaKind, ProducerSink, Want};
use tokio::io::AsyncWriteExt;

const SDP: &str = "v=0\r\no=- 0 0 IN IP4 127.0.0.1\r\ns=test\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\n\
m=audio 0 RTP/AVP 8\r\na=rtpmap:8 PCMA/8000\r\na=control:streamid=0\r\n";

/// A hub whose stream `pub` hands its sink to the test once a consumer starts it.
async fn sink() -> (Arc<oxrtc_core::Hub>, ProducerSink, oxrtc_core::Subscription) {
    let factory = Arc::new(Factory::default());
    let mut sinks = factory.handoff("pub", vec![(MediaKind::Audio, Codec::Pcma)]);
    let hub = hub(factory);
    let stream = hub.upsert("pub", &["handoff:pub"]).unwrap();
    let sub = stream.subscribe(&Want::all(), ConsumerTag::new("test")).await.unwrap();
    let sink = tokio::time::timeout(Duration::from_secs(5), sinks.recv()).await.unwrap().unwrap();
    (hub, sink, sub)
}

fn path_of(url: &str) -> String {
    url.splitn(4, '/').nth(3).unwrap().to_owned()
}

#[tokio::test]
async fn unregistered_tokens_and_other_paths_are_refused() {
    let (hub, sink, _sub) = sink().await;
    let server = start(hub, config()).await;
    let mut c = Client::connect(server.addr).await;
    let a = c.request("ANNOUNCE", &server.url("_publish/0123456789abcdef0123456789abcdef"), &[], SDP).await.unwrap();
    assert_eq!(a.status, 404);
    let mut c = Client::connect(server.addr).await;
    let a = c.request("ANNOUNCE", &server.url("pub"), &["Content-Type: application/sdp"], SDP).await.unwrap();
    assert_eq!(a.status, 403);
    assert!(c.closed_within(Duration::from_secs(2)).await, "a forbidden publisher is disconnected");
    // A registered token cannot be read from.
    let handle = server.publishers.register(sink);
    let mut c = Client::connect(server.addr).await;
    assert_eq!(c.request("DESCRIBE", &handle.url(), &[], "").await.unwrap().status, 403);
}

#[tokio::test]
async fn handle_url_points_at_the_server() {
    let (hub, sink, _sub) = sink().await;
    let server = start(hub, config()).await;
    let handle = server.publishers.register(sink);
    let url = handle.url();
    let prefix = format!("rtsp://127.0.0.1:{}/_publish/", server.addr.port());
    let token = url.strip_prefix(&prefix).unwrap();
    assert_eq!(token.len(), 32);
    assert!(token.chars().all(|c| c.is_ascii_hexdigit()));
    assert!(!format!("{handle:?}").contains(token), "Debug must not print the token");
}

async fn publish_session(c: &mut Client, url: &str) -> String {
    let a = c.request("ANNOUNCE", url, &["Content-Type: application/sdp"], SDP).await.unwrap();
    assert_eq!(a.status, 200, "{a:?}");
    let s = c
        .request(
            "SETUP",
            &format!("{url}/streamid=0"),
            &["Transport: RTP/AVP/TCP;unicast;interleaved=0-1;mode=record"],
            "",
        )
        .await
        .unwrap();
    assert_eq!(s.status, 200, "{s:?}");
    let session = format!("Session: {}", s.header("Session").unwrap().split(';').next().unwrap());
    let r = c.request("RECORD", url, &[session.as_str()], "").await.unwrap();
    assert_eq!(r.status, 200, "{r:?}");
    session
}

fn rtp(seq: u16, ts: u32) -> Vec<u8> {
    let mut p = vec![b'$', 0, 0, 12 + 160, 0x80, 8];
    p.extend_from_slice(&seq.to_be_bytes());
    p.extend_from_slice(&ts.to_be_bytes());
    p.extend_from_slice(&[0, 0, 0, 1]);
    p.extend_from_slice(&[0xd5; 160]);
    p
}

#[tokio::test]
async fn announced_media_reaches_the_sink() {
    let (hub, sink, mut sub) = sink().await;
    let server = start(hub, config()).await;
    let handle = server.publishers.register(sink);
    let mut c = Client::connect(server.addr).await;
    publish_session(&mut c, &handle.url()).await;
    for i in 0..10u16 {
        c.stream.write_all(&rtp(i, u32::from(i) * 160)).await.unwrap();
    }
    let mut frames = 0;
    while frames < 10 {
        match tokio::time::timeout(Duration::from_secs(5), sub.recv()).await.unwrap().unwrap() {
            Event::Tracks(tracks) => assert_eq!(tracks[0].codec, Codec::Pcma),
            Event::Frame { frame, .. } => {
                assert_eq!(frame.data.len(), 160);
                assert_eq!(frame.pts, i64::from(frames) * 160);
                frames += 1;
            }
            Event::Discontinuity => {}
        }
    }
    let _ = path_of(&handle.url());
}

#[tokio::test]
async fn dropping_the_handle_ends_the_session_and_the_token() {
    let (hub, sink, _sub) = sink().await;
    let server = start(hub, config()).await;
    let handle = server.publishers.register(sink);
    let url = handle.url();
    let mut c = Client::connect(server.addr).await;
    publish_session(&mut c, &url).await;
    drop(handle);
    assert!(c.closed_within(Duration::from_secs(2)).await, "session not ended");
    let mut again = Client::connect(server.addr).await;
    let a = again.request("ANNOUNCE", &url, &["Content-Type: application/sdp"], SDP).await.unwrap();
    assert_eq!(a.status, 404, "token still registered");
}

#[tokio::test]
async fn a_new_publisher_replaces_the_previous_one() {
    let (hub, sink, _sub) = sink().await;
    let server = start(hub, config()).await;
    let handle = server.publishers.register(sink);
    let mut first = Client::connect(server.addr).await;
    publish_session(&mut first, &handle.url()).await;
    let mut second = Client::connect(server.addr).await;
    publish_session(&mut second, &handle.url()).await;
    assert!(first.closed_within(Duration::from_secs(2)).await, "previous publisher kept");
}

#[tokio::test]
async fn record_without_setup_is_refused_and_unknown_sdp_is_unsupported() {
    let (hub, sink, _sub) = sink().await;
    let server = start(hub, config()).await;
    let handle = server.publishers.register(sink);
    let mut c = Client::connect(server.addr).await;
    let a = c.request("ANNOUNCE", &handle.url(), &["Content-Type: application/sdp"], SDP).await.unwrap();
    assert_eq!(a.status, 200);
    assert_eq!(c.request("RECORD", &handle.url(), &[], "").await.unwrap().status, 455);
    let mut c = Client::connect(server.addr).await;
    let odd = "v=0\r\no=- 0 0 IN IP4 127.0.0.1\r\ns=x\r\nt=0 0\r\nm=video 0 RTP/AVP 96\r\na=rtpmap:96 VP9/90000\r\n";
    assert_eq!(c.request("ANNOUNCE", &handle.url(), &[], odd).await.unwrap().status, 415);
}

#[tokio::test]
async fn registrations_are_bounded() {
    let (hub, sink, _sub) = sink().await;
    let server = start(hub, config()).await;
    let handles: Vec<_> = (0..1100).map(|_| server.publishers.register(sink.clone())).collect();
    assert_eq!(format!("{:?}", server.publishers), "Publishers { registered: 1024 }");
    // A handle past the bound gives a URL that is refused.
    let mut c = Client::connect(server.addr).await;
    let last = handles.last().unwrap();
    let a = c.request("ANNOUNCE", &last.url(), &["Content-Type: application/sdp"], SDP).await.unwrap();
    assert_eq!(a.status, 404);
    drop(handles);
    assert_eq!(format!("{:?}", server.publishers), "Publishers { registered: 0 }");
}
