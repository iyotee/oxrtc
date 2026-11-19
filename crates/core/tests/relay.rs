//! `stream:` relays (and the loopback RTSP URLs that become them) are
//! aliases: a consumer of the relay reads the upstream stream's own source,
//! its ring, its GOP cache and its supervisor, resolved when it subscribes.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing, clippy::panic)]

mod common;

use std::sync::Arc;
use std::time::Duration;

use common::{After, FakeFactory, Script, hub_with, limits};
use oxrtc_core::{Codec, ConsumerTag, Event, Frame, Subscription, TrackInfo, Want};
use tokio::time::Instant;

fn tag() -> ConsumerTag {
    ConsumerTag::new("test")
}

/// The next `n` frames, with the subscription's track index.
async fn frames(sub: &mut Subscription, n: usize) -> Vec<(usize, Arc<Frame>)> {
    let mut out = Vec::new();
    while out.len() < n {
        match tokio::time::timeout(Duration::from_secs(30), sub.recv()).await.expect("timeout").expect("ended") {
            Event::Frame { track, frame } => out.push((track, frame)),
            Event::Tracks(_) | Event::Discontinuity => {}
        }
    }
    out
}

/// A camera with a picture and a sound, interleaved, a keyframe every 10 frames.
fn camera() -> Script {
    Script {
        tracks: vec![TrackInfo::video(Codec::H265), TrackInfo::audio(Codec::Pcma, 8000, 1)],
        frames: (0..400).map(|i| if i % 2 == 0 { (0, i % 20 == 0) } else { (1, false) }).collect(),
        ..Script::video(Codec::H265, vec![])
    }
}

#[tokio::test(start_paused = true)]
async fn a_relay_hands_on_the_upstream_frames_themselves() {
    let factory = Arc::new(FakeFactory::default());
    let fake = factory.add("cam", camera());
    let hub = hub_with(factory, limits());
    let upstream = hub.upsert("camera_1_src", &["fake:cam"]).unwrap();
    let relay = hub.upsert("camera_1", &["stream:camera_1_src?video"]).unwrap();
    let mut direct = upstream.subscribe(&Want::all(), tag()).await.unwrap();
    let mut relayed = relay.subscribe(&Want::all(), tag()).await.unwrap();
    let (direct, relayed) = tokio::join!(frames(&mut direct, 40), frames(&mut relayed, 10));
    for (track, frame) in &relayed {
        assert_eq!(*track, 0, "video only, as the relay asks");
        let same = direct.iter().find(|(_, d)| d.received == frame.received && d.pts == frame.pts).expect("upstream");
        // Not a copy published again into a ring of the relay's own: the
        // very allocation the upstream's ring carried.
        assert!(Arc::ptr_eq(&same.1, frame), "the relay copied a frame");
    }
    assert_eq!(fake.runs(), 1, "one source run feeds both");
}

/// When the first frame after a pause of more than a second came.
async fn back_after_the_outage(sub: &mut Subscription) -> Instant {
    let mut last: Option<Instant> = None;
    loop {
        let event = tokio::time::timeout(Duration::from_secs(60), sub.recv()).await.expect("timeout").expect("ended");
        if let Event::Frame { frame, .. } = event {
            if last.is_some_and(|at| frame.received.duration_since(at) > Duration::from_secs(1)) {
                return Instant::now();
            }
            last = Some(frame.received);
        }
    }
}

#[tokio::test(start_paused = true)]
async fn a_relay_comes_back_with_its_upstream_without_a_backoff_of_its_own() {
    // The upstream delivers for 2 s, then fails for about 9 s (longer than
    // the stall timeout, 3 s here), then delivers again. A relay with a
    // supervisor of its own stalled, failed, and slept in its own backoff
    // (up to 1.25 s here, 6.25 s by default) while the upstream was back.
    let factory = Arc::new(FakeFactory::default());
    let script = Script {
        frames: (0..50).map(|i| (0, i % 10 == 0)).collect(),
        after: After::End,
        outage: 2..14,
        // A source of this server: the old relay's resubscription failed at
        // once, so it spent the outage in its own backoff, not in a probe.
        local: true,
        ..Script::video(Codec::H264, vec![])
    };
    let fake = factory.add("cam", script);
    let hub = hub_with(factory, limits());
    let upstream = hub.upsert("camera_1_src", &["fake:cam"]).unwrap();
    let relay = hub.upsert("camera_1", &["stream:camera_1_src"]).unwrap();
    let mut direct = upstream.subscribe(&Want::all(), tag()).await.unwrap();
    let mut relayed = relay.subscribe(&Want::all(), tag()).await.unwrap();
    let (direct_at, relayed_at) = tokio::join!(back_after_the_outage(&mut direct), back_after_the_outage(&mut relayed));
    assert!(fake.runs() >= 14, "the outage happened: {} runs", fake.runs());
    let lag = relayed_at.saturating_duration_since(direct_at);
    assert!(lag < Duration::from_millis(50), "the relay came back {lag:?} after its upstream");
    // One supervisor, so one count of failures and one backoff.
    let (up, re) = (upstream.snapshot(), relay.snapshot());
    assert_eq!(re.sources[0].failures, up.sources[0].failures, "the relay failed on its own");
}

#[tokio::test(start_paused = true)]
async fn a_relay_leaves_its_upstream_after_one_idle_grace() {
    // With a relay that ran on its own, the upstream stopped one grace after
    // the relay did: two graces.
    let factory = Arc::new(FakeFactory::default());
    let fake = factory.add("cam", camera());
    let hub = hub_with(factory, limits());
    hub.upsert("camera_1_src", &["fake:cam"]).unwrap();
    let relay = hub.upsert("camera_1", &["stream:camera_1_src?video"]).unwrap();
    let mut viewer = relay.subscribe(&Want::all(), tag()).await.unwrap();
    frames(&mut viewer, 3).await;
    drop(viewer);
    tokio::time::sleep(limits().idle_grace + Duration::from_millis(200)).await;
    assert_eq!(fake.active(), 0, "the camera still runs");
}

#[tokio::test(start_paused = true)]
async fn a_preload_through_a_relay_holds_the_upstream_source() {
    let factory = Arc::new(FakeFactory::default());
    let fake = factory.add("cam", camera());
    let hub = hub_with(factory, limits());
    let upstream = hub.upsert("camera_1_src", &["fake:cam"]).unwrap();
    let relay = hub.upsert("camera_1_rec", &["stream:camera_1_src?video"]).unwrap();
    let held = relay.preload(&Want::from_query("video").unwrap()).await.unwrap();
    tokio::time::sleep(Duration::from_secs(5)).await;
    assert_eq!(fake.active(), 1);
    assert_eq!(upstream.snapshot().sources[0].demand, 1, "held once, by the preload of the relay");
    drop(held);
    tokio::time::sleep(limits().idle_grace + Duration::from_millis(200)).await;
    assert_eq!(fake.active(), 0);
}

#[tokio::test(start_paused = true)]
async fn a_relay_is_resolved_when_it_is_read() {
    // The stream it names may come later: the relay is not checked against
    // the hub when it is made, only when a consumer reads it.
    let factory = Arc::new(FakeFactory::default());
    factory.add("cam", camera());
    let hub = hub_with(factory, limits());
    let relay = hub.upsert("camera_1", &["stream:camera_1_src?video"]).unwrap();
    let info = relay.snapshot();
    assert_eq!(info.sources[0].reads.as_deref(), Some("camera_1_src"));
    assert_eq!(info.sources[0].last_error.as_deref(), Some("no stream named camera_1_src"));
    assert_eq!(relay.subscribe(&Want::all(), tag()).await.unwrap_err(), oxrtc_core::SubscribeError::NoMatchingSource);
    hub.upsert("camera_1_src", &["fake:cam"]).unwrap();
    let mut viewer = relay.subscribe(&Want::all(), tag()).await.unwrap();
    assert!(frames(&mut viewer, 3).await.iter().all(|(track, _)| *track == 0));
    // Its listing is the camera's (running once its watchdog has seen a
    // frame), with the tracks it passes.
    tokio::time::sleep(Duration::from_millis(500)).await;
    let info = relay.snapshot();
    let source = &info.sources[0];
    assert_eq!(source.url, "stream:camera_1_src?video");
    assert_eq!(source.state, oxrtc_core::SourceState::Running);
    assert_eq!(source.tracks, [TrackInfo::video(Codec::H265)], "the sound is not passed");
    assert_eq!(source.track_frames.len(), 1);
    assert!(source.frames > 0 && source.last_error.is_none(), "{source:?}");
}

#[tokio::test(start_paused = true)]
async fn a_relay_viewer_moves_to_the_stream_that_replaces_its_upstream() {
    let factory = Arc::new(FakeFactory::default());
    let old = factory.add("old", camera());
    let new = factory.add("new", Script::video(Codec::H264, (0..400).map(|i| (0, i % 10 == 0)).collect()));
    let hub = hub_with(factory, limits());
    hub.upsert("camera_1_src", &["fake:old"]).unwrap();
    let relay = hub.upsert("camera_1", &["stream:camera_1_src?video"]).unwrap();
    let mut viewer = relay.subscribe(&Want::all(), tag()).await.unwrap();
    frames(&mut viewer, 3).await;
    // The camera's URL changed: openIVR puts the stream again.
    hub.upsert("camera_1_src", &["fake:new"]).unwrap();
    let mut tracks = None;
    while tracks.is_none() {
        match tokio::time::timeout(Duration::from_secs(5), viewer.recv()).await.unwrap().unwrap() {
            Event::Tracks(list) => tracks = Some(list),
            Event::Frame { .. } | Event::Discontinuity => {}
        }
    }
    assert_eq!(&*tracks.unwrap(), [TrackInfo::video(Codec::H264)], "the new camera's picture");
    let after = frames(&mut viewer, 5).await;
    assert!(after[0].1.keyframe, "on a keyframe");
    assert_eq!(new.active(), 1);
    tokio::time::sleep(limits().idle_grace + Duration::from_millis(200)).await;
    assert_eq!(old.active(), 0, "the old camera is let go");
}

#[tokio::test(start_paused = true)]
async fn a_relay_viewer_ends_when_its_upstream_is_removed() {
    let factory = Arc::new(FakeFactory::default());
    factory.add("cam", camera());
    let hub = hub_with(factory, limits());
    hub.upsert("camera_1_src", &["fake:cam"]).unwrap();
    let relay = hub.upsert("camera_1", &["stream:camera_1_src?video"]).unwrap();
    let mut viewer = relay.subscribe(&Want::all(), tag()).await.unwrap();
    frames(&mut viewer, 3).await;
    assert!(hub.remove("camera_1_src"));
    let end = loop {
        match tokio::time::timeout(Duration::from_secs(5), viewer.recv()).await.unwrap() {
            Ok(_) => {}
            Err(end) => break end,
        }
    };
    assert_eq!(end, oxrtc_core::SubEnd::Closed);
}

#[tokio::test(start_paused = true)]
async fn a_viewer_keeps_its_picture_when_the_stream_lending_its_sound_goes() {
    // a's picture is its own camera, its sound b's microphone. Removing b
    // ended the viewer of a, picture and all, though the picture never was
    // b's; a relay that retried on its own kept it, and had the sound again
    // once b was back.
    let factory = Arc::new(FakeFactory::default());
    factory.add("cam", Script::video(Codec::H264, (0..4000).map(|i| (0, i % 10 == 0)).collect()));
    let mic = Script {
        tracks: vec![TrackInfo::audio(Codec::Pcma, 8000, 1)],
        frames: vec![(0, false); 4000],
        ..Script::video(Codec::H264, vec![])
    };
    factory.add("mic", mic);
    let hub = hub_with(factory, limits());
    hub.upsert("b", &["fake:mic"]).unwrap();
    let a = hub.upsert("a", &["fake:cam", "stream:b?audio"]).unwrap();
    let mut viewer = a.subscribe(&Want::all(), tag()).await.unwrap();
    let both = frames(&mut viewer, 40).await;
    assert!(both.iter().any(|(t, _)| *t == 0) && both.iter().any(|(t, _)| *t == 1), "picture and sound");
    assert!(hub.remove("b"));
    // About 4 s of the picture alone (`frames` fails if the viewer ends).
    let alone = frames(&mut viewer, 100).await;
    assert!(alone.iter().skip(20).all(|(t, _)| *t == 0), "the sound went with b");
    hub.upsert("b", &["fake:mic"]).unwrap();
    let back = frames(&mut viewer, 200).await;
    assert!(back.iter().any(|(t, _)| *t == 1), "the sound is back with b");
    assert!(back.iter().any(|(t, _)| *t == 0));
}

#[tokio::test(start_paused = true)]
async fn a_transcode_stops_with_its_last_viewer_and_the_camera_after_its_grace() {
    // openIVR's live stream: the camera's session, and the transcode a viewer
    // watches. The transcode is the expensive one: gone with its viewer
    // (`transcode_idle_grace`, 0 by default); the camera stays for
    // `idle_grace`, so a viewer who comes back soon finds it running (D36).
    let factory = Arc::new(FakeFactory::default());
    let cam = factory.add("cam", camera());
    let transcode = factory.add("h264", Script { transcodes: true, ..Script::video(Codec::H264, Script::gop(25)) });
    let hub = hub_with(factory, limits());
    hub.upsert("camera_1_src", &["fake:cam"]).unwrap();
    let live = hub.upsert("camera_1", &["fake:h264", "stream:camera_1_src?video"]).unwrap();
    let held = hub.get("camera_1_src").unwrap().subscribe(&Want::all(), tag()).await.unwrap();
    let mut viewer = live.subscribe(&Want::from_query("video=h264").unwrap(), tag()).await.unwrap();
    frames(&mut viewer, 3).await;
    assert_eq!((cam.active(), transcode.active()), (1, 1));
    drop((viewer, held));
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(transcode.active(), 0, "the transcode outlived its viewer");
    assert_eq!(cam.active(), 1, "the camera is kept for its grace");
    tokio::time::sleep(limits().idle_grace).await;
    assert_eq!(cam.active(), 0);

    // Each grace is the operator's (api.idle_grace_ms, api.transcode_idle_grace_ms).
    let factory = Arc::new(FakeFactory::default());
    let transcode = factory.add("h264", Script { transcodes: true, ..Script::video(Codec::H264, Script::gop(25)) });
    let kept = oxrtc_core::Limits { transcode_idle_grace: Duration::from_secs(3), ..limits() };
    let hub = hub_with(factory, kept);
    let live = hub.upsert("camera_1", &["fake:h264"]).unwrap();
    let mut viewer = live.subscribe(&Want::all(), tag()).await.unwrap();
    frames(&mut viewer, 3).await;
    drop(viewer);
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert_eq!(transcode.active(), 1, "kept for the grace the file gave");
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert_eq!(transcode.active(), 0);
}
