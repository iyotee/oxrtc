//! `GET /api/frame.mp4` (F4): one keyframe as an init segment and one
//! fragment, with its length and go2rtc's type; the cached keyframe at once
//! from a running source; the picture alone; `filename`; the refusals; a
//! source with no picture in time; the subscription gone once answered.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing, clippy::panic)]

mod common;

use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use common::*;
use oxrtc_api::{ApiState, Mp4Limits};
use oxrtc_core::{Codec, CodecParams, Hub, TrackInfo};
use oxrtc_mp4::fuzzing::{H264_PPS, H264_SPS, hex};
use oxrtc_mp4::reader::{atoms, read_fragment, read_init};
use tower::ServiceExt;

const NON_SYNC: u32 = 0x0001_0000;

fn h264() -> TrackInfo {
    TrackInfo::video(Codec::H264).with_params(CodecParams::H264 { sps: hex(H264_SPS), pps: hex(H264_PPS) })
}

fn setup(limits: Mp4Limits) -> (axum::Router, Arc<Hub>) {
    let factory = Arc::new(FakeFactory::default());
    // Pictures that carry their number, a keyframe every 10, with A-law.
    let numbered =
        Script { video_bytes: 64, ..Script::tracks(vec![h264(), TrackInfo::audio(Codec::Pcma, 8000, 1)], 10_000) };
    factory.add("cam", numbered);
    factory.add("silent", Script::silent(vec![(oxrtc_core::MediaKind::Video, Codec::H264)]));
    factory.add("sound", Script::tracks(vec![TrackInfo::audio(Codec::Pcma, 8000, 1)], 10_000));
    let hub = hub(factory);
    for name in ["cam", "silent", "sound"] {
        hub.upsert(name, &[&format!("fake:{name}")]).unwrap();
    }
    (oxrtc_api::router(ApiState::new(hub.clone(), "1.9.15", auth()).with_mp4_limits(limits)), hub)
}

async fn get(router: &axum::Router, uri: &str) -> (StatusCode, axum::http::HeaderMap, bytes::Bytes) {
    let request = Request::get(uri).header(header::AUTHORIZATION, basic(USER, PASS)).body(Body::empty()).unwrap();
    let response = router.clone().oneshot(request).await.unwrap();
    let (parts, body) = response.into_parts();
    (parts.status, parts.headers, axum::body::to_bytes(body, usize::MAX).await.unwrap())
}

/// The file as its init segment and its fragments' bytes.
fn split(file: &[u8]) -> (Vec<u8>, Vec<Vec<u8>>) {
    let top = atoms(file, 0).unwrap();
    let kinds: Vec<[u8; 4]> = top.iter().map(|a| a.kind).collect();
    assert_eq!(kinds, vec![*b"ftyp", *b"moov", *b"moof", *b"mdat"], "one init and one fragment");
    let init = [top[0].whole, top[1].whole].concat();
    (init, vec![[top[2].whole, top[3].whole].concat()])
}

#[tokio::test]
async fn one_keyframe_as_a_short_mp4_with_its_length() {
    let (router, hub) = setup(Mp4Limits::default());
    let (status, headers, file) = get(&router, "/api/frame.mp4?src=cam").await;
    assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&file));
    assert_eq!(headers[header::CONTENT_TYPE], "video/mp4; codecs=\"avc1.64001e\"", "the picture alone");
    assert_eq!(headers[header::CONTENT_LENGTH], file.len().to_string().as_str(), "Safari shows none without it");
    let (init, fragments) = split(&file);
    let init = read_init(&init).unwrap();
    assert_eq!(init.tracks.len(), 1);
    let fragment = read_fragment(&fragments[0], &init.trex).unwrap();
    let sample = &fragment.runs[0].samples[0];
    assert_eq!((fragment.runs[0].samples.len(), sample.decode_time), (1, 0));
    assert_eq!(sample.flags & NON_SYNC, 0, "a keyframe");
    assert_eq!(sample.data[4], 0x65, "an IDR slice");
    assert!(eventually(Duration::from_millis(300), || hub.get("cam").unwrap().snapshot().consumers.is_empty()).await);
}

#[tokio::test]
async fn a_running_source_gives_its_cached_keyframe_at_once() {
    let (router, hub) = setup(Mp4Limits::default());
    // Keep the source running, and let a keyframe and a few pictures pass.
    let _held = hub.get("cam").unwrap().preload(&oxrtc_core::Want::all()).await.unwrap();
    tokio::time::sleep(Duration::from_millis(400)).await;
    let began = Instant::now();
    let (status, _, file) = get(&router, "/api/frame.mp4?src=cam").await;
    assert_eq!(status, StatusCode::OK);
    assert!(began.elapsed() < Duration::from_millis(150), "not the next keyframe: {:?}", began.elapsed());
    let (init, fragments) = split(&file);
    let sample = &read_fragment(&fragments[0], &read_init(&init).unwrap().trex).unwrap().runs[0].samples[0];
    let number = u64::from_be_bytes(sample.data[5..13].try_into().unwrap());
    assert_eq!(number % 20, 0, "a keyframe (every tenth picture of two tracks): {number}");
}

#[tokio::test]
async fn a_camera_whose_parameter_sets_in_band_differ_from_its_description_gets_the_init_its_picture_uses() {
    let factory = Arc::new(FakeFactory::default());
    let in_band = CodecParams::H264 { sps: hex(H264_SPS), pps: hex("68ee3c80") };
    let script = Script {
        in_band: Some(vec![TrackInfo::video(Codec::H264).with_params(in_band)]),
        ..Script::tracks(vec![h264()], 10_000)
    };
    factory.add("cam", script);
    let hub = hub(factory);
    hub.upsert("cam", &["fake:cam"]).unwrap();
    let router = oxrtc_api::router(ApiState::new(hub, "1.9.15", auth()));
    let (status, _, file) = get(&router, "/api/frame.mp4?src=cam").await;
    assert_eq!(status, StatusCode::OK);
    let (init, _) = split(&file);
    let init = read_init(&init).unwrap();
    // An `avcC` of High profile has bytes after its PPS.
    let pps = hex("68ee3c80");
    let config = &init.tracks[0].config;
    assert!(config.windows(pps.len()).any(|w| w == pps), "the parameter sets in band, which the picture uses");
}

/// A server whose `cam` publishes H.264 with no parameter sets: its
/// subscription runs, its frames come, and no init segment can ever be
/// written, so a `stream.mp4` or `frame.mp4` of it waits in its head.
async fn waiting_server() -> (common::ws::Served, Arc<Hub>) {
    let factory = Arc::new(FakeFactory::default());
    factory.add("cam", Script::tracks(vec![TrackInfo::video(Codec::H264)], 100_000));
    let hub = hub(factory);
    hub.upsert("cam", &["fake:cam"]).unwrap();
    let state = ApiState::new(hub.clone(), "1.9.15", auth());
    (common::ws::serve(state, oxrtc_api::ServeLimits::default()).await, hub)
}

/// Sends `GET path` and waits until its subscription is listed.
async fn waiting(server: &common::ws::Served, hub: &Hub, path: &str) -> tokio::net::TcpStream {
    use tokio::io::AsyncWriteExt;
    let mut stream = tokio::net::TcpStream::connect(server.addr).await.unwrap();
    let request = format!("GET {path} HTTP/1.1\r\nHost: x\r\nAuthorization: {}\r\n\r\n", basic(USER, PASS));
    stream.write_all(request.as_bytes()).await.unwrap();
    let listed = || hub.get("cam").unwrap().snapshot().consumers.len() == 1;
    assert!(eventually(Duration::from_secs(2), listed).await, "{path}: subscribed");
    stream
}

fn none_left(hub: &Hub) -> bool {
    hub.get("cam").unwrap().snapshot().consumers.is_empty()
}

#[tokio::test]
async fn a_client_gone_while_its_head_waits_ends_the_subscription_at_once() {
    let (server, hub) = waiting_server().await;
    for path in ["/api/frame.mp4?src=cam", "/api/stream.mp4?src=cam"] {
        let stream = waiting(&server, &hub, path).await;
        tokio::time::sleep(Duration::from_millis(200)).await;
        drop(stream);
        assert!(eventually(Duration::from_millis(500), || none_left(&hub)).await, "{path}: its client gone");
    }
}

#[tokio::test]
async fn the_servers_stop_ends_a_head_that_waits_at_once() {
    for path in ["/api/frame.mp4?src=cam", "/api/stream.mp4?src=cam"] {
        let (server, hub) = waiting_server().await;
        let _stream = waiting(&server, &hub, path).await;
        server.cancel.cancel();
        assert!(eventually(Duration::from_millis(500), || none_left(&hub)).await, "{path}: the server stopped");
    }
}

#[tokio::test]
async fn filename_and_refusals() {
    let limits = Mp4Limits { frame_timeout: Duration::from_millis(300), ..Mp4Limits::default() };
    let (router, _hub) = setup(limits);
    let (_, headers, _) = get(&router, "/api/frame.mp4?src=cam&filename=snap.mp4").await;
    assert_eq!(headers[header::CONTENT_DISPOSITION], "attachment; filename=\"snap.mp4\"");
    for (uri, status, text) in [
        ("/api/frame.mp4?src=nope", StatusCode::NOT_FOUND, "stream not found"),
        ("/api/frame.mp4", StatusCode::BAD_REQUEST, "src is required"),
        ("/api/frame.mp4?src=sound", StatusCode::INTERNAL_SERVER_ERROR, "codecs not matched"),
        ("/api/frame.mp4?src=silent", StatusCode::INTERNAL_SERVER_ERROR, "no keyframe within 300ms"),
    ] {
        let (got, _, body) = get(&router, uri).await;
        let body = String::from_utf8_lossy(&body);
        assert_eq!(got, status, "{uri}: {body}");
        assert!(body.contains(text), "{uri}: {body}");
    }
}
