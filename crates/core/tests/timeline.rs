//! The hub's timeline through real supervision: a source that restarts its
//! timestamps, and a relay of it.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing, clippy::panic)]

mod common;

use std::collections::HashMap;
use std::sync::Arc;

use common::{After, FakeFactory, Script, hub_with, limits};
use oxrtc_core::timeline::ticks;
use oxrtc_core::{Codec, ConsumerTag, Event, Frame, Subscription, TrackInfo, Want};

async fn collect(sub: &mut Subscription, n: usize) -> Vec<Arc<Frame>> {
    let mut frames = Vec::new();
    while frames.len() < n {
        if let Event::Frame { frame, .. } = sub.recv().await.unwrap() {
            frames.push(frame);
        }
    }
    frames
}

/// A camera of two tracks whose every run plays 8 frames from pts 0, then ends.
fn restarting() -> Script {
    let mut script = Script {
        tracks: vec![TrackInfo::video(Codec::H264), TrackInfo::audio(Codec::Pcma, 8000, 1)],
        frames: (0..16).map(|i| if i % 2 == 0 { (0, i == 0) } else { (1, true) }).collect(),
        ..Script::video(Codec::H264, vec![])
    };
    script.after = After::End;
    script
}

#[tokio::test(start_paused = true)]
async fn a_source_that_restarts_its_timestamps_stays_on_the_wall_clock() {
    let factory = Arc::new(FakeFactory::default());
    // Its frames are 40 ms apart, and 3600 ticks of 90 kHz: its own steps
    // and the wall clock agree, so every time is exactly its arrival.
    let fake = factory.add("cam", Script { frames: Script::gop(8), ..restarting() });
    let hub = hub_with(factory, limits());
    let stream = hub.upsert("cam", &["fake:cam"]).unwrap();
    let mut sub = stream.subscribe(&Want::from_query("video").unwrap(), ConsumerTag::new("t")).await.unwrap();
    let frames = collect(&mut sub, 20).await;
    assert!(fake.runs() >= 3, "restarted: {} runs", fake.runs());
    for frame in &frames {
        let wall = ticks(frame.received - hub.epoch(), 90_000);
        assert_eq!(frame.pts, wall, "a frame's time is its arrival on the hub's clock");
    }
    assert!(frames.windows(2).all(|w| w[1].pts > w[0].pts), "never back");
}

#[tokio::test(start_paused = true)]
async fn a_relay_keeps_the_time_of_the_stream_it_reads() {
    let factory = Arc::new(FakeFactory::default());
    // 40 ms of media every 20 ms (a transcoder catching up), keyframes every
    // 8 frames: its times and its arrivals part, so a relay that placed
    // frames by their arrival would move them.
    let script = Script {
        frames: (0..24).map(|i| (0, i % 8 == 0)).collect(),
        interval: std::time::Duration::from_millis(20),
        ..Script::video(Codec::H264, vec![])
    };
    factory.add("cam", script);
    let hub = hub_with(factory, limits());
    let upstream = hub.upsert("camera_1_src", &["fake:cam"]).unwrap();
    let mut direct = upstream.subscribe(&Want::all(), ConsumerTag::new("t")).await.unwrap();
    let mut times: HashMap<_, _> = collect(&mut direct, 10).await.iter().map(|f| (f.received, f.pts)).collect();
    // The relay starts on the cached GOP, frame 8 on.
    let relay = hub.upsert("camera_1", &["stream:camera_1_src"]).unwrap();
    let mut relayed = relay.subscribe(&Want::all(), ConsumerTag::new("t")).await.unwrap();
    let relayed = collect(&mut relayed, 12).await;
    times.extend(collect(&mut direct, 12).await.iter().map(|f| (f.received, f.pts)));
    assert!(relayed[0].keyframe && relayed[0].pts == 8 * 3600 + times.values().min().unwrap());
    for frame in &relayed {
        assert_eq!(Some(&frame.pts), times.get(&frame.received), "the relay gave its own time");
    }
}

#[tokio::test(start_paused = true)]
async fn a_relay_started_again_on_the_cached_gop_keeps_the_time_of_the_stream_it_reads() {
    let factory = Arc::new(FakeFactory::default());
    // A 4 s GOP: a viewer who comes back within it restarts the relay on
    // frames older than the last one the relay gave the previous viewer.
    factory.add("cam", Script::video(Codec::H264, (0..300).map(|i| (0, i % 100 == 0)).collect()));
    let hub = hub_with(factory, limits());
    let upstream = hub.upsert("camera_1_src", &["fake:cam"]).unwrap();
    let mut direct = upstream.subscribe(&Want::all(), ConsumerTag::new("t")).await.unwrap();
    let mut times: HashMap<_, _> = collect(&mut direct, 5).await.iter().map(|f| (f.received, f.pts)).collect();
    let relay = hub.upsert("camera_1", &["stream:camera_1_src"]).unwrap();
    let first = collect(&mut relay.subscribe(&Want::all(), ConsumerTag::new("t")).await.unwrap(), 10).await;
    // The viewer left; past the idle grace the relay stops; it comes back.
    tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
    let again = collect(&mut relay.subscribe(&Want::all(), ConsumerTag::new("t")).await.unwrap(), 60).await;
    times.extend(collect(&mut direct, 100).await.iter().map(|f| (f.received, f.pts)));
    assert!(again[0].keyframe && again[0].received < first[9].received, "started on the cached GOP");
    for frame in first.iter().chain(&again) {
        assert_eq!(Some(&frame.pts), times.get(&frame.received), "the relay moved its time");
    }
}
