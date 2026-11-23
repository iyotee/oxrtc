//! An offer whose video lists no codec this server sends (VP8 alone, as an
//! old browser or a VP8-only client writes it), D43: refused when the stream
//! has a picture, with a reason that names the stream's codecs and the
//! offer's (go2rtc's words); answered with the sound when the stream has no
//! picture, its video line answered on the offer's own codec so that the
//! client accepts the answer (aiortc refused one whose video line was
//! rejected). Without a browser: a str0m client offers.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing, clippy::panic)]

mod common;

use std::sync::Arc;
use std::time::{Duration, Instant};

use common::*;
use oxrtc_core::{Codec, Stream, TrackInfo};
use oxrtc_webrtc::{WebRtcServer, WhepError};
use str0m::RtcConfig;
use str0m::format::Codec as RtcCodec;
use str0m::media::{Direction, MediaKind as RtcKind};

const WAIT: Duration = Duration::from_secs(10);

fn vp8_and_opus() -> RtcConfig {
    RtcConfig::new().clear_codecs().enable_vp8(true).enable_opus(true, false)
}

/// A camera's H.264 and Opus.
fn camera() -> Arc<Stream> {
    let (params, video) = synthetic_h264();
    hub(FakeFactory::default().with("cam", av_script(Codec::H264, params, video, false)))
        .upsert("cam", &["fake:cam"])
        .unwrap()
}

/// A microphone: Opus and no picture.
fn microphone() -> Arc<Stream> {
    let script = Script {
        declared: None,
        tracks: Some(vec![TrackInfo::audio(Codec::Opus, 48_000, 2)]),
        frames: opus_frames(0, 400),
        interval: Duration::from_millis(20),
    };
    hub(FakeFactory::default().with("mic", script)).upsert("mic", &["fake:mic"]).unwrap()
}

/// A str0m client's offer of video and audio with `config`.
fn offer(config: RtcConfig) -> String {
    let mut rtc = config.build(Instant::now());
    let mut api = rtc.sdp_api();
    api.add_media(RtcKind::Video, Direction::RecvOnly, None, None, None);
    api.add_media(RtcKind::Audio, Direction::RecvOnly, None, None, None);
    api.apply().unwrap().0.to_sdp_string()
}

async fn answer(server: &WebRtcServer, stream: Arc<Stream>, config: RtcConfig) -> Result<String, WhepError> {
    server.answer(stream, &offer(config), None).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_vp8_offer_to_a_stream_with_a_picture_is_refused_and_named() {
    let stream = camera();
    let server = server(4, Duration::from_secs(10), WAIT).await;
    let refused = answer(&server, stream.clone(), vp8_and_opus()).await;
    let Err(WhepError::NoMatchingCodec(reason)) = refused else { panic!("answered: {refused:?}") };
    assert_eq!(reason, "streams: codecs not matched: video:H264, audio:OPUS => video:VP8, audio:OPUS");
    assert_eq!(server.session_count(), 0);
    assert!(eventually(WAIT, || stream.snapshot().consumers.is_empty()).await, "the refusal holds no consumer");
    // VP8 with no audio line: refused at once, the stream's side what its
    // source showed (it ran for the offer before).
    let Err(WhepError::NoMatchingCodec(reason)) =
        answer(&server, stream, RtcConfig::new().clear_codecs().enable_vp8(true)).await
    else {
        panic!("answered")
    };
    assert!(reason.ends_with("=> video:VP8"), "{reason}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_vp8_offer_to_a_stream_without_a_picture_gets_the_sound() {
    let stream = microphone();
    let server = server(4, Duration::from_secs(10), WAIT).await;
    let sdp = answer(&server, stream.clone(), vp8_and_opus()).await.unwrap();
    let video = sdp.split("m=video").nth(1).unwrap().split("m=audio").next().unwrap();
    assert!(!video.starts_with(" 0 "), "the video line is answered, not rejected: {video}");
    assert!(video.contains("VP8/90000"), "on the offer's own codec: {video}");
    // A client that plays it hears the sound.
    let mut client = connect(&server, stream, vp8_and_opus()).await.unwrap();
    assert_eq!(client.wait_for(WAIT, |e| *e == ClientEvent::Connected).await, Some(ClientEvent::Connected));
    let heard = client.wait_for(WAIT, |e| matches!(e, ClientEvent::Media { kind: RtcKind::Audio, .. })).await;
    assert!(matches!(heard, Some(ClientEvent::Media { codec: RtcCodec::Opus, .. })), "{heard:?}");
    let seen =
        client.wait_for(Duration::from_millis(500), |e| matches!(e, ClientEvent::Media { kind: RtcKind::Video, .. }));
    assert_eq!(seen.await, None, "nothing is sent on the video line");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_h264_offer_is_answered_as_before() {
    let server = server(4, Duration::from_secs(10), WAIT).await;
    let sdp = answer(&server, camera(), browser(false)).await.unwrap();
    assert!(sdp.contains("H264/90000") && sdp.contains("opus/48000/2"), "{sdp}");
    assert!(!sdp.contains("VP8"), "{sdp}");
    // To the microphone, an H.264 offer's video line is answered on H.264.
    let sdp = answer(&server, microphone(), browser(false)).await.unwrap();
    assert!(sdp.contains("H264/90000") && !sdp.contains("VP8"), "{sdp}");
}
