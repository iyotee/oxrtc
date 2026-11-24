//! HLS (F6, D47): `stream.m3u8`'s master playlist and the session it opens,
//! fMP4 (`mp4` in the query) or MPEG-TS (go2rtc's default); the media
//! playlist with its parts, preload hint and blocking reload; init segment,
//! segments and parts read back by the writers' own test readers (segments
//! start at keyframes, media sequence numbers rise); the session's routes
//! read with its id alone; its expiry and the release of its subscription;
//! the per-address bound; `stream.mpd`'s DASH manifest.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing, clippy::panic)]

mod common;

use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use common::*;
use oxrtc_api::{ApiState, HlsLimits};
use oxrtc_core::{Codec, CodecParams, Hub, TrackInfo};
use oxrtc_mp4::fuzzing::{H264_PPS, H264_SPS, hex};
use oxrtc_mp4::reader::{read_fragment, read_init};
use tower::ServiceExt;

fn h264() -> TrackInfo {
    TrackInfo::video(Codec::H264).with_params(CodecParams::H264 { sps: hex(H264_SPS), pps: hex(H264_PPS) })
}

fn aac() -> TrackInfo {
    TrackInfo::audio(Codec::Aac, 48_000, 2)
        .with_params(CodecParams::Aac { config: bytes::Bytes::from_static(&[0x11, 0x90]) })
}

/// `video`: H.264 alone, a picture every 20 ms of 40 ms of media, a keyframe
/// every tenth; `cam`: H.264 and AAC; `few`: 30 pictures, then nothing.
fn setup(limits: HlsLimits) -> (axum::Router, Arc<Hub>) {
    let factory = Arc::new(FakeFactory::default());
    factory.add("video", Script::tracks(vec![h264()], 100_000));
    factory.add("cam", Script::tracks(vec![h264(), aac()], 100_000));
    factory.add("few", Script::tracks(vec![h264()], 30));
    let hub = hub(factory);
    for name in ["video", "cam", "few"] {
        hub.upsert(name, &[&format!("fake:{name}")]).unwrap();
    }
    let state = ApiState::new(hub.clone(), "1.9.15", auth()).with_hls_limits(limits);
    (oxrtc_api::router(state), hub)
}

struct Got {
    status: StatusCode,
    headers: axum::http::HeaderMap,
    body: Vec<u8>,
}

impl Got {
    fn text(&self) -> String {
        String::from_utf8(self.body.clone()).unwrap()
    }
}

async fn get(router: &axum::Router, uri: &str, authenticated: bool) -> Got {
    let mut request = Request::get(uri);
    if authenticated {
        request = request.header(header::AUTHORIZATION, basic(USER, PASS));
    }
    let response = router.clone().oneshot(request.body(Body::empty()).unwrap()).await.unwrap();
    let (parts, body) = response.into_parts();
    let body = axum::body::to_bytes(body, usize::MAX).await.unwrap().to_vec();
    Got { status: parts.status, headers: parts.headers, body }
}

/// Opens a session: its id, from the master playlist.
async fn open(router: &axum::Router, query: &str) -> (String, String) {
    let master = get(router, &format!("/api/stream.m3u8?{query}"), true).await;
    assert_eq!(master.status, StatusCode::OK, "{}", master.text());
    assert_eq!(master.headers[header::CONTENT_TYPE], "application/vnd.apple.mpegurl");
    assert_eq!(master.headers[header::ACCESS_CONTROL_ALLOW_ORIGIN], "*");
    let text = master.text();
    let id = text.lines().last().unwrap().strip_prefix("hls/playlist.m3u8?id=").unwrap().to_owned();
    assert_eq!(id.len(), 22);
    (id, text)
}

/// A tag's value in a playlist.
fn tag<'a>(playlist: &'a str, name: &str) -> Option<&'a str> {
    playlist.lines().find_map(|l| l.strip_prefix(name)?.strip_prefix(':'))
}

fn segments(playlist: &str, ext: &str) -> Vec<u64> {
    playlist
        .lines()
        .filter_map(|l| l.strip_prefix(&format!("segment.{ext}?id="))?.split("&n=").nth(1)?.parse().ok())
        .collect()
}

/// The playlist once it lists at least `count` segments.
async fn playlist_with(router: &axum::Router, id: &str, count: usize, ext: &str) -> String {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let got = get(router, &format!("/api/hls/playlist.m3u8?id={id}"), false).await;
        assert_eq!(got.status, StatusCode::OK, "{}", got.text());
        let text = got.text();
        if segments(&text, ext).len() >= count || Instant::now() > deadline {
            return text;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn an_fmp4_session_s_playlists_init_and_segments() {
    let (router, _hub) = setup(HlsLimits::default());
    let (id, master) = open(&router, "src=cam&mp4").await;
    assert!(master.contains("CODECS=\"avc1.64001e,mp4a.40.2\""), "{master}");
    let first = playlist_with(&router, &id, 3, "m4s").await;
    assert_eq!(tag(&first, "#EXT-X-VERSION"), Some("9"));
    assert!(tag(&first, "#EXT-X-MAP").unwrap().starts_with(&format!("URI=\"init.mp4?id={id}&i=1\"")));
    assert!(tag(&first, "#EXT-X-SERVER-CONTROL").unwrap().starts_with("CAN-BLOCK-RELOAD=YES"));
    assert!(tag(&first, "#EXT-X-PROGRAM-DATE-TIME").unwrap().ends_with('Z'));
    assert!(tag(&first, "#EXT-X-PRELOAD-HINT").unwrap().starts_with("TYPE=PART,URI=\"part.m4s?id="));
    let target: f64 = tag(&first, "#EXT-X-TARGETDURATION").unwrap().parse().unwrap();
    for extinf in first.lines().filter_map(|l| l.strip_prefix("#EXTINF:")) {
        let d: f64 = extinf.trim_end_matches(',').parse().unwrap();
        assert!(d > 0.0 && d.round() <= target, "{d} under a target of {target}");
    }
    let init = get(&router, &format!("/api/hls/init.mp4?id={id}"), false).await;
    assert_eq!(init.status, StatusCode::OK);
    assert_eq!(init.headers[header::CONTENT_TYPE], "video/mp4");
    let parsed = read_init(&init.body).unwrap();
    for n in segments(&first, "m4s") {
        let segment = get(&router, &format!("/api/hls/segment.m4s?id={id}&n={n}"), false).await;
        assert_eq!(segment.status, StatusCode::OK);
        assert_eq!(segment.headers[header::CONTENT_TYPE], "video/iso.segment");
        let fragment = read_fragment(&segment.body, &parsed.trex).unwrap();
        let video = fragment.runs.iter().find(|r| r.track_id == 1).unwrap();
        assert_eq!(video.samples[0].flags & 0x0001_0000, 0, "segment {n} starts at a keyframe");
    }
    // Later: the window moved on, its numbers rising.
    tokio::time::sleep(Duration::from_millis(1500)).await;
    let later = playlist_with(&router, &id, 3, "m4s").await;
    let (a, b): (u64, u64) = (
        tag(&first, "#EXT-X-MEDIA-SEQUENCE").unwrap().parse().unwrap(),
        tag(&later, "#EXT-X-MEDIA-SEQUENCE").unwrap().parse().unwrap(),
    );
    assert!(b >= a);
    assert!(segments(&later, "m4s").last() > segments(&first, "m4s").last());
    let numbers = segments(&later, "m4s");
    assert!(numbers.windows(2).all(|w| w[1] == w[0] + 1), "{numbers:?}");
    assert_eq!(numbers[0], b);
}

#[tokio::test(flavor = "multi_thread")]
async fn an_mpeg_ts_session_s_segments_start_with_the_tables_and_a_keyframe() {
    let (router, _hub) = setup(HlsLimits::default());
    let (id, master) = open(&router, "src=cam").await;
    assert!(master.contains("CODECS=\"avc1.64001e,mp4a.40.2\""), "{master}");
    let text = playlist_with(&router, &id, 2, "ts").await;
    assert_eq!(tag(&text, "#EXT-X-VERSION"), Some("3"));
    assert!(tag(&text, "#EXT-X-MAP").is_none());
    for n in segments(&text, "ts") {
        let segment = get(&router, &format!("/api/hls/segment.ts?id={id}&n={n}"), false).await;
        assert_eq!(segment.headers[header::CONTENT_TYPE], "video/mp2t");
        let read = oxrtc_mpegts::reader::read(&segment.body).unwrap();
        assert_eq!(&segment.body[..4], &[0x47, 0x40, 0x00, segment.body[3]], "the PAT first");
        assert_eq!(read.streams.iter().map(|s| s.stream_type).collect::<Vec<_>>(), [0x1B, 0x0F]);
        let first_video = read.pes.iter().find(|p| p.pid == 0x100).unwrap();
        assert!(first_video.random_access, "segment {n} starts at a keyframe");
        assert!(read.pes.iter().any(|p| p.pid == 0x101), "the sound is there");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_blocking_reload_answers_once_the_part_exists() {
    let (router, _hub) = setup(HlsLimits::default());
    let (id, _) = open(&router, "src=video&mp4").await;
    let text = playlist_with(&router, &id, 1, "m4s").await;
    let hint = tag(&text, "#EXT-X-PRELOAD-HINT").unwrap();
    let after = hint.split("&n=").nth(1).unwrap().trim_end_matches('"');
    let (n, p) = after.split_once("&p=").unwrap();
    let started = Instant::now();
    let uri = format!("/api/hls/playlist.m3u8?id={id}&_HLS_msn={n}&_HLS_part={p}");
    let blocked = get(&router, &uri, false).await;
    assert_eq!(blocked.status, StatusCode::OK);
    assert!(blocked.text().contains(&format!("URI=\"part.m4s?id={id}&n={n}&p={p}\"")), "the part asked for is listed");
    assert!(started.elapsed() < Duration::from_secs(3));
    // The hinted part, asked before it exists, is answered once it does.
    let next = tag(&blocked.text(), "#EXT-X-PRELOAD-HINT").unwrap().to_owned();
    let uri = next.trim_start_matches("TYPE=PART,URI=\"").trim_end_matches('"');
    let part = get(&router, &format!("/api/hls/{uri}"), false).await;
    assert_eq!(part.status, StatusCode::OK);
    assert!(!part.body.is_empty());
    // Too far ahead: refused at once.
    let far: u64 = n.parse::<u64>().unwrap() + 10;
    let refused = get(&router, &format!("/api/hls/playlist.m3u8?id={id}&_HLS_msn={far}"), false).await;
    assert_eq!(refused.status, StatusCode::BAD_REQUEST);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_blocking_reload_times_out_when_the_stream_stops() {
    let limits =
        HlsLimits { max_wait: Duration::from_millis(600), idle: Duration::from_secs(30), ..HlsLimits::default() };
    let (router, _hub) = setup(limits);
    let (id, _) = open(&router, "src=few&mp4").await;
    tokio::time::sleep(Duration::from_millis(1000)).await;
    let text = get(&router, &format!("/api/hls/playlist.m3u8?id={id}"), false).await.text();
    let (n, _) = text
        .lines()
        .rev()
        .find_map(|l| l.strip_prefix("#EXT-X-PRELOAD-HINT:"))
        .unwrap()
        .split_once("&n=")
        .unwrap()
        .1
        .split_once("&p=")
        .unwrap();
    let next: u64 = n.parse::<u64>().unwrap() + 1;
    let started = Instant::now();
    let got = get(&router, &format!("/api/hls/playlist.m3u8?id={id}&_HLS_msn={next}"), false).await;
    assert_eq!(got.status, StatusCode::SERVICE_UNAVAILABLE, "{}", got.text());
    assert!(started.elapsed() >= Duration::from_millis(550) && started.elapsed() < Duration::from_secs(3));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_session_ends_when_no_request_comes_and_releases_its_subscription() {
    let limits = HlsLimits { idle: Duration::from_millis(800), ..HlsLimits::default() };
    let (router, hub) = setup(limits);
    let (id, _) = open(&router, "src=video&mp4").await;
    let consumers = || hub.get("video").unwrap().snapshot().consumers.len();
    assert_eq!(consumers(), 1);
    // Requests keep it.
    for _ in 0..6 {
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(get(&router, &format!("/api/hls/playlist.m3u8?id={id}"), false).await.status, StatusCode::OK);
    }
    // None: it ends (its idle time is three of its longest segments, 3.6 s here).
    assert!(eventually(Duration::from_secs(8), || consumers() == 0).await, "the subscription is released");
    assert_eq!(get(&router, &format!("/api/hls/playlist.m3u8?id={id}"), false).await.status, StatusCode::NOT_FOUND);
}

#[tokio::test(flavor = "multi_thread")]
async fn the_session_s_routes_need_its_id_and_stream_m3u8_its_credential() {
    let (router, _hub) = setup(HlsLimits::default());
    assert_eq!(get(&router, "/api/stream.m3u8?src=video", false).await.status, StatusCode::UNAUTHORIZED);
    assert_eq!(get(&router, "/api/stream.m3u8?src=nothing", true).await.status, StatusCode::NOT_FOUND);
    for path in ["playlist.m3u8", "init.mp4", "segment.m4s", "segment.ts", "part.m4s", "manifest.mpd"] {
        let got = get(&router, &format!("/api/hls/{path}?id=0123456789abcdefghijkl&n=0&p=0"), false).await;
        assert_eq!(got.status, StatusCode::NOT_FOUND, "{path}");
        assert_eq!(get(&router, &format!("/api/hls/{path}"), true).await.status, StatusCode::NOT_FOUND, "{path}");
    }
    let (id, _) = open(&router, "src=video").await;
    assert_eq!(get(&router, &format!("/api/hls/playlist.m3u8?id={id}"), false).await.status, StatusCode::OK);
    assert_eq!(
        get(&router, &format!("/api/hls/playlist.m3u8?id={id}&_HLS_part=1"), false).await.status,
        StatusCode::BAD_REQUEST
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn sessions_are_bounded_per_address() {
    let (router, _hub) = setup(HlsLimits { max_sessions_per_ip: 2, ..HlsLimits::default() });
    open(&router, "src=video").await;
    open(&router, "src=video").await;
    let third = get(&router, "/api/stream.m3u8?src=video", true).await;
    assert_eq!(third.status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(third.text(), "too many hls sessions");
}

#[tokio::test(flavor = "multi_thread")]
async fn stream_mpd_redirects_to_its_session_s_dash_manifest() {
    let (router, _hub) = setup(HlsLimits::default());
    let started = Instant::now();
    let got = get(&router, "/api/stream.mpd?src=cam", true).await;
    assert!(started.elapsed() < Duration::from_secs(5), "{:?}", started.elapsed());
    assert_eq!(got.status, StatusCode::FOUND);
    let location = got.headers[header::LOCATION].to_str().unwrap().to_owned();
    let id = location.strip_prefix("hls/manifest.mpd?id=").unwrap();
    let mpd = get(&router, &format!("/api/{location}"), false).await;
    assert_eq!(mpd.status, StatusCode::OK, "{location}: {}", mpd.text());
    assert_eq!(mpd.headers[header::CONTENT_TYPE], "application/dash+xml");
    let text = mpd.text();
    assert!(text.contains("type=\"dynamic\"") && text.contains("<S t="), "{text}");
    assert!(text.contains(&format!("media=\"segment.m4s?id={id}&amp;n=$Number$\"")));
    assert!(text.contains("codecs=\"avc1.64001e,mp4a.40.2\""));
}
