//! `GET /api/stream.mp4` (F4): go2rtc's headers, an init segment then one
//! fragment per frame from a keyframe (the media from zero), the codecs `mp4`
//! and the filters choose, `duration` on the media and on the wall clock,
//! `filename`, the refusals, the per-address bound, the end of the
//! subscription with the body (dropped, or its client gone over TCP), the
//! server's stop, a silent source, and a slow reader that skips to the next
//! keyframe. Fragments are read back with `oxrtc-mp4`'s own test reader.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing, clippy::panic)]

mod common;

use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use common::*;
use futures_util::StreamExt;
use oxrtc_api::{ApiState, Mp4Limits, ServeLimits};
use oxrtc_core::{Codec, CodecParams, Hub, TrackInfo};
use oxrtc_mp4::fuzzing::{H264_PPS, H264_SPS, hex};
use oxrtc_mp4::reader::{Init, read_fragment, read_init};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tower::ServiceExt;

/// `trun`'s `sample_is_non_sync_sample`.
const NON_SYNC: u32 = 0x0001_0000;

fn h264() -> TrackInfo {
    TrackInfo::video(Codec::H264).with_params(CodecParams::H264 { sps: hex(H264_SPS), pps: hex(H264_PPS) })
}

/// `cam`: H.264 and A-law; `video`: H.264 alone, a picture every 20 ms of
/// 40 ms of media each; `slow`: the same every 100 ms; `few`: 15 pictures.
fn setup(limits: Mp4Limits, cam: Script) -> (ApiState, Arc<Hub>) {
    let factory = Arc::new(FakeFactory::default());
    factory.add("cam", cam);
    factory.add("video", Script::tracks(vec![h264()], 10_000));
    factory.add("slow", Script { interval: Duration::from_millis(100), ..Script::tracks(vec![h264()], 10_000) });
    factory.add("few", Script::tracks(vec![h264()], 15));
    factory.add("jpeg", Script::video(Codec::Jpeg, 10_000));
    let hub = hub(factory);
    for name in ["cam", "video", "slow", "few", "jpeg"] {
        hub.upsert(name, &[&format!("fake:{name}")]).unwrap();
    }
    (ApiState::new(hub.clone(), "1.9.15", auth()).with_mp4_limits(limits), hub)
}

fn sound_and_picture() -> Script {
    Script::tracks(vec![h264(), TrackInfo::audio(Codec::Pcma, 8000, 1)], 10_000)
}

fn get(uri: &str) -> Request<Body> {
    Request::get(uri).header(header::AUTHORIZATION, basic(USER, PASS)).body(Body::empty()).unwrap()
}

/// A response's status, headers, and its body chunk by chunk.
struct Streamed {
    status: StatusCode,
    headers: axum::http::HeaderMap,
    body: axum::body::BodyDataStream,
}

impl Streamed {
    async fn of(router: &axum::Router, uri: &str) -> Self {
        let response = router.clone().oneshot(get(uri)).await.unwrap();
        let (parts, body) = response.into_parts();
        Self { status: parts.status, headers: parts.headers, body: body.into_data_stream() }
    }

    fn header(&self, name: header::HeaderName) -> &str {
        self.headers.get(name).map_or("", |v| v.to_str().unwrap())
    }

    async fn chunk(&mut self) -> Option<bytes::Bytes> {
        tokio::time::timeout(Duration::from_secs(5), self.body.next()).await.unwrap().map(Result::unwrap)
    }
}

/// A fragment's one sample: its track, decode time, duration and whether it
/// is a sync sample.
fn only(data: &[u8], init: &Init) -> (u32, u64, u32, bool) {
    let fragment = read_fragment(data, &init.trex).unwrap();
    assert_eq!(fragment.runs.len(), 1, "one frame per fragment");
    let (run, sample) = (&fragment.runs[0], &fragment.runs[0].samples[0]);
    assert_eq!(run.samples.len(), 1, "one frame per fragment");
    (run.track_id, sample.decode_time, sample.duration, sample.flags & NON_SYNC == 0)
}

/// The whole body: its init segment and every fragment's sample.
async fn whole(mut streamed: Streamed) -> (Init, Vec<(u32, u64, u32, bool)>) {
    let init = read_init(&streamed.chunk().await.unwrap()).unwrap();
    let mut samples = Vec::new();
    while let Some(chunk) = streamed.chunk().await {
        samples.push(only(&chunk, &init));
    }
    (init, samples)
}

fn consumers(hub: &Hub, stream: &str) -> Vec<oxrtc_core::ConsumerSnapshot> {
    hub.get(stream).unwrap().snapshot().consumers
}

#[tokio::test]
async fn go2rtcs_headers_then_the_init_then_one_fragment_per_frame_from_a_keyframe() {
    let (state, hub) = setup(Mp4Limits::default(), sound_and_picture());
    let router = oxrtc_api::router(state);
    let mut streamed = Streamed::of(&router, "/api/stream.mp4?src=cam&mp4=flac").await;
    assert_eq!(streamed.status, StatusCode::OK);
    assert_eq!(streamed.header(header::CONTENT_TYPE), "video/mp4; codecs=\"avc1.64001e,flac\"", "G.711 as FLAC");
    assert_eq!(streamed.header(header::CONTENT_LENGTH), "", "a progressive body has no length: chunked");
    assert_eq!(streamed.header(header::CONTENT_DISPOSITION), "");
    let init = read_init(&streamed.chunk().await.unwrap()).unwrap();
    let entries: Vec<[u8; 4]> = init.tracks.iter().map(|t| t.entry).collect();
    assert_eq!(entries, vec![*b"avc1", *b"fLaC"]);
    let mut samples = Vec::new();
    for _ in 0..30 {
        samples.push(only(&streamed.chunk().await.unwrap(), &init));
    }
    assert_eq!((samples[0].0, samples[0].1, samples[0].3), (1, 0, true), "a keyframe, at zero");
    for track in [1, 2] {
        let times: Vec<u64> = samples.iter().filter(|s| s.0 == track).map(|s| s.1).collect();
        assert!(times.len() > 5 && times.windows(2).all(|w| w[0] < w[1]), "track {track} rises: {times:?}");
    }
    let listed = consumers(&hub, "cam");
    assert_eq!((listed.len(), listed[0].kind.as_str(), listed[0].class.as_str()), (1, "mp4", "live"));
    drop(streamed);
    assert!(eventually(Duration::from_millis(300), || consumers(&hub, "cam").is_empty()).await, "dropped, it ends");
}

/// The PPS a camera sends in band where its description said [`H264_PPS`].
const OTHER_PPS: &str = "68ee3c80";

/// Whether `config` (an `avcC`: High profile has bytes after its PPS)
/// holds `nal`.
fn holds(config: &[u8], nal: &[u8]) -> bool {
    config.windows(nal.len()).any(|w| w == nal)
}

/// H.264 whose parameter sets in band are not those of its description, as
/// many cameras' are: the same level (`pps`), or another one (`level`).
fn described_otherwise(other: &str) -> Script {
    let in_band = match other {
        "pps" => CodecParams::H264 { sps: hex(H264_SPS), pps: hex(OTHER_PPS) },
        _ => CodecParams::H264 { sps: hex(&H264_SPS.replacen("6764001e", "6764001f", 1)), pps: hex(H264_PPS) },
    };
    Script {
        in_band: Some(vec![TrackInfo::video(Codec::H264).with_params(in_band)]),
        ..Script::tracks(vec![h264()], 10_000)
    }
}

#[tokio::test]
async fn a_camera_whose_parameter_sets_in_band_differ_from_its_description_is_written_whole() {
    for (other, mime) in [("pps", "avc1.64001e"), ("level", "avc1.64001f")] {
        let factory = Arc::new(FakeFactory::default());
        factory.add("cam", described_otherwise(other));
        let hub = hub(factory);
        hub.upsert("cam", &["fake:cam"]).unwrap();
        let router = oxrtc_api::router(ApiState::new(hub.clone(), "1.9.15", auth()));
        // The first reader starts the source: its description, then its
        // parameter sets in band ahead of the first keyframe.
        let (init, samples) = whole(Streamed::of(&router, "/api/stream.mp4?src=cam&duration=1").await).await;
        assert_eq!(samples.len(), 25, "{other}: a whole second of pictures, not an init segment alone");
        assert_eq!((samples[0].1, samples[0].3), (0, true), "{other}: from a keyframe");
        let config = &init.tracks[0].config;
        let CodecParams::H264 { sps, pps } = &described_otherwise(other).in_band.unwrap()[0].params else { panic!() };
        assert!(holds(config, sps) && holds(config, pps), "{other}: the init segment has the sets the pictures use");
        let typed = Streamed::of(&router, "/api/stream.mp4?src=cam").await;
        assert_eq!(typed.header(header::CONTENT_TYPE), format!("video/mp4; codecs=\"{mime}\""), "{other}");
    }
}

#[tokio::test]
async fn the_query_chooses_the_codecs_as_go2rtc_does() {
    let (state, _hub) = setup(Mp4Limits::default(), sound_and_picture());
    let router = oxrtc_api::router(state);
    for (query, mime) in [
        ("", "video/mp4; codecs=\"avc1.64001e\""),
        ("&mp4", "video/mp4; codecs=\"avc1.64001e\""),
        ("&mp4=all", "video/mp4; codecs=\"avc1.64001e,flac\""),
        ("&video", "video/mp4; codecs=\"avc1.64001e\""),
        ("&audio", "audio/mp4; codecs=\"flac\""),
        ("&video&audio=pcma", "video/mp4; codecs=\"avc1.64001e,flac\""),
    ] {
        let streamed = Streamed::of(&router, &format!("/api/stream.mp4?src=cam{query}")).await;
        assert_eq!((streamed.status, streamed.header(header::CONTENT_TYPE)), (StatusCode::OK, mime), "{query}");
    }
    let recorder = Streamed::of(&router, "/api/stream.mp4?src=cam&queue=recorder").await;
    assert_eq!(recorder.status, StatusCode::OK, "oxrtc's `queue`, as an RTSP reader names it");
}

#[tokio::test]
async fn duration_ends_the_response_after_that_much_media() {
    let (state, _hub) = setup(Mp4Limits::default(), sound_and_picture());
    let router = oxrtc_api::router(state);
    let began = Instant::now();
    let (_, samples) = whole(Streamed::of(&router, "/api/stream.mp4?src=video&duration=2").await).await;
    // 40 ms of media every 20 ms: two seconds of it are 50 pictures, in 1 s.
    assert_eq!(samples.len(), 50, "{samples:?}");
    assert!(samples.iter().all(|s| s.1 < 2 * 90_000), "nothing at or past 2 s");
    assert!(began.elapsed() < Duration::from_millis(1800), "ended on the media, not the clock: {:?}", began.elapsed());
}

#[tokio::test]
async fn duration_ends_a_slower_stream_on_the_wall_clock() {
    let (state, _hub) = setup(Mp4Limits::default(), sound_and_picture());
    let router = oxrtc_api::router(state);
    let began = Instant::now();
    let (_, samples) = whole(Streamed::of(&router, "/api/stream.mp4?src=slow&duration=1").await).await;
    let took = began.elapsed();
    // 40 ms of media every 100 ms: 1 s of it takes 2.5 s; the response ends
    // a second after its duration, at 2 s, with 0.8 s of media.
    assert!(took >= Duration::from_millis(1950) && took < Duration::from_millis(2600), "{took:?}");
    assert!((16..=22).contains(&samples.len()), "a picture every 100 ms for 2 s: {}", samples.len());
}

#[tokio::test]
async fn filename_gives_content_disposition() {
    let (state, _hub) = setup(Mp4Limits::default(), sound_and_picture());
    let router = oxrtc_api::router(state);
    let streamed = Streamed::of(&router, "/api/stream.mp4?src=cam&filename=record.mp4").await;
    assert_eq!(streamed.header(header::CONTENT_DISPOSITION), "attachment; filename=\"record.mp4\"");
    let streamed = Streamed::of(&router, "/api/stream.mp4?src=cam&filename=a%22%0D%0Ab.mp4").await;
    assert_eq!(streamed.header(header::CONTENT_DISPOSITION), "attachment; filename=\"a_b.mp4\"");
}

#[tokio::test]
async fn what_cannot_be_served_is_refused_with_go2rtcs_words() {
    let (state, _hub) = setup(Mp4Limits::default(), sound_and_picture());
    let router = oxrtc_api::router(state);
    for (uri, status, text) in [
        ("/api/stream.mp4?src=nope", StatusCode::NOT_FOUND, "stream not found"),
        ("/api/stream.mp4", StatusCode::BAD_REQUEST, "src is required"),
        ("/api/stream.mp4?src=cam&duration=5.5", StatusCode::BAD_REQUEST, "invalid duration"),
        ("/api/stream.mp4?src=cam&video=vp9", StatusCode::BAD_REQUEST, "unknown codec"),
        ("/api/stream.mp4?src=cam&queue=fast", StatusCode::BAD_REQUEST, "queue"),
        ("/api/stream.mp4?src=jpeg", StatusCode::INTERNAL_SERVER_ERROR, "codecs not matched"),
        ("/api/stream.mp4?src=cam&video=jpeg", StatusCode::INTERNAL_SERVER_ERROR, "codecs not matched"),
    ] {
        let reply = router.clone().oneshot(get(uri)).await.unwrap();
        assert_eq!(reply.status(), status, "{uri}");
        let body = axum::body::to_bytes(reply.into_body(), 4096).await.unwrap();
        let body = String::from_utf8_lossy(&body);
        assert!(body.contains(text), "{uri}: {body}");
    }
}

#[tokio::test]
async fn one_address_holds_a_bounded_number_of_responses() {
    let (logs, _guard) = Logs::capture();
    let limits = Mp4Limits { max_responses_per_ip: 2, ..Mp4Limits::default() };
    let (state, _hub) = setup(limits, sound_and_picture());
    let router = oxrtc_api::router(state);
    let first = Streamed::of(&router, "/api/stream.mp4?src=cam").await;
    let _second = Streamed::of(&router, "/api/stream.mp4?src=video").await;
    let third = Streamed::of(&router, "/api/stream.mp4?src=cam").await;
    assert_eq!(third.status, StatusCode::SERVICE_UNAVAILABLE);
    assert!(logs.text().contains("api: per-address mp4 response limit reached, refusing"), "{}", logs.text());
    let frame = router.clone().oneshot(get("/api/frame.mp4?src=cam")).await.unwrap();
    assert_eq!(frame.status(), StatusCode::SERVICE_UNAVAILABLE, "frame.mp4 shares the bound");
    drop(first);
    assert_eq!(Streamed::of(&router, "/api/stream.mp4?src=cam").await.status, StatusCode::OK, "a slot freed");
    let total = Mp4Limits { max_responses: 1, ..Mp4Limits::default() };
    let router = oxrtc_api::router(setup(total, sound_and_picture()).0);
    let _held = Streamed::of(&router, "/api/stream.mp4?src=cam").await;
    assert_eq!(Streamed::of(&router, "/api/stream.mp4?src=cam").await.status, StatusCode::SERVICE_UNAVAILABLE);
}

#[tokio::test]
async fn a_silent_source_ends_the_response() {
    let limits = Mp4Limits { silence: Duration::from_millis(300), ..Mp4Limits::default() };
    let (state, _hub) = setup(limits, sound_and_picture());
    let router = oxrtc_api::router(state);
    let began = Instant::now();
    let (_, samples) = whole(Streamed::of(&router, "/api/stream.mp4?src=few").await).await;
    assert_eq!(samples.len(), 15);
    assert!(began.elapsed() < Duration::from_secs(2), "{:?}", began.elapsed());
}

#[tokio::test]
async fn a_slow_reader_skips_to_the_next_keyframe_and_says_so() {
    let (logs, _guard) = Logs::capture();
    // 200 KB pictures every 5 ms, a keyframe every 10: 40 MB/s.
    let script =
        Script { video_bytes: 200_000, interval: Duration::from_millis(5), ..Script::tracks(vec![h264()], 100_000) };
    let (state, hub) = setup(Mp4Limits::default(), script);
    let router = oxrtc_api::router(state);
    let mut streamed = Streamed::of(&router, "/api/stream.mp4?src=cam").await;
    let init = read_init(&streamed.chunk().await.unwrap()).unwrap();
    for _ in 0..5 {
        streamed.chunk().await.unwrap();
    }
    // Not read for longer than a live reader's queue holds (2 s).
    tokio::time::sleep(Duration::from_millis(3500)).await;
    let mut got = Vec::new();
    for _ in 0..400 {
        got.push(only(&streamed.chunk().await.unwrap(), &init));
    }
    assert!(consumers(&hub, "cam")[0].dropped > 0, "the stream's listing counts the loss");
    let resumed: Vec<_> = got.iter().filter(|s| s.2 > 10 * 3600 * 2).collect();
    assert!(!resumed.is_empty() && resumed.iter().all(|s| s.3), "resumed at keyframes: {:?}", &got[..10]);
    assert!(eventually(Duration::from_secs(2), || logs.text().contains("mp4 reader fell behind the stream")).await);
}

/// `GET path` over `stream` with the API's credential: the response head.
async fn head_over(stream: &mut TcpStream, path: &str) -> String {
    let request = format!("GET {path} HTTP/1.1\r\nHost: x\r\nAuthorization: {}\r\n\r\n", basic(USER, PASS));
    stream.write_all(request.as_bytes()).await.unwrap();
    let mut head = Vec::new();
    while !head.ends_with(b"\r\n\r\n") {
        head.push(stream.read_u8().await.unwrap());
    }
    String::from_utf8(head).unwrap()
}

#[tokio::test]
async fn over_tcp_it_is_chunked_and_ends_with_its_client() {
    let (state, hub) = setup(Mp4Limits::default(), sound_and_picture());
    let server = common::ws::serve(state, ServeLimits::default()).await;
    let mut stream = TcpStream::connect(server.addr).await.unwrap();
    let local = stream.local_addr().unwrap();
    let head = head_over(&mut stream, "/api/stream.mp4?src=cam").await.to_ascii_lowercase();
    assert!(head.starts_with("http/1.1 200") && head.contains("transfer-encoding: chunked"), "{head}");
    let mut some = vec![0; 4096];
    stream.read_exact(&mut some).await.unwrap();
    assert_eq!(consumers(&hub, "cam")[0].remote, Some(local.to_string()));
    drop(stream);
    assert!(eventually(Duration::from_millis(300), || consumers(&hub, "cam").is_empty()).await, "its client gone");
}

#[tokio::test]
async fn the_servers_stop_ends_the_body_at_once() {
    let (state, hub) = setup(Mp4Limits::default(), sound_and_picture());
    let server = common::ws::serve(state, ServeLimits::default()).await;
    let mut stream = TcpStream::connect(server.addr).await.unwrap();
    head_over(&mut stream, "/api/stream.mp4?src=cam").await;
    let stopped = Instant::now();
    server.cancel.cancel();
    let mut rest = Vec::new();
    let read = tokio::time::timeout(Duration::from_secs(2), stream.read_to_end(&mut rest)).await;
    assert!(read.is_ok(), "the response ended");
    assert!(rest.ends_with(b"0\r\n\r\n"), "a complete chunked body");
    tokio::time::timeout(Duration::from_secs(2), server.task).await.unwrap().unwrap().unwrap();
    assert!(stopped.elapsed() < oxrtc_api::SHUTDOWN_GRACE, "before the grace: {:?}", stopped.elapsed());
    assert!(eventually(Duration::from_millis(300), || consumers(&hub, "cam").is_empty()).await);
}
