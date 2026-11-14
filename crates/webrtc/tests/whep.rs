//! WHEP end to end, without a browser: a str0m client offers, the server
//! answers, and media flows over real UDP on 127.0.0.1.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing, clippy::panic)]

mod common;

use std::time::{Duration, Instant};

use common::*;
use oxrtc_core::{Codec, TrackInfo};
use oxrtc_webrtc::WhepError;
use str0m::format::Codec as RtcCodec;
use str0m::media::MediaKind as RtcKind;

const WAIT: Duration = Duration::from_secs(10);

async fn h264_clip() -> (oxrtc_core::CodecParams, Vec<ScriptFrame>) {
    match ffmpeg_h264().await {
        Some(clip) => clip,
        None => synthetic_h264(),
    }
}

/// Asserts the client connects and receives `codec` video starting with a
/// keyframe, plus Opus audio.
async fn assert_plays(client: &mut Client, codec: RtcCodec) {
    assert_eq!(client.wait_for(WAIT, |e| *e == ClientEvent::Connected).await, Some(ClientEvent::Connected));
    let first = client.wait_for(WAIT, |e| matches!(e, ClientEvent::Media { kind: RtcKind::Video, .. })).await;
    let Some(ClientEvent::Media { codec: got, keyframe, len, params, .. }) = first else { panic!("no video arrived") };
    assert_eq!(got, codec);
    assert!(keyframe, "the first video frame is a keyframe");
    assert!(params, "the first keyframe carries the parameter sets the source announced out of band");
    assert!(len > 0);
    let mut video = 1;
    let mut audio = 0;
    let deadline = tokio::time::Instant::now() + WAIT;
    while (video < 10 || audio < 5) && tokio::time::Instant::now() < deadline {
        match client.wait_for(Duration::from_secs(1), |e| matches!(e, ClientEvent::Media { .. })).await {
            Some(ClientEvent::Media { kind: RtcKind::Video, codec: c, .. }) => {
                assert_eq!(c, codec);
                video += 1;
            }
            Some(ClientEvent::Media { kind: RtcKind::Audio, codec: c, .. }) => {
                assert_eq!(c, RtcCodec::Opus);
                audio += 1;
            }
            _ => {}
        }
    }
    assert!(video >= 10 && audio >= 5, "video {video}, audio {audio}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn h264_offer_plays_h264_with_opus() {
    let (params, video) = h264_clip().await;
    let hub = hub(FakeFactory::default().with("cam", av_script(Codec::H264, params, video, false)));
    let stream = hub.upsert("cam", &["fake:cam"]).unwrap();
    let server = server(4, Duration::from_secs(10), WAIT).await;
    let mut client = connect(&server, stream.clone(), browser(false)).await.unwrap();
    assert_eq!(server.session_count(), 1);
    assert_plays(&mut client, RtcCodec::H264).await;
    let consumers = stream.snapshot().consumers;
    assert_eq!(consumers.len(), 1);
    assert_eq!(consumers[0].kind, "webrtc");
    assert_eq!(consumers[0].remote.as_deref(), Some(client.local.to_string().as_str()));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_offer_chooses_the_source() {
    let (h265_params, h265) = synthetic_h265();
    let (h264_params, h264) = synthetic_h264();
    let factory = FakeFactory::default()
        .with("camera", av_script(Codec::H265, h265_params, h265, true))
        .with("transcode", av_script(Codec::H264, h264_params, h264, true));
    let hub = hub(factory);
    let stream = hub.upsert("cam", &["fake:camera", "fake:transcode"]).unwrap();
    let server = server(4, Duration::from_secs(10), WAIT).await;

    let mut hevc = connect(&server, stream.clone(), browser(true)).await.unwrap();
    assert_plays(&mut hevc, RtcCodec::H265).await;
    let mut avc = connect(&server, stream.clone(), browser(false)).await.unwrap();
    assert_plays(&mut avc, RtcCodec::H264).await;
    assert_eq!(server.session_count(), 2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn max_sessions_is_enforced() {
    let (params, video) = synthetic_h264();
    let hub = hub(FakeFactory::default().with("cam", av_script(Codec::H264, params, video, true)));
    let stream = hub.upsert("cam", &["fake:cam"]).unwrap();
    let server = server(1, Duration::from_secs(10), WAIT).await;
    let mut first = connect(&server, stream.clone(), browser(false)).await.unwrap();
    let second = connect(&server, stream.clone(), browser(false)).await;
    assert!(matches!(second, Err(WhepError::TooManySessions)), "{:?}", second.err());
    // Once the first leaves, a place is free again.
    first.stop(Control::Close);
    assert!(eventually(WAIT, || server.session_count() == 0).await);
    let third = connect(&server, stream, browser(false)).await;
    assert!(third.is_ok(), "{:?}", third.err());
}

/// R9: one caller address cannot take every session - loopback included -
/// and another address still gets one, which plays.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn max_sessions_per_ip_is_enforced() {
    let (params, video) = synthetic_h264();
    let hub = hub(FakeFactory::default().with("cam", av_script(Codec::H264, params, video, true)));
    let stream = hub.upsert("cam", &["fake:cam"]).unwrap();
    let config = oxrtc_webrtc::WebRtcConfig {
        listen: "127.0.0.1:0".parse().unwrap(),
        max_sessions: 8,
        max_sessions_per_ip: 1,
        answer_timeout: WAIT,
        ..oxrtc_webrtc::WebRtcConfig::default()
    };
    let server = oxrtc_webrtc::WebRtcServer::bind(config, oxrtc_core::CancellationToken::new()).await.unwrap();
    let mut first = connect(&server, stream.clone(), browser(false)).await.unwrap();
    let second = connect(&server, stream.clone(), browser(false)).await;
    assert!(matches!(second, Err(WhepError::TooManySessions)), "{:?}", second.err());
    match tokio::net::UdpSocket::bind("127.0.0.2:0").await {
        Ok(socket) => {
            let mut other = connect_on(&server, stream.clone(), browser(false), socket).await.unwrap();
            assert_plays(&mut other, RtcCodec::H264).await;
            assert_eq!(server.session_count(), 2);
        }
        Err(error) => eprintln!("127.0.0.2 cannot be bound here ({error}): one address only"),
    }
    // Once the first leaves, its address has a place again.
    first.stop(Control::Close);
    assert!(eventually(WAIT, || server.session_count() <= 1).await);
    let again = connect(&server, stream, browser(false)).await;
    assert!(again.is_ok(), "{:?}", again.err());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_streams_consumer_cap_is_too_many_sessions() {
    let (params, video) = synthetic_h264();
    let limits = oxrtc_core::Limits { max_consumers_per_stream: 1, ..oxrtc_core::Limits::default() };
    let hub = hub_with(FakeFactory::default().with("cam", av_script(Codec::H264, params, video, true)), limits);
    let stream = hub.upsert("cam", &["fake:cam"]).unwrap();
    let server = server(4, Duration::from_secs(10), WAIT).await;
    let _first = connect(&server, stream.clone(), browser(false)).await.unwrap();
    let second = connect(&server, stream, browser(false)).await;
    assert!(matches!(second, Err(WhepError::TooManySessions)), "{:?}", second.err());
    assert_eq!(server.session_count(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_silent_source_is_unavailable_after_the_timeout() {
    let script = Script { declared: None, tracks: None, frames: Vec::new(), interval: Duration::from_millis(10) };
    let hub = hub(FakeFactory::default().with("dead", script));
    let stream = hub.upsert("dead", &["fake:dead"]).unwrap();
    let server = server(4, Duration::from_secs(10), Duration::from_millis(600)).await;
    let started = Instant::now();
    let result = connect(&server, stream.clone(), browser(false)).await;
    // The answer timeout is what ended it (not an early refusal), and no later.
    assert!(
        matches!(&result, Err(WhepError::SourceUnavailable(reason)) if reason.starts_with("no tracks within")),
        "{:?}",
        result.err()
    );
    assert!(started.elapsed() >= Duration::from_millis(550), "{:?}", started.elapsed());
    assert!(started.elapsed() < Duration::from_secs(3));
    assert_eq!(server.session_count(), 0);
    assert!(eventually(WAIT, || stream.snapshot().consumers.is_empty()).await);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_codec_the_stream_lacks_is_refused() {
    let (params, video) = synthetic_h265();
    let hub = hub(FakeFactory::default().with("cam", av_script(Codec::H265, params, video, true)));
    let stream = hub.upsert("cam", &["fake:cam"]).unwrap();
    // Declared H.265 and an offer of H.264 only: no audio wanted either.
    let server = server(4, Duration::from_secs(10), WAIT).await;
    let config = str0m::RtcConfig::new().clear_codecs().enable_h264(true);
    let result = connect(&server, stream.clone(), config).await;
    assert!(matches!(result, Err(WhepError::NoMatchingCodec)), "{:?}", result.err());
    let vp8_only = str0m::RtcConfig::new().clear_codecs().enable_vp8(true);
    let result = connect(&server, stream, vp8_only).await;
    assert!(matches!(result, Err(WhepError::NoMatchingCodec)), "{:?}", result.err());
    assert_eq!(server.session_count(), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn closing_the_client_ends_the_session_and_releases_the_stream() {
    let (params, video) = synthetic_h264();
    let hub = hub(FakeFactory::default().with("cam", av_script(Codec::H264, params, video, false)));
    let stream = hub.upsert("cam", &["fake:cam"]).unwrap();
    let server = server(4, Duration::from_secs(30), WAIT).await;
    let mut client = connect(&server, stream.clone(), browser(false)).await.unwrap();
    assert_plays(&mut client, RtcCodec::H264).await;
    client.stop(Control::Close);
    assert!(eventually(Duration::from_secs(5), || server.session_count() == 0).await, "session still running");
    assert!(stream.snapshot().consumers.is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_vanished_client_ends_at_the_idle_timeout() {
    let (params, video) = synthetic_h264();
    let script = av_script(Codec::H264, params, video, false);
    let hub = hub(FakeFactory::default().with("cam", script));
    let stream = hub.upsert("cam", &["fake:cam"]).unwrap();
    let server = server(4, Duration::from_millis(1500), WAIT).await;
    let mut client = connect(&server, stream.clone(), browser(false)).await.unwrap();
    assert_eq!(client.wait_for(WAIT, |e| *e == ClientEvent::Connected).await, Some(ClientEvent::Connected));
    client.stop(Control::Vanish);
    assert!(eventually(Duration::from_secs(8), || server.session_count() == 0).await, "session still running");
    assert!(stream.snapshot().consumers.is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn audio_only_stream_answers_an_av_offer() {
    let script = Script {
        declared: None,
        tracks: Some(vec![TrackInfo::audio(Codec::Opus, 48_000, 2)]),
        frames: opus_frames(0, 50),
        interval: Duration::from_millis(20),
    };
    let hub = hub(FakeFactory::default().with("mic", script));
    let stream = hub.upsert("mic", &["fake:mic"]).unwrap();
    let server = server(4, Duration::from_secs(10), WAIT).await;
    let mut client = connect(&server, stream, browser(false)).await.unwrap();
    assert_eq!(client.wait_for(WAIT, |e| *e == ClientEvent::Connected).await, Some(ClientEvent::Connected));
    let audio = client.wait_for(WAIT, |e| matches!(e, ClientEvent::Media { kind: RtcKind::Audio, .. })).await;
    assert!(matches!(audio, Some(ClientEvent::Media { codec: RtcCodec::Opus, .. })), "{audio:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn removing_the_stream_ends_the_session() {
    let (params, video) = synthetic_h264();
    let hub = hub(FakeFactory::default().with("cam", av_script(Codec::H264, params, video, false)));
    let stream = hub.upsert("cam", &["fake:cam"]).unwrap();
    let server = server(4, Duration::from_secs(30), WAIT).await;
    let mut client = connect(&server, stream.clone(), browser(false)).await.unwrap();
    assert_eq!(client.wait_for(WAIT, |e| *e == ClientEvent::Connected).await, Some(ClientEvent::Connected));
    assert!(hub.remove("cam"));
    // The subscription ends (SubEnd): the session follows, well before its idle timeout.
    assert!(eventually(Duration::from_secs(5), || server.session_count() == 0).await, "session still running");
    assert!(stream.snapshot().consumers.is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn oversized_junk_datagrams_do_not_stall_the_socket() {
    let (params, video) = synthetic_h264();
    let hub = hub(FakeFactory::default().with("cam", av_script(Codec::H264, params, video, false)));
    let stream = hub.upsert("cam", &["fake:cam"]).unwrap();
    let server = server(4, Duration::from_secs(30), WAIT).await;
    // Datagrams larger than any WebRTC packet, from a stranger, all the time a
    // real client connects: none of them may slow the socket down.
    let junk = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let target = server.local_addr();
    let flooding = oxrtc_core::CancellationToken::new();
    let flood = tokio::spawn({
        let flooding = flooding.clone();
        async move {
            let big = vec![0x17u8; 9000];
            while !flooding.is_cancelled() {
                for _ in 0..20 {
                    let _ = junk.send_to(&big, target).await;
                }
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        }
    });
    let started = Instant::now();
    let mut client = connect(&server, stream.clone(), browser(false)).await.unwrap();
    let connected = client.wait_for(WAIT, |e| *e == ClientEvent::Connected).await;
    let took = started.elapsed();
    flooding.cancel();
    flood.await.unwrap();
    assert_eq!(connected, Some(ClientEvent::Connected));
    assert!(took < Duration::from_secs(3), "connecting took {took:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shutdown_ends_every_session() {
    let (params, video) = synthetic_h264();
    let hub = hub(FakeFactory::default().with("cam", av_script(Codec::H264, params, video, false)));
    let stream = hub.upsert("cam", &["fake:cam"]).unwrap();
    let server = server(4, Duration::from_secs(30), WAIT).await;
    let mut client = connect(&server, stream.clone(), browser(false)).await.unwrap();
    assert_eq!(client.wait_for(WAIT, |e| *e == ClientEvent::Connected).await, Some(ClientEvent::Connected));
    server.shutdown();
    assert!(eventually(Duration::from_secs(5), || server.session_count() == 0).await);
    assert!(stream.snapshot().consumers.is_empty());
    let refused = connect(&server, stream, browser(false)).await;
    assert!(matches!(refused, Err(WhepError::Internal(_))), "{:?}", refused.err());
}
