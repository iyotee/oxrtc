//! Sources that fail before they deliver: falling through to the next one,
//! and answering without a track whose source cannot produce it.
//!
//! openIVR's layout depends on both. Its de-duplicated sound transcode exits
//! for audio it cannot de-duplicate, and go2rtc then falls through to the
//! plain transcode listed after it. Its public stream lists a sound transcode
//! third, which exits at once on a camera without a microphone, and the
//! picture must still play.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing, clippy::panic)]

mod common;

use std::sync::Arc;
use std::time::Duration;

use common::{FakeFactory, Script, hub_with, limits};
use oxrtc_core::{Codec, ConsumerTag, Event, MediaKind, TrackInfo, Want};
use tokio::time::Instant;

fn tag() -> ConsumerTag {
    ConsumerTag::new("test")
}

/// An Opus source that declares its codec, as an `ffmpeg:...#audio=opus` does.
fn opus(fail_first: usize) -> Script {
    Script {
        declared: Some(vec![(MediaKind::Audio, Codec::Opus)]),
        tracks: vec![TrackInfo::audio(Codec::Opus, 48_000, 2)],
        frames: vec![(0, false); 50],
        fail_first,
        ..Script::video(Codec::Opus, vec![])
    }
}

async fn first_tracks(sub: &mut oxrtc_core::Subscription) -> Arc<[TrackInfo]> {
    match tokio::time::timeout(Duration::from_secs(30), sub.recv()).await.expect("timeout").expect("ended") {
        Event::Tracks(tracks) => tracks,
        other => panic!("{other:?}"),
    }
}

#[tokio::test(start_paused = true)]
async fn a_declared_source_that_fails_falls_through_to_the_next() {
    let factory = Arc::new(FakeFactory::default());
    let dedup = factory.add("dedup", opus(usize::MAX));
    let plain = factory.add("plain", opus(0));
    let hub = hub_with(factory, limits());
    let stream = hub.upsert("cam_rec", &["fake:dedup", "fake:plain"]).unwrap();

    let started = Instant::now();
    let mut sub = stream.subscribe(&Want::from_query("audio=opus").unwrap(), tag()).await.unwrap();
    let tracks = first_tracks(&mut sub).await;
    assert_eq!(tracks[0].codec, Codec::Opus);
    assert!(
        started.elapsed() < Duration::from_millis(500),
        "fell through at its first failure: {:?}",
        started.elapsed()
    );
    assert!(dedup.runs() >= 1, "the first source was tried");
    assert_eq!(plain.runs(), 1, "and the next one serves");

    // A source that is failing right now is passed over without a new wait.
    let started = Instant::now();
    let mut again = stream.subscribe(&Want::from_query("audio=opus").unwrap(), tag()).await.unwrap();
    first_tracks(&mut again).await;
    assert!(started.elapsed() < Duration::from_millis(50), "{:?}", started.elapsed());
}

#[tokio::test(start_paused = true)]
async fn a_preload_falls_through_too() {
    let factory = Arc::new(FakeFactory::default());
    factory.add("dedup", opus(usize::MAX));
    let plain = factory.add("plain", opus(0));
    let hub = hub_with(factory, limits());
    let stream = hub.upsert("cam_rec", &["fake:dedup", "fake:plain"]).unwrap();
    let _preload = stream.preload(&Want::from_query("audio=opus").unwrap()).await.unwrap();
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert_eq!(plain.active(), 1, "the preload holds the source that works");
}

#[tokio::test(start_paused = true)]
async fn a_sole_source_keeps_its_chance_after_a_failed_attempt() {
    // Nowhere else to go: a camera that refuses one connection (cameras that
    // ration them do) is retried until the deadline, not abandoned.
    let factory = Arc::new(FakeFactory::default());
    let mut script = Script::video(Codec::H264, Script::gop(5));
    script.declared = Some(vec![(MediaKind::Video, Codec::H264)]);
    script.fail_first = 2;
    let fake = factory.add("cam", script);
    let hub = hub_with(factory, limits());
    let stream = hub.upsert("cam", &["fake:cam"]).unwrap();
    let mut sub = stream.subscribe(&Want::all(), tag()).await.unwrap();
    assert_eq!(first_tracks(&mut sub).await[0].codec, Codec::H264);
    assert_eq!(fake.runs(), 3);
}

#[tokio::test(start_paused = true)]
async fn a_failing_unknown_source_falls_through_to_a_declared_one() {
    let factory = Arc::new(FakeFactory::default());
    let mut camera = Script::video(Codec::H265, Script::gop(5));
    camera.fail_first = usize::MAX;
    factory.add("cam", camera);
    let mut avc = Script::video(Codec::H264, Script::gop(5));
    avc.declared = Some(vec![(MediaKind::Video, Codec::H264)]);
    factory.add("x264", avc);
    let hub = hub_with(factory, limits());
    let stream = hub.upsert("cam", &["fake:cam", "fake:x264"]).unwrap();
    let started = Instant::now();
    // Video only: for sound the camera is the one candidate, which keeps its
    // chance until the deadline.
    let mut sub = stream.subscribe(&Want::from_query("video").unwrap(), tag()).await.unwrap();
    assert_eq!(first_tracks(&mut sub).await[0].codec, Codec::H264);
    assert!(started.elapsed() < Duration::from_millis(500), "{:?}", started.elapsed());
}

#[tokio::test(start_paused = true)]
async fn a_sound_source_that_fails_does_not_hold_back_the_picture() {
    let factory = Arc::new(FakeFactory::default());
    factory.add("video", Script::video(Codec::H264, Script::gop(25)));
    factory.add("sound", opus(usize::MAX));
    let hub = hub_with(factory, limits());
    let stream = hub.upsert("camera_1", &["fake:video", "fake:sound"]).unwrap();

    let started = Instant::now();
    let mut sub = stream.subscribe(&Want::all(), tag()).await.unwrap();
    let tracks = first_tracks(&mut sub).await;
    assert_eq!(tracks.len(), 1, "{tracks:?}");
    assert_eq!(tracks[0].kind, MediaKind::Video);
    // Before this, the answer waited the whole probe deadline (2 s here,
    // 8 s by default) for a sound that was never coming.
    assert!(started.elapsed() < Duration::from_millis(500), "{:?}", started.elapsed());
    match tokio::time::timeout(Duration::from_secs(5), sub.recv()).await.unwrap().unwrap() {
        Event::Frame { track: 0, frame } => assert!(frame.keyframe),
        other => panic!("{other:?}"),
    }
}

#[tokio::test(start_paused = true)]
async fn a_working_sound_source_is_kept() {
    // Giving up a failed sound must not drop a sound that works.
    let factory = Arc::new(FakeFactory::default());
    factory.add("video", Script::video(Codec::H264, Script::gop(25)));
    factory.add("sound", opus(0));
    let hub = hub_with(factory, limits());
    let stream = hub.upsert("camera_1", &["fake:video", "fake:sound"]).unwrap();
    let mut sub = stream.subscribe(&Want::all(), tag()).await.unwrap();
    let tracks = first_tracks(&mut sub).await;
    assert_eq!(tracks.len(), 2, "{tracks:?}");
    assert_eq!((tracks[0].kind, tracks[1].kind), (MediaKind::Video, MediaKind::Audio));
}

#[tokio::test(start_paused = true)]
async fn the_first_track_list_arrives_within_the_deadline_of_the_subscribe_call() {
    // A sound source that runs without ever delivering a sound track: the
    // picture goes out at the probe deadline counted from the subscribe call.
    let factory = Arc::new(FakeFactory::default());
    factory.add("video", Script::video(Codec::H264, Script::gop(250)));
    let mut sound = opus(0);
    sound.tracks = vec![TrackInfo::video(Codec::H264)];
    sound.frames = Vec::new();
    factory.add("sound", sound);
    let hub = hub_with(factory, limits());
    let stream = hub.upsert("camera_1", &["fake:video", "fake:sound"]).unwrap();
    let started = Instant::now();
    let mut sub = stream.subscribe(&Want::all(), tag()).await.unwrap();
    let tracks = first_tracks(&mut sub).await;
    assert_eq!(tracks.len(), 1);
    let waited = started.elapsed();
    assert!(waited <= limits().probe_timeout + Duration::from_millis(150), "{waited:?}");
}
