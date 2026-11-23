//! `ffmpeg:<stream>#video=mjpeg` with real FFmpeg (M5 F5, D46): the pictures
//! come on FFmpeg's standard output, cut and published one keyframe each -
//! a picture wider than RTP/JPEG's 2040 pixels included, which the RTSP
//! publication could not carry; the relay read with the run's own credential
//! when the server asks its readers for one; and a run that can make no
//! picture fails, with FFmpeg's words, and is retried. Skipped when FFmpeg is
//! absent.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing, clippy::panic)]

mod common;

use std::sync::Arc;
use std::time::Duration;

use common::{Clip, Harness, encode, ffmpeg, jpeg_size, retry_reason};
use oxrtc_core::{Codec, ConsumerTag, Event, MediaWant, Want};
use oxrtc_rtsp_server::ReadAuth;

fn jpeg() -> Want {
    Want { video: MediaWant::OneOf(vec![Codec::Jpeg]), audio: MediaWant::Skip }
}

/// The first `wanted` pictures of `stream`: every one a keyframe.
async fn pictures(stream: &Arc<oxrtc_core::Stream>, wanted: usize) -> Vec<bytes::Bytes> {
    let mut sub = stream.subscribe(&jpeg(), ConsumerTag::new("test")).await.unwrap();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(40);
    let mut out = Vec::new();
    while out.len() < wanted {
        match tokio::time::timeout_at(deadline, sub.recv()).await.expect("too few pictures").unwrap() {
            Event::Tracks(tracks) => assert_eq!(tracks.iter().map(|t| t.codec).collect::<Vec<_>>(), [Codec::Jpeg]),
            Event::Frame { frame, .. } => {
                assert!(frame.keyframe);
                out.push(frame.data.clone());
            }
            Event::Discontinuity => {}
        }
    }
    out
}

#[tokio::test(flavor = "multi_thread")]
async fn a_stream_by_name_is_encoded_to_jpeg_pictures_wider_than_rtp_carries() {
    let Some(bin) = ffmpeg() else { return };
    let source = encode(&bin, Codec::H265, "2400x1350", 3).await;
    let h = Harness::start(
        &bin,
        vec![("camera", Clip::from_annex_b(Codec::H265, &source))],
        &[],
        4,
        Duration::from_secs(20),
        None,
    )
    .await;
    h.stream("camera_src", &["clip:camera"]);
    let camera = h.stream("camera", &["ffmpeg:camera_src#video=mjpeg"]);
    let got = pictures(&camera, 20).await;
    for picture in &got {
        assert_eq!(jpeg_size(picture), Some((2400, 1350)));
        assert!(picture.ends_with(&[0xff, 0xd9]));
    }
    assert!(got.windows(2).any(|w| w[0] != w[1]), "the pictures move");
    let source = &camera.snapshot().sources[0];
    assert_eq!(source.tracks.len(), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn it_reads_the_relay_with_the_runs_own_credential_and_scales() {
    let Some(bin) = ffmpeg() else { return };
    let source = encode(&bin, Codec::H264, "640x360", 3).await;
    let auth = Some(ReadAuth::new("operator", "s3cret-pass"));
    let clips = vec![("camera", Clip::from_annex_b(Codec::H264, &source))];
    let h = Harness::start_reading_as(&bin, clips, &[], 4, Duration::from_secs(20), None, auth).await;
    h.stream("camera_src", &["clip:camera"]);
    let camera = h.stream("camera", &["ffmpeg:camera_src#video=mjpeg#width=320"]);
    for picture in pictures(&camera, 5).await {
        assert_eq!(jpeg_size(&picture), Some((320, 180)));
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_stream_without_a_picture_fails_the_run_which_is_retried() {
    let Some(bin) = ffmpeg() else { return };
    // The stream it reads has no picture: the relay refuses `?video`.
    let h = Harness::start(&bin, vec![("mic", Clip::pcma())], &[], 4, Duration::from_secs(2), None).await;
    h.stream("mic_src", &["clip:mic"]);
    let camera = h.stream("camera", &["ffmpeg:mic_src#video=mjpeg"]);
    let reader = tokio::spawn({
        let camera = camera.clone();
        async move {
            // Held while it waits: its demand keeps the source trying.
            let mut sub = camera.subscribe(&jpeg(), ConsumerTag::new("test")).await.unwrap();
            while sub.recv().await.is_ok() {}
        }
    });
    let why = retry_reason(&camera, Duration::from_secs(30)).await;
    assert!(why.contains("ffmpeg"), "{why}");
    reader.abort();
}
