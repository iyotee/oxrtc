//! A source that takes long to know its tracks (FFmpeg transcoding a webcam
//! that sends one picture a second probes 20 of them first: go2rtc answered
//! the owner's `camera_4` after 28.6 s) is waited for past the probe
//! deadline, while a source that fails is not: a camera that is down answers
//! at the probe deadline, as before.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing, clippy::panic)]

mod common;

use std::time::Duration;

use common::{FakeFactory, Script, hub_with, limits};
use oxrtc_core::{Codec, ConsumerTag, Event, MediaKind, SubEnd, Want};
use tokio::time::Instant;

/// A transcode: it declares H.264 before it runs, and delivers it `delay` after it starts.
fn transcode(delay: Duration, fail_first: usize) -> Script {
    Script {
        declared: Some(vec![(MediaKind::Video, Codec::H264)]),
        start_delay: delay,
        // What an FFmpeg driver gives its transcode (`FfmpegConfig::start_timeout`).
        start_timeout: Some(Duration::from_secs(30)),
        fail_first,
        ..Script::video(Codec::H264, Script::gop(250))
    }
}

#[tokio::test(start_paused = true)]
async fn a_source_slower_than_the_probe_deadline_is_waited_for() {
    let factory = std::sync::Arc::new(FakeFactory::default());
    factory.add("slow", transcode(Duration::from_secs(20), 0));
    let limits = limits();
    assert!(limits.probe_timeout < Duration::from_secs(20) && Duration::from_secs(20) < limits.first_track_timeout);
    let stream = hub_with(factory, limits.clone()).upsert("camera_4", &["fake:slow"]).unwrap();
    let started = Instant::now();
    let mut sub = stream.subscribe(&Want::all(), ConsumerTag::new("test")).await.unwrap();
    let event = tokio::time::timeout(Duration::from_secs(60), sub.recv()).await.unwrap().unwrap();
    assert!(matches!(event, Event::Tracks(_)), "{event:?}");
    let waited = started.elapsed();
    assert!(waited >= Duration::from_secs(20) && waited < Duration::from_secs(21), "{waited:?}");
}

#[tokio::test(start_paused = true)]
async fn a_source_that_never_starts_is_given_up_at_the_longer_deadline() {
    let factory = std::sync::Arc::new(FakeFactory::default());
    factory.add("hung", transcode(Duration::from_secs(3600), 0));
    let limits = limits();
    let stream = hub_with(factory, limits.clone()).upsert("camera_4", &["fake:hung"]).unwrap();
    let started = Instant::now();
    let mut sub = stream.subscribe(&Want::all(), ConsumerTag::new("test")).await.unwrap();
    let end = tokio::time::timeout(Duration::from_secs(120), sub.recv()).await.unwrap().unwrap_err();
    assert_eq!(end, SubEnd::NoTracks);
    let waited = started.elapsed();
    let hard = limits.first_track_timeout;
    assert!(waited >= hard && waited <= hard + Duration::from_millis(150), "{waited:?}");
}

#[tokio::test(start_paused = true)]
async fn a_source_that_fails_is_not_waited_for_past_the_probe_deadline() {
    let factory = std::sync::Arc::new(FakeFactory::default());
    factory.add("down", transcode(Duration::from_millis(500), usize::MAX));
    let limits = limits();
    let stream = hub_with(factory, limits.clone()).upsert("camera_4", &["fake:down"]).unwrap();
    let started = Instant::now();
    let mut sub = stream.subscribe(&Want::all(), ConsumerTag::new("test")).await.unwrap();
    let end = tokio::time::timeout(Duration::from_secs(120), sub.recv()).await.unwrap().unwrap_err();
    assert_eq!(end, SubEnd::NoTracks);
    let waited = started.elapsed();
    assert!(
        waited >= limits.probe_timeout && waited <= limits.probe_timeout + Duration::from_millis(150),
        "{waited:?}"
    );
}

#[tokio::test(start_paused = true)]
async fn a_source_silent_past_the_connect_timeout_is_restarted_unless_its_driver_asks_for_more() {
    let factory = std::sync::Arc::new(FakeFactory::default());
    let patient = factory.add("patient", transcode(Duration::from_secs(20), 0));
    let mut hasty = transcode(Duration::from_secs(20), 0);
    hasty.start_timeout = None;
    let hasty = factory.add("hasty", hasty);
    let hub = hub_with(factory, limits());
    let patient_stream = hub.upsert("a", &["fake:patient"]).unwrap();
    let hasty_stream = hub.upsert("b", &["fake:hasty"]).unwrap();
    let _a = patient_stream.preload(&Want::all()).await.unwrap();
    let _b = hasty_stream.preload(&Want::all()).await.unwrap();
    tokio::time::sleep(Duration::from_secs(25)).await;
    assert_eq!(patient.runs(), 1, "one run, still starting");
    assert!(hasty.runs() > 1, "restarted every connect timeout ({} runs)", hasty.runs());
}
