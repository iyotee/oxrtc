//! Each consumer's queue, bounded by bytes and time for its class: a
//! recorder rides out a pause a live viewer is cut at.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing, clippy::panic)]

mod common;

use std::sync::Arc;
use std::time::Duration;

use common::{FakeFactory, Script, hub_with, limits};
use oxrtc_core::queue::ITEM_OVERHEAD;
use oxrtc_core::{Codec, ConsumerClass, ConsumerTag, Event, Limits, QueueBudget, Subscription, TrackInfo, Want};

/// Picture and sound, one frame each 10 ms (so 500 items in 5 s, twice the
/// 256 of the ring this queue replaced), a keyframe each second.
fn camera() -> Script {
    Script {
        tracks: vec![TrackInfo::video(Codec::H264), TrackInfo::audio(Codec::Pcma, 8000, 1)],
        frames: (0..3000).map(|i| if i % 2 == 0 { (0, i % 100 == 0) } else { (1, false) }).collect(),
        interval: Duration::from_millis(10),
        ..Script::video(Codec::H264, vec![])
    }
}

/// What a consumer reads now, without waiting: frames, and whether a loss
/// came first; the first frame of video after it.
struct Read {
    frames: usize,
    lost: bool,
    video_after_loss: Option<bool>,
}

async fn read_ready(sub: &mut Subscription) -> Read {
    let mut read = Read { frames: 0, lost: false, video_after_loss: None };
    while let Ok(Ok(event)) = tokio::time::timeout(Duration::from_millis(1), sub.recv()).await {
        match event {
            Event::Discontinuity => read.lost = true,
            Event::Frame { track, frame } => {
                read.frames += 1;
                if read.lost && track == 0 && read.video_after_loss.is_none() {
                    read.video_after_loss = Some(frame.keyframe);
                }
            }
            Event::Tracks(_) => {}
        }
    }
    read
}

#[tokio::test(start_paused = true)]
async fn a_recorder_paused_5_s_loses_nothing_and_a_live_viewer_resumes_at_a_keyframe() {
    let factory = Arc::new(FakeFactory::default());
    factory.add("cam", camera());
    let hub = hub_with(factory, limits());
    let stream = hub.upsert("cam", &["fake:cam"]).unwrap();
    let recorder_tag = ConsumerTag::new("rtsp").with_class(ConsumerClass::Recorder);
    let mut recorder = stream.subscribe(&Want::all(), recorder_tag).await.unwrap();
    let mut viewer = stream.subscribe(&Want::all(), ConsumerTag::new("webrtc")).await.unwrap();
    read_ready(&mut recorder).await;
    read_ready(&mut viewer).await;
    // Neither reads for 5 s: 500 frames published meanwhile.
    tokio::time::sleep(Duration::from_secs(5)).await;
    let recorded = read_ready(&mut recorder).await;
    assert!(!recorded.lost, "a recorder's 10 s budget covers a 5 s pause");
    assert!(recorded.frames >= 499, "{} of the 500 frames reached the recorder", recorded.frames);
    let viewed = read_ready(&mut viewer).await;
    assert!(viewed.lost, "a live viewer 5 s behind is cut");
    assert_eq!(viewed.video_after_loss, Some(true), "and resumes at a keyframe");
    assert!(viewed.frames < 250, "{} frames: it keeps only what follows its latest keyframe", viewed.frames);
    let consumers = stream.snapshot().consumers;
    assert_eq!(
        consumers.iter().map(|c| (c.kind.as_str(), c.dropped > 0)).collect::<Vec<_>>(),
        [("rtsp", false), ("webrtc", true)]
    );
}

#[tokio::test(start_paused = true)]
async fn a_recorder_past_its_budget_resumes_at_a_keyframe() {
    let factory = Arc::new(FakeFactory::default());
    factory.add("cam", camera());
    let small = Limits {
        recorder_queue: QueueBudget { max_bytes: 32 * 1024 * 1024, max_age: Duration::from_secs(2) },
        ..limits()
    };
    let hub = hub_with(factory, small);
    let stream = hub.upsert("cam", &["fake:cam"]).unwrap();
    let tag = ConsumerTag::new("rtsp").with_class(ConsumerClass::Recorder);
    let mut recorder = stream.subscribe(&Want::all(), tag).await.unwrap();
    read_ready(&mut recorder).await;
    tokio::time::sleep(Duration::from_secs(5)).await;
    let read = read_ready(&mut recorder).await;
    assert!(read.lost);
    assert_eq!(read.video_after_loss, Some(true));
    // It kept what follows its latest keyframe within 2 s: not nothing.
    assert!(read.frames >= 100, "{} frames kept", read.frames);
}

#[tokio::test(start_paused = true)]
async fn a_queue_is_bounded_by_its_bytes_too() {
    let factory = Arc::new(FakeFactory::default());
    factory.add("cam", camera());
    // A day of age, and 64 KiB: the bytes bind.
    let small =
        Limits { live_queue: QueueBudget { max_bytes: 64 * 1024, max_age: Duration::from_secs(86_400) }, ..limits() };
    let hub = hub_with(factory, small);
    let stream = hub.upsert("cam", &["fake:cam"]).unwrap();
    let mut viewer = stream.subscribe(&Want::all(), ConsumerTag::new("webrtc")).await.unwrap();
    read_ready(&mut viewer).await;
    tokio::time::sleep(Duration::from_secs(5)).await;
    let read = read_ready(&mut viewer).await;
    assert!(read.lost);
    // Half picture (6 bytes), half sound (160): at most what 64 KiB holds.
    let pair = 6 + 160 + 2 * ITEM_OVERHEAD;
    assert!(read.frames <= 2 * (64 * 1024 / pair + 1), "{} frames kept", read.frames);
}

/// The owner's cameras send a full picture every 6 s (`GovLength 90`). The live
/// class cuts a reader stalled past 2 s to its latest keyframe, and with that
/// keyframe older than the budget there is nothing to resume at: the reader is
/// shown no picture until the camera's next keyframe, which is the price of
/// not being left behind (a detector fed by `?video&queue=live`, R36).
#[tokio::test(start_paused = true)]
async fn a_live_reader_stalled_on_a_six_second_gop_waits_for_the_next_keyframe() {
    let factory = Arc::new(FakeFactory::default());
    let camera = Script {
        frames: (0..1000).map(|i| (0, i % 150 == 0)).collect(),
        interval: Duration::from_millis(40),
        ..Script::video(Codec::H264, vec![])
    };
    factory.add("cam", camera);
    let hub = hub_with(factory, limits());
    let stream = hub.upsert("cam", &["fake:cam"]).unwrap();
    let mut reader =
        stream.subscribe(&Want::all(), ConsumerTag::new("rtsp").with_class(ConsumerClass::Live)).await.unwrap();
    read_ready(&mut reader).await;
    tokio::time::sleep(Duration::from_secs(4)).await;
    let stalled = read_ready(&mut reader).await;
    assert!(stalled.lost, "4 s behind is beyond the live 2 s");
    assert_eq!(
        stalled.frames, 0,
        "the keyframe is 4 s old and the P-frames after it are of no use: {} frames",
        stalled.frames
    );
    tokio::time::sleep(Duration::from_millis(2500)).await;
    let resumed = read_ready(&mut reader).await;
    assert_eq!(resumed.video_after_loss, Some(true), "the picture is back at the next keyframe");
    assert!(resumed.frames > 0);
}

/// A reader that draws from the output memory budget is bounded by its
/// share of it, and the line that says it fell behind names that share
/// (D45 review): it named only its class's `max_bytes`, 32 MiB for a
/// recorder, a bound such a reader never reached on a small host.
#[tokio::test(start_paused = true)]
async fn a_pooled_reader_that_falls_behind_is_told_its_share_of_the_output_budget() {
    use oxrtc_core::host_memory::{Fixed, HostReading, MemorySource};
    use oxrtc_core::memory::{BudgetSize, MemoryBudget};
    let (logs, _guard) = common::Logs::capture();
    let factory = Arc::new(FakeFactory::default());
    factory.add("cam", camera());
    let hub = hub_with(factory, limits());
    let reading = HostReading { limit: 8 << 30, available: 8 << 30, source: MemorySource::Meminfo };
    assert!(hub.set_output_memory(MemoryBudget::new(BudgetSize::Bytes(64 * 1024), Box::new(Fixed(reading)))));
    let stream = hub.upsert("cam", &["fake:cam"]).unwrap();
    let tag = ConsumerTag::new("mp4").with_class(ConsumerClass::Recorder).pooled();
    let mut reader = stream.subscribe(&Want::all(), tag).await.unwrap();
    read_ready(&mut reader).await;
    // Paused 6 s: 600 items, more than a 64 KiB share holds (each costs its
    // payload and ITEM_OVERHEAD), well under the class's 32 MiB and 10 s.
    tokio::time::sleep(Duration::from_secs(6)).await;
    assert!(read_ready(&mut reader).await.lost, "the share bounded the reader");
    let warnings = logs.lines("consumer fell behind its queue budget");
    assert_eq!(warnings.len(), 1, "{warnings:?}");
    assert!(warnings[0].contains("output_share=65536"), "{}", warnings[0]);
    assert!(warnings[0].contains("max_bytes=33554432"), "{}", warnings[0]);
}
