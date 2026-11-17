//! The sound of a session starts at its connection. The subscription is
//! attached when the offer is answered, and what it delivers waits until ICE
//! and DTLS are up: the picture that waited is sent (nothing decodes without
//! its keyframe), the sound that waited is not. Sent in one burst, it starts
//! a browser's audio buffer as deep as the connection was long, and the
//! buffer never comes all the way back down; since the picture and the sound
//! are one `MediaStream` (`one_stream.rs`) the browser holds the picture for
//! the sound, so that depth is the picture's latency (L01, 2026-11-17: 116 ms
//! with the burst, against 38 ms before the two were one stream).
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing, clippy::panic)]

mod common;

use std::time::{Duration, Instant};

use common::*;
use oxrtc_core::Codec;
use str0m::media::MediaKind as RtcKind;
use tokio::net::UdpSocket;

/// How long the client waits before its connectivity checks: the sound of
/// that long (about 25 frames of 20 ms) is queued when the session connects.
const LATE: Duration = Duration::from_millis(500);

/// The arrival and the RTP time of every frame of `kind` the client receives
/// in `window`, counted from the first one.
async fn first_frames(client: &mut Client, kind: RtcKind, window: Duration) -> Vec<(Instant, u64)> {
    let mut frames: Vec<(Instant, u64)> = Vec::new();
    let deadline = tokio::time::Instant::now() + WAIT;
    while tokio::time::Instant::now() < deadline {
        let Ok(Some(event)) = tokio::time::timeout(Duration::from_millis(200), client.events.recv()).await else {
            if frames.is_empty() {
                continue;
            }
            break;
        };
        if let ClientEvent::Media { kind: k, at, time, .. } = event
            && k == kind
        {
            if frames.first().is_some_and(|(first, _)| at > *first + window) {
                break;
            }
            frames.push((at, time));
        }
    }
    frames
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_sound_that_waited_for_the_connection_is_not_sent() {
    let (params, video) = synthetic_h264();
    let hub = hub(FakeFactory::default().with("cam", av_script(Codec::H264, params, video, false)));
    let stream = hub.upsert("cam", &["fake:cam"]).unwrap();
    let server = server(8, Duration::from_secs(10), WAIT).await;
    let socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let mut client = connect_late(&server, stream, browser(false), socket, LATE).await.unwrap();

    // The script's sound: two frames of 20 ms every 39 ms, so about five in
    // 100 ms once live. The half second that waited is 25 more, at once.
    let sound = first_frames(&mut client, RtcKind::Audio, Duration::from_millis(100)).await;
    assert!(!sound.is_empty(), "the session has sound");
    assert!(sound.len() <= 15, "{} sound frames in the first 100 ms: the sound that waited was sent", sound.len());
    let (oldest, newest) = (sound.iter().map(|f| f.1).min().unwrap(), sound.iter().map(|f| f.1).max().unwrap());
    let span = Duration::from_millis((newest - oldest) / 48);
    assert!(span < Duration::from_millis(300), "the first 100 ms of sound span {span:?} of the camera's time");
}

/// The picture that waited is still sent: the session starts on the cached
/// keyframe, not at the next one.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_picture_that_waited_is_sent() {
    let (params, video) = synthetic_h264();
    let hub = hub(FakeFactory::default().with("cam", av_script(Codec::H264, params, video, false)));
    let stream = hub.upsert("cam", &["fake:cam"]).unwrap();
    let server = server(8, Duration::from_secs(10), WAIT).await;
    let socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let mut client = connect_late(&server, stream, browser(false), socket, LATE).await.unwrap();
    let first = client.wait_for(WAIT, |e| matches!(e, ClientEvent::Media { kind: RtcKind::Video, .. })).await;
    let Some(ClientEvent::Media { keyframe, .. }) = first else { panic!("no picture") };
    assert!(keyframe, "the first picture is a keyframe");
    // More pictures at once than a camera sends live: the ones that waited.
    let pictures = first_frames(&mut client, RtcKind::Video, Duration::from_millis(100)).await;
    assert!(pictures.len() >= 8, "{} pictures in 100 ms: what waited was not sent", pictures.len());
}
