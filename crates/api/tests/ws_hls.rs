//! `hls` on `/api/ws` over real TCP (F6): go2rtc's answer, the master
//! playlist of a new fMP4 session in the list's codecs, whose
//! `hls/playlist.m3u8?id=` go2rtc's player plays from the server's
//! `/api/hls/`; go2rtc's error form before it.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing, clippy::panic)]

mod common;

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use common::ws::{error_of, open_ok, serve};
use common::*;
use oxrtc_api::{ApiState, ServeLimits};
use oxrtc_core::{Codec, CodecParams, Hub, TrackInfo};
use oxrtc_mp4::fuzzing::{H264_PPS, H264_SPS, hex};
use serde_json::json;
use tower::ServiceExt;

fn setup() -> (ApiState, Arc<Hub>) {
    let factory = Arc::new(FakeFactory::default());
    let h264 = TrackInfo::video(Codec::H264).with_params(CodecParams::H264 { sps: hex(H264_SPS), pps: hex(H264_PPS) });
    factory.add("camera", Script::tracks(vec![h264], 100_000));
    let hub = hub(factory);
    hub.upsert("camera", &["fake:camera"]).unwrap();
    (ApiState::new(hub.clone(), "1.9.15", auth()), hub)
}

#[tokio::test(flavor = "multi_thread")]
async fn hls_is_answered_with_the_master_playlist_of_a_session() {
    let (state, hub) = setup();
    let router = oxrtc_api::router(state.clone());
    let server = serve(state, ServeLimits::default()).await;
    let mut client = open_ok(server.addr, "?src=camera").await;
    // What go2rtc's player sends: the types `canPlayType` said yes to.
    client.send(json!({"type": "hls", "value": "avc1.640029,avc1.64002A,hvc1.1.6.L153.B0,mp4a.40.2"})).await;
    let answer = client.json().await;
    assert_eq!(answer["type"], "hls");
    let playlist = answer["value"].as_str().unwrap();
    assert!(playlist.starts_with("#EXTM3U\n"), "{playlist}");
    assert!(playlist.contains("CODECS=\"avc1.64001e\""), "{playlist}");
    let id = playlist.lines().last().unwrap().strip_prefix("hls/playlist.m3u8?id=").unwrap();
    let kinds: Vec<String> = hub.get("camera").unwrap().snapshot().consumers.iter().map(|c| c.kind.clone()).collect();
    assert_eq!(kinds, ["hls/fmp4"]);
    // The player's `hls/` replaced with the server's `/api/hls/`: no header needed.
    let request = Request::get(format!("/api/hls/playlist.m3u8?id={id}")).body(Body::empty()).unwrap();
    let response = router.clone().oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let text =
        String::from_utf8(axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap().to_vec()).unwrap();
    assert!(text.contains("#EXT-X-MAP:URI=\"init.mp4?id="), "{text}");
}

#[tokio::test]
async fn an_error_before_the_answer_is_go2rtcs() {
    let (state, _hub) = setup();
    let server = serve(state, ServeLimits::default()).await;
    let mut client = open_ok(server.addr, "?src=nothing").await;
    client.send(json!({"type": "hls", "value": "avc1.640029"})).await;
    assert_eq!(error_of(&client.json().await), "hls: stream not found");
    let mut client = open_ok(server.addr, "?src=camera").await;
    client.send(json!({"type": "hls", "value": "vp9"})).await;
    assert_eq!(error_of(&client.json().await), "hls: codecs not matched");
}
