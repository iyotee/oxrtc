//! H.265 to the browsers that offer it (W7), without a browser: a str0m
//! client offers H.265 as Chrome 157 and Edge 157 write it (Main and Main 10,
//! Main tier, level 6.0) beside H.264, over openIVR's layout (the camera's
//! own picture first, its H.264 transcode second). The camera's H.265 is
//! sent when the offer takes its profile, tier and level, and the transcode
//! never starts; otherwise the transcode is sent.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing, clippy::panic)]

mod common;

use std::sync::Arc;
use std::time::{Duration, Instant};

use bytes::Bytes;
use common::*;
use oxrtc_core::{Codec, CodecParams, Stream};
use oxrtc_webrtc::{OfferedCodecs, WebRtcServer};
use str0m::RtcConfig;
use str0m::format::Codec as RtcCodec;
use str0m::media::{Direction, MediaKind as RtcKind, Pt};

const WAIT: Duration = Duration::from_secs(10);

/// libx265's (FFmpeg 8.0.1) parameter sets of a 1920x1080 Main stream at
/// level 4 (`hvc1.1.6.L120.90`).
const VPS: &str = "40010c01ffff016000000300900000030000030078959809";
const MAIN_L4: &str = "420101016000000300900000030000030078a003c08010e596566924caf0168080000003008000000c84";
const PPS: &str = "4401c172b46240";
/// libx265's Main 10, 640x360, level 2.1.
const MAIN10_L21: &str = "42010102200000030090000003000003003fa005020169365959a4932bc05a020000030002000003003210";

fn hex(s: &str) -> Bytes {
    Bytes::from((0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap()).collect::<Vec<u8>>())
}

/// A camera's H.265 parameter sets with `sps`.
fn camera(sps: &str) -> CodecParams {
    CodecParams::H265 { vps: hex(VPS), sps: hex(sps), pps: hex(PPS) }
}

/// `MAIN_L4` at level 6.2 (`general_level_idc` 186): above what any browser
/// of this PC offers.
fn main_l62() -> String {
    MAIN_L4.replace("030078a0", "0300baa0")
}

/// `MAIN_L4` in the High tier.
fn main_high_tier() -> String {
    MAIN_L4.replacen("42010101", "42010121", 1)
}

/// The H.265 payload types Chrome 157 and Edge 157 offer on this PC (an RTX
/// 4080): `level-id=180;profile-id=1;tier-flag=0` and the same with
/// `profile-id=2`; with `main10` false, Main alone.
fn chrome(main10: bool) -> RtcConfig {
    let mut config = RtcConfig::new().clear_codecs().enable_h264(true).enable_opus(true, false);
    config.codec_config().add_h265(Pt::new_with_value(49), Some(Pt::new_with_value(50)), 1, 0, 180);
    if main10 {
        config.codec_config().add_h265(Pt::new_with_value(51), Some(Pt::new_with_value(52)), 2, 0, 180);
    }
    config
}

/// openIVR's `camera_N`: the camera, then its H.264 transcode.
fn layout(camera_sps: &str) -> Arc<Stream> {
    let (_, h265) = synthetic_h265();
    let (h264_params, h264) = synthetic_h264();
    let factory = FakeFactory::default()
        .with("camera", av_script(Codec::H265, camera(camera_sps), h265, true))
        .with("transcode", av_script(Codec::H264, h264_params, h264, true));
    hub(factory).upsert("camera_1", &["fake:camera", "fake:transcode"]).unwrap()
}

/// Frames the transcode ever published and who holds it now.
fn transcode_use(stream: &Stream) -> (u64, usize) {
    let snapshot = stream.snapshot();
    let source = snapshot.sources.iter().find(|s| s.url.contains("transcode")).unwrap();
    (source.frames, source.demand)
}

/// A str0m client's offer, and the server's answer to it.
async fn answer(server: &WebRtcServer, stream: Arc<Stream>, config: RtcConfig) -> (String, String) {
    let mut rtc = config.build(Instant::now());
    let mut api = rtc.sdp_api();
    api.add_media(RtcKind::Video, Direction::RecvOnly, None, None, None);
    api.add_media(RtcKind::Audio, Direction::RecvOnly, None, None, None);
    let (offer, _pending) = api.apply().unwrap();
    let offer = offer.to_sdp_string();
    let answer = server.answer(stream, &offer, None).await.unwrap();
    (offer, answer)
}

/// The answer's video `rtpmap` and `fmtp` lines.
fn video_lines(answer: &str) -> Vec<&str> {
    let video = answer.split("m=video").nth(1).unwrap().split("m=audio").next().unwrap();
    video.split("\r\n").filter(|l| l.starts_with("a=rtpmap:") || l.starts_with("a=fmtp:")).collect()
}

#[test]
fn chrome_s_offer_is_read_as_main_and_main10_at_level_6() {
    let mut rtc = chrome(true).build(Instant::now());
    let mut api = rtc.sdp_api();
    api.add_media(RtcKind::Video, Direction::RecvOnly, None, None, None);
    let offer = api.apply().unwrap().0.to_sdp_string();
    let codecs = OfferedCodecs::from_sdp(&offer).unwrap();
    let read: Vec<(Option<u8>, u8, Option<u8>)> = codecs.h265.iter().map(|p| (p.profile, p.tier, p.level)).collect();
    assert_eq!(read, vec![(Some(1), 0, Some(180)), (Some(2), 0, Some(180))]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_offer_of_h265_and_h264_gets_the_camera_s_h265_and_starts_no_transcode() {
    let stream = layout(MAIN_L4);
    let server = server(4, Duration::from_secs(10), WAIT).await;
    let mut viewer = connect(&server, stream.clone(), chrome(true)).await.unwrap();
    assert_video_codec(&mut viewer, RtcCodec::H265).await;
    assert_eq!(transcode_use(&stream), (0, 0), "the transcode never started");
    // The answer names the camera's own level, the offer's Main payload type.
    let (_, sdp) = answer(&server, stream.clone(), chrome(true)).await;
    let lines = video_lines(&sdp);
    assert!(lines.contains(&"a=rtpmap:49 H265/90000"), "{lines:?}");
    assert!(lines.contains(&"a=fmtp:49 profile-id=1;tier-flag=0;level-id=120"), "{lines:?}");
    assert!(!lines.iter().any(|l| l.contains("H264")), "{lines:?}");
    assert_eq!(transcode_use(&stream), (0, 0));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_offer_of_h264_alone_gets_the_transcode() {
    let stream = layout(MAIN_L4);
    let server = server(4, Duration::from_secs(10), WAIT).await;
    let mut viewer = connect(&server, stream.clone(), browser(false)).await.unwrap();
    assert_video_codec(&mut viewer, RtcCodec::H264).await;
    assert!(transcode_use(&stream).0 > 0, "the transcode runs");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_camera_above_the_offer_s_level_gets_the_transcode() {
    let stream = layout(&main_l62());
    let server = server(4, Duration::from_secs(10), WAIT).await;
    let mut viewer = connect(&server, stream.clone(), chrome(true)).await.unwrap();
    assert_video_codec(&mut viewer, RtcCodec::H264).await;
    let (frames, demand) = transcode_use(&stream);
    assert!(frames > 0 && demand == 1, "the transcode runs for the viewer: {frames} frames, demand {demand}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_camera_of_the_high_tier_gets_the_transcode() {
    let stream = layout(&main_high_tier());
    let server = server(4, Duration::from_secs(10), WAIT).await;
    let mut viewer = connect(&server, stream.clone(), chrome(true)).await.unwrap();
    assert_video_codec(&mut viewer, RtcCodec::H264).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_main10_camera_is_answered_main10_and_not_sent_to_a_main_only_offer() {
    let stream = layout(MAIN10_L21);
    let server = server(4, Duration::from_secs(10), WAIT).await;
    // Main and Main 10 offered: the Main 10 payload type is the one answered.
    let (_, sdp) = answer(&server, stream.clone(), chrome(true)).await;
    let lines = video_lines(&sdp);
    assert!(lines.contains(&"a=rtpmap:51 H265/90000"), "{lines:?}");
    assert!(lines.contains(&"a=fmtp:51 profile-id=2;tier-flag=0;level-id=63"), "{lines:?}");
    assert!(!lines.iter().any(|l| l.starts_with("a=rtpmap:49 ")), "not answered as Main: {lines:?}");
    let mut ten = connect(&server, stream.clone(), chrome(true)).await.unwrap();
    assert_video_codec(&mut ten, RtcCodec::H265).await;
    // Main alone: the transcode.
    let mut main = connect(&server, stream.clone(), chrome(false)).await.unwrap();
    assert_video_codec(&mut main, RtcCodec::H264).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn with_no_other_source_the_camera_s_h265_is_sent_whatever_its_level() {
    let (_, h265) = synthetic_h265();
    let factory = FakeFactory::default().with("camera", av_script(Codec::H265, camera(&main_l62()), h265, true));
    let stream = hub(factory).upsert("camera_1", &["fake:camera"]).unwrap();
    let server = server(4, Duration::from_secs(10), WAIT).await;
    let mut viewer = connect(&server, stream, chrome(true)).await.unwrap();
    assert_video_codec(&mut viewer, RtcCodec::H265).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn with_no_other_source_a_main10_camera_still_reaches_a_main_only_offer() {
    // As before W7: with nothing else to send, the camera's picture goes on
    // the offer's H.265 payload type, whatever its profile.
    let (_, h265) = synthetic_h265();
    let factory = FakeFactory::default().with("camera", av_script(Codec::H265, camera(MAIN10_L21), h265, true));
    let stream = hub(factory).upsert("camera_1", &["fake:camera"]).unwrap();
    let server = server(4, Duration::from_secs(10), WAIT).await;
    let (_, sdp) = answer(&server, stream.clone(), chrome(false)).await;
    let lines = video_lines(&sdp);
    assert!(lines.contains(&"a=rtpmap:49 H265/90000"), "the answer sends H.265: {lines:?}");
    let mut viewer = connect(&server, stream, chrome(false)).await.unwrap();
    assert_video_codec(&mut viewer, RtcCodec::H265).await;
}

/// The viewer connects and its first video frames are of `codec`.
async fn assert_video_codec(client: &mut Client, codec: RtcCodec) {
    assert_eq!(client.wait_for(WAIT, |e| *e == ClientEvent::Connected).await, Some(ClientEvent::Connected));
    for _ in 0..5 {
        let got = client.wait_for(WAIT, |e| matches!(e, ClientEvent::Media { kind: RtcKind::Video, .. })).await;
        let Some(ClientEvent::Media { codec: got, .. }) = got else { panic!("no video arrived") };
        assert_eq!(got, codec);
    }
}
