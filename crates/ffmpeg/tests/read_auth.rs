//! An `ffmpeg:` source that reads the server's own streams when the server's
//! readers must authenticate (R28, D24): the run reads with a credential of
//! its own, and without one it is refused. Skipped when FFmpeg is absent.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing, clippy::panic)]

mod common;

use std::sync::Arc;
use std::time::Duration;

use common::{Clip, FixedTarget, Harness, encode, ffmpeg, retry_reason};
use oxrtc_core::{Codec, ConsumerTag, Event, Want};
use oxrtc_rtsp_server::ReadAuth;

const OPERATOR: (&str, &str) = ("operator", "s3cret-pass");

async fn harness(bin: &std::path::Path, target: Option<Arc<dyn oxrtc_ffmpeg::PublishTarget>>) -> Harness {
    let source = encode(bin, Codec::H265, "640x360", 4).await;
    let clip = Clip::from_annex_b(Codec::H265, &source);
    let auth = Some(ReadAuth::new(OPERATOR.0, OPERATOR.1));
    Harness::start_reading_as(bin, vec![("camera", clip)], &[], 4, Duration::from_secs(20), target, auth).await
}

#[tokio::test(flavor = "multi_thread")]
async fn a_transcode_of_a_stream_by_name_reads_with_the_runs_own_credential() {
    let Some(bin) = ffmpeg() else { return };
    let h = harness(&bin, None).await;
    h.stream("camera_src", &["clip:camera"]);
    let camera = h.stream("camera", &["ffmpeg:camera_src#video=h264"]);
    let mut sub = camera.subscribe(&Want::from_query("video").unwrap(), ConsumerTag::new("test")).await.unwrap();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    let (mut codec, mut frames) = (None, 0u32);
    while frames < 30 {
        match tokio::time::timeout_at(deadline, sub.recv()).await.expect("too few frames").unwrap() {
            Event::Tracks(tracks) => codec = Some(tracks[0].codec),
            Event::Frame { .. } => frames += 1,
            Event::Discontinuity => {}
        }
    }
    assert_eq!(codec, Some(Codec::H264));
}

#[tokio::test(flavor = "multi_thread")]
async fn the_relays_url_written_out_is_read_with_it_too() {
    let Some(bin) = ffmpeg() else { return };
    let h = harness(&bin, None).await;
    h.stream("camera_src", &["clip:camera"]);
    let url = format!("ffmpeg:rtsp://{}/camera_src#video=h264", h.addr);
    let camera = h.stream("camera", &[url.as_str()]);
    let mut sub = camera.subscribe(&Want::from_query("video").unwrap(), ConsumerTag::new("test")).await.unwrap();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        let event = tokio::time::timeout_at(deadline, sub.recv()).await.expect("no frame").unwrap();
        if matches!(event, Event::Frame { .. }) {
            break;
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn with_no_credential_issued_the_run_is_refused_and_the_error_says_so() {
    let Some(bin) = ffmpeg() else { return };
    // A target that hands out a fixed URL has no credential to give.
    let h = harness(&bin, Some(Arc::new(FixedTarget("rtsp://127.0.0.1:9/_publish/none".into())))).await;
    h.stream("camera_src", &["clip:camera"]);
    let camera = h.stream("camera", &["ffmpeg:camera_src#video=h264"]);
    let _sub = camera.subscribe(&Want::from_query("video").unwrap(), ConsumerTag::new("test")).await.unwrap();
    let why = retry_reason(&camera, Duration::from_secs(30)).await;
    assert!(why.contains("401") || why.to_lowercase().contains("unauthorized"), "{why}");
    assert!(!why.contains(OPERATOR.1));
}
