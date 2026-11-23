//! Where a `stream.mp4`, a `frame.mp4` and an `mse` start (M5 item D (5)):
//! by default at the cached group of pictures' keyframe (a picture at once,
//! the reader about one group behind live), and with `start=next` at the
//! first keyframe that comes after the request, as go2rtc starts every
//! reader. The fake camera numbers its pictures, so a test reads which one
//! came first.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing, clippy::panic)]

mod common;

use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use common::ws::{open_ok, serve};
use common::*;
use futures_util::StreamExt;
use oxrtc_api::{ApiState, ServeLimits};
use oxrtc_core::{Codec, CodecParams, ConsumerTag, Hub, TrackInfo, Want};
use oxrtc_mp4::fuzzing::{H264_PPS, H264_SPS, hex};
use oxrtc_mp4::reader::{atoms, read_fragment, read_init};
use serde_json::json;
use tower::ServiceExt;

/// A keyframe every 10 pictures, 20 ms apart: a group of 200 ms.
fn setup() -> (ApiState, Arc<Hub>) {
    let factory = Arc::new(FakeFactory::default());
    let h264 = TrackInfo::video(Codec::H264).with_params(CodecParams::H264 { sps: hex(H264_SPS), pps: hex(H264_PPS) });
    factory.add("cam", Script { video_bytes: 64, ..Script::tracks(vec![h264], 100_000) });
    let hub = hub(factory);
    hub.upsert("cam", &["fake:cam"]).unwrap();
    (ApiState::new(hub.clone(), "1.9.15", auth()), hub)
}

/// The number the fake camera wrote into a picture, after its NAL header.
fn number(sample: &[u8]) -> u64 {
    u64::from_be_bytes(sample[5..13].try_into().unwrap())
}

/// Pictures the camera published so far.
fn published(hub: &Hub) -> u64 {
    hub.get("cam").unwrap().snapshot().sources[0].frames
}

/// Starts the camera, held by a subscription of the test's own, and waits
/// until its cache holds a group and a few pictures past its keyframe: the
/// cached keyframe is then older than the request.
async fn running(hub: &Hub) -> oxrtc_core::Subscription {
    let keeper = hub.get("cam").unwrap().subscribe(&Want::all(), ConsumerTag::new("keep")).await.unwrap();
    assert!(eventually(Duration::from_secs(5), || published(hub) > 20).await);
    assert!(eventually(Duration::from_secs(5), || (3..=6).contains(&(published(hub) % 10))).await);
    keeper
}

/// The first picture of a `GET` of `uri`, and the pictures published when it was asked.
async fn first_picture(hub: &Hub, router: &axum::Router, uri: &str) -> (u64, u64) {
    let asked = published(hub);
    let request = Request::get(uri).header(header::AUTHORIZATION, basic(USER, PASS)).body(Body::empty()).unwrap();
    let response = router.clone().oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let mut body = response.into_body().into_data_stream();
    let mut bytes = Vec::new();
    while let Some(chunk) = tokio::time::timeout(Duration::from_secs(5), body.next()).await.unwrap() {
        bytes.extend_from_slice(&chunk.unwrap());
        // The init segment's boxes, then the first fragment's, once all came.
        let Ok(boxes) = atoms(&bytes, 0) else { continue };
        let Some(at) = boxes.iter().position(|b| &b.kind == b"moof") else { continue };
        if boxes.len() < at + 2 {
            continue;
        }
        let init: Vec<u8> = boxes[..at].iter().flat_map(|b| b.whole.to_vec()).collect();
        let fragment: Vec<u8> = boxes[at..at + 2].iter().flat_map(|b| b.whole.to_vec()).collect();
        let init = read_init(&init).unwrap();
        let fragment = read_fragment(&fragment, &init.trex).unwrap();
        return (number(&fragment.runs[0].samples[0].data), asked);
    }
    panic!("no picture");
}

#[tokio::test]
async fn stream_mp4_starts_at_the_cached_keyframe_or_with_start_next_at_the_next_one() {
    let (state, hub) = setup();
    let router = oxrtc_api::router(state);
    let _keeper = running(&hub).await;
    let (first, asked) = first_picture(&hub, &router, "/api/stream.mp4?src=cam").await;
    assert!(first % 10 == 0 && first < asked, "the cached keyframe {first}, asked at {asked}");
    let (first, asked) = first_picture(&hub, &router, "/api/stream.mp4?src=cam&start=next").await;
    assert!(first % 10 == 0 && first >= asked, "a keyframe after the request: {first}, asked at {asked}");
    let (first, asked) = first_picture(&hub, &router, "/api/stream.mp4?src=cam&start=cache").await;
    assert!(first < asked, "start=cache is the default: {first}, asked at {asked}");
}

#[tokio::test]
async fn frame_mp4_with_start_next_is_a_picture_taken_after_the_request() {
    let (state, hub) = setup();
    let router = oxrtc_api::router(state);
    let _keeper = running(&hub).await;
    let (first, asked) = first_picture(&hub, &router, "/api/frame.mp4?src=cam").await;
    assert!(first < asked, "{first}, asked at {asked}");
    let (first, asked) = first_picture(&hub, &router, "/api/frame.mp4?src=cam&start=next").await;
    assert!(first >= asked, "{first}, asked at {asked}");
}

#[tokio::test]
async fn a_start_that_is_not_cache_or_next_is_refused() {
    let (state, _hub) = setup();
    let router = oxrtc_api::router(state);
    for uri in [
        "/api/stream.mp4?src=cam&start=now",
        "/api/frame.mp4?src=cam&start=",
        "/api/stream.mp4?src=cam&start=next&start=cache",
    ] {
        let request = Request::get(uri).header(header::AUTHORIZATION, basic(USER, PASS)).body(Body::empty()).unwrap();
        let response = router.clone().oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{uri}");
    }
}

#[tokio::test]
async fn an_mse_socket_opened_with_start_next_starts_at_the_next_keyframe() {
    let (state, hub) = setup();
    let _keeper = running(&hub).await;
    let server = serve(state, ServeLimits::default()).await;
    for (query, after) in [("?src=cam", false), ("?src=cam&start=next", true)] {
        let mut client = open_ok(server.addr, query).await;
        let asked = published(&hub);
        client.send(json!({"type": "mse", "value": "avc1.640029"})).await;
        assert_eq!(client.json().await["type"], "mse");
        let init = read_init(&client.binary().await).unwrap();
        let fragment = read_fragment(&client.binary().await, &init.trex).unwrap();
        let first = number(&fragment.runs[0].samples[0].data);
        assert_eq!(first >= asked, after, "{query}: {first}, asked at {asked}");
    }
}
