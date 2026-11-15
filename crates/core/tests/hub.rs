//! The core's behaviour, end to end, on scripted sources and virtual time.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing, clippy::panic)]

mod common;

use std::sync::Arc;
use std::time::Duration;

use common::{After, FakeFactory, Script, hub_with, limits};
use oxrtc_core::{
    Codec, ConsumerTag, Event, HubError, Limits, MediaKind, SourceState, SpecError, SubEnd, SubscribeError, TrackInfo,
    Want,
};

fn tag() -> ConsumerTag {
    ConsumerTag::new("test")
}

async fn frames(sub: &mut oxrtc_core::Subscription, n: usize) -> Vec<(usize, bool)> {
    let mut out = Vec::new();
    while out.len() < n {
        match tokio::time::timeout(Duration::from_secs(30), sub.recv()).await.expect("timeout").expect("ended") {
            Event::Frame { track, frame } => out.push((track, frame.keyframe)),
            Event::Tracks(_) | Event::Discontinuity => {}
        }
    }
    out
}

#[tokio::test(start_paused = true)]
async fn first_event_is_tracks_and_video_starts_on_a_keyframe() {
    let factory = Arc::new(FakeFactory::default());
    factory.add("cam", Script::video(Codec::H264, [Script::gop(5), Script::gop(5)].concat()));
    let hub = hub_with(factory, limits());
    let stream = hub.upsert("cam", &["fake:cam"]).unwrap();
    let mut sub = stream.subscribe(&Want::all(), tag()).await.unwrap();
    match sub.recv().await.unwrap() {
        Event::Tracks(t) => assert_eq!(t[0].codec, Codec::H264),
        other => panic!("first event {other:?}"),
    }
    let got = frames(&mut sub, 10).await;
    assert!(got[0].1, "first frame must be a keyframe");
}

#[tokio::test(start_paused = true)]
async fn a_late_consumer_starts_at_once_on_the_cached_gop() {
    let factory = Arc::new(FakeFactory::default());
    factory.add("cam", Script::video(Codec::H265, Script::gop(30)));
    let hub = hub_with(factory, limits());
    let stream = hub.upsert("cam", &["fake:cam"]).unwrap();
    let _first = stream.subscribe(&Want::all(), tag()).await.unwrap();
    tokio::time::sleep(Duration::from_millis(40 * 10 + 20)).await;
    let mut late = stream.subscribe(&Want::all(), tag()).await.unwrap();
    assert!(matches!(late.recv().await.unwrap(), Event::Tracks(_)));
    let start = tokio::time::Instant::now();
    let got = frames(&mut late, 10).await;
    assert!(got[0].1, "cached GOP starts with its keyframe");
    assert!(start.elapsed() < Duration::from_millis(5), "the 10 cached frames arrive without waiting");
}

#[tokio::test(start_paused = true)]
async fn sources_run_on_demand_and_stop_after_the_grace() {
    let factory = Arc::new(FakeFactory::default());
    let fake = factory.add("cam", Script::video(Codec::H264, Script::gop(5)));
    let hub = hub_with(factory, limits());
    let stream = hub.upsert("cam", &["fake:cam"]).unwrap();
    tokio::time::sleep(Duration::from_secs(5)).await;
    assert_eq!(fake.runs(), 0, "nothing runs without a consumer");
    let sub = stream.subscribe(&Want::all(), tag()).await.unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(fake.active(), 1);
    drop(sub);
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(fake.active(), 1, "still within the grace");
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert_eq!(fake.active(), 0, "stopped after the grace");
    assert_eq!(stream.snapshot().sources[0].state, SourceState::Idle);
}

#[tokio::test(start_paused = true)]
async fn declared_codecs_pick_a_source_without_starting_the_others() {
    let factory = Arc::new(FakeFactory::default());
    let mut hevc = Script::video(Codec::H265, Script::gop(5));
    hevc.declared = Some(vec![(MediaKind::Video, Codec::H265)]);
    let mut avc = Script::video(Codec::H264, Script::gop(5));
    avc.declared = Some(vec![(MediaKind::Video, Codec::H264)]);
    let raw = factory.add("raw", hevc);
    let transcode = factory.add("x264", avc);
    let hub = hub_with(factory, limits());
    let stream = hub.upsert("cam", &["fake:raw", "fake:x264"]).unwrap();

    let mut sub = stream.subscribe(&Want::from_query("video=h264").unwrap(), tag()).await.unwrap();
    match sub.recv().await.unwrap() {
        Event::Tracks(t) => assert_eq!(t[0].codec, Codec::H264),
        other => panic!("{other:?}"),
    }
    assert_eq!(raw.runs(), 0, "the H.265 source was never started for an H.264 viewer");
    assert_eq!(transcode.runs(), 1);

    let mut any = stream.subscribe(&Want::all(), tag()).await.unwrap();
    match any.recv().await.unwrap() {
        Event::Tracks(t) => assert_eq!(t[0].codec, Codec::H265, "a viewer that takes anything gets the first source"),
        other => panic!("{other:?}"),
    }
}

#[tokio::test(start_paused = true)]
async fn an_unknown_source_is_probed_and_released_when_it_does_not_fit() {
    let factory = Arc::new(FakeFactory::default());
    let raw = factory.add("raw", Script::video(Codec::H265, Script::gop(5)));
    let mut avc = Script::video(Codec::H264, Script::gop(5));
    avc.declared = Some(vec![(MediaKind::Video, Codec::H264)]);
    factory.add("x264", avc);
    let hub = hub_with(factory, limits());
    let stream = hub.upsert("cam", &["fake:raw", "fake:x264"]).unwrap();
    let mut sub = stream.subscribe(&Want::from_query("video=h264").unwrap(), tag()).await.unwrap();
    assert!(matches!(sub.recv().await.unwrap(), Event::Tracks(t) if t[0].codec == Codec::H264));
    assert_eq!(raw.runs(), 1, "probed once to learn its codec");
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert_eq!(raw.active(), 0, "and released after the grace");
}

#[tokio::test(start_paused = true)]
async fn video_and_audio_from_two_sources_arrive_as_one_track_list() {
    let factory = Arc::new(FakeFactory::default());
    let mut video = Script::video(Codec::H265, Script::gop(5));
    video.declared = Some(vec![(MediaKind::Video, Codec::H265)]);
    let audio = Script {
        declared: Some(vec![(MediaKind::Audio, Codec::Opus)]),
        tracks: vec![TrackInfo::audio(Codec::Opus, 48_000, 2)],
        frames: vec![(0, false); 5],
        ..Script::video(Codec::Opus, vec![])
    };
    factory.add("v", video);
    factory.add("a", audio);
    let hub = hub_with(factory, limits());
    let stream = hub.upsert("cam", &["fake:v", "fake:a"]).unwrap();
    let mut sub = stream.subscribe(&Want::all(), tag()).await.unwrap();
    match sub.recv().await.unwrap() {
        Event::Tracks(t) => {
            assert_eq!(t.len(), 2);
            assert_eq!((t[0].kind, t[1].kind), (MediaKind::Video, MediaKind::Audio));
        }
        other => panic!("{other:?}"),
    }
    let got = frames(&mut sub, 10).await;
    assert!(got.iter().any(|(t, _)| *t == 0) && got.iter().any(|(t, _)| *t == 1));
}

#[tokio::test(start_paused = true)]
async fn a_failing_source_is_retried_with_backoff() {
    let factory = Arc::new(FakeFactory::default());
    let mut script = Script::video(Codec::H264, Script::gop(5));
    script.fail_first = 3;
    let fake = factory.add("cam", script);
    let hub = hub_with(factory, limits());
    let stream = hub.upsert("cam", &["fake:cam"]).unwrap();
    let mut sub = stream.subscribe(&Want::all(), tag()).await.unwrap();
    let got = frames(&mut sub, 3).await;
    assert!(got[0].1);
    assert_eq!(fake.runs(), 4);
}

#[tokio::test(start_paused = true)]
async fn refused_credentials_are_retried_slowly() {
    // A changed camera password: retried every backoff_max (1 s here, 5 s by
    // default) it would lock the account of a camera that counts failures.
    let factory = Arc::new(FakeFactory::default());
    let mut script = Script::video(Codec::H264, Script::gop(5));
    script.fail_first = usize::MAX;
    script.auth = true;
    // Declared, so the selection takes it on its word and the preload holds it.
    script.declared = Some(vec![(MediaKind::Video, Codec::H264)]);
    let fake = factory.add("cam", script);
    let limits = Limits {
        auth_backoff_initial: Duration::from_secs(30),
        auth_backoff_max: Duration::from_secs(120),
        ..limits()
    };
    let hub = hub_with(factory, limits);
    let stream = hub.upsert("cam", &["fake:cam"]).unwrap();
    let _held = stream.preload(&Want::from_query("video").unwrap()).await.unwrap();
    tokio::time::sleep(Duration::from_secs(300)).await;
    // 30 s, 60 s, then 120 s each (with jitter): at most 5 attempts in 300 s.
    assert!((2..=5).contains(&fake.runs()), "{} attempts in 300 s", fake.runs());
}

#[tokio::test(start_paused = true)]
async fn a_stalled_source_is_restarted() {
    let factory = Arc::new(FakeFactory::default());
    let fake = factory.add("cam", Script::video(Codec::H264, Script::gop(3)));
    let hub = hub_with(factory, limits());
    let stream = hub.upsert("cam", &["fake:cam"]).unwrap();
    let mut sub = stream.subscribe(&Want::all(), tag()).await.unwrap();
    frames(&mut sub, 3).await;
    assert_eq!(fake.runs(), 1);
    tokio::time::sleep(Duration::from_secs(5)).await;
    assert!(fake.runs() >= 2, "silent past the stall timeout: restarted");
}

#[tokio::test(start_paused = true)]
async fn a_source_may_set_its_own_stall_timeout() {
    let factory = Arc::new(FakeFactory::default());
    let mut script = Script::video(Codec::H264, Script::gop(3));
    script.stall_timeout = Some(Duration::from_secs(20));
    let fake = factory.add("cam", script);
    let hub = hub_with(factory, limits());
    let stream = hub.upsert("cam", &["fake:cam"]).unwrap();
    let mut sub = stream.subscribe(&Want::all(), tag()).await.unwrap();
    frames(&mut sub, 3).await;
    tokio::time::sleep(Duration::from_secs(10)).await;
    assert_eq!(fake.runs(), 1, "past the hub's 3 s but within the source's own 20 s: not restarted");
    tokio::time::sleep(Duration::from_secs(12)).await;
    assert!(fake.runs() >= 2, "past its own 20 s: restarted");
}

#[tokio::test(start_paused = true)]
async fn a_source_that_ends_is_restarted_and_its_consumer_is_told() {
    let factory = Arc::new(FakeFactory::default());
    let mut script = Script::video(Codec::H264, Script::gop(3));
    script.after = After::End;
    let fake = factory.add("cam", script);
    let hub = hub_with(factory, limits());
    let stream = hub.upsert("cam", &["fake:cam"]).unwrap();
    let mut sub = stream.subscribe(&Want::all(), tag()).await.unwrap();
    frames(&mut sub, 6).await;
    assert!(fake.runs() >= 2);
}

#[tokio::test(start_paused = true)]
async fn a_slow_consumer_loses_frames_and_resumes_on_a_keyframe() {
    let factory = Arc::new(FakeFactory::default());
    let frames_script: Vec<(u8, bool)> = (0..200).map(|i| (0, i % 25 == 0)).collect();
    factory.add("cam", Script::video(Codec::H264, frames_script));
    let small = Limits { ring_capacity: 16, gop_cache_bytes: 0, ..limits() };
    let hub = hub_with(factory, small);
    let stream = hub.upsert("cam", &["fake:cam"]).unwrap();
    let mut sub = stream.subscribe(&Want::all(), tag()).await.unwrap();
    assert!(matches!(sub.recv().await.unwrap(), Event::Tracks(_)));
    tokio::time::sleep(Duration::from_secs(4)).await;
    let mut saw_gap = false;
    loop {
        match sub.recv().await.unwrap() {
            Event::Discontinuity => saw_gap = true,
            Event::Frame { frame, .. } if saw_gap => {
                assert!(frame.keyframe, "after a gap, video resumes on a keyframe");
                break;
            }
            _ => {}
        }
    }
    assert!(stream.snapshot().consumers[0].dropped > 0);
}

#[tokio::test(start_paused = true)]
async fn upsert_with_the_same_sources_keeps_viewers_and_a_change_ends_them() {
    let factory = Arc::new(FakeFactory::default());
    factory.add("cam", Script::video(Codec::H264, Script::gop(5)));
    factory.add("other", Script::video(Codec::H264, Script::gop(5)));
    let hub = hub_with(factory, limits());
    let stream = hub.upsert("cam", &["fake:cam"]).unwrap();
    let mut sub = stream.subscribe(&Want::all(), tag()).await.unwrap();
    let same = hub.upsert("cam", &["fake:cam"]).unwrap();
    assert!(Arc::ptr_eq(&stream, &same));
    frames(&mut sub, 2).await;
    hub.upsert("cam", &["fake:other"]).unwrap();
    loop {
        if let Err(end) = sub.recv().await {
            assert_eq!(end, SubEnd::Closed);
            break;
        }
    }
}

#[tokio::test(start_paused = true)]
async fn a_stream_can_read_another_one_locally() {
    let factory = Arc::new(FakeFactory::default());
    let script = Script {
        declared: None,
        tracks: vec![TrackInfo::video(Codec::H265), TrackInfo::audio(Codec::Pcma, 8000, 1)],
        frames: (0..20).map(|i| if i % 2 == 0 { (0, i == 0) } else { (1, false) }).collect(),
        ..Script::video(Codec::H265, vec![])
    };
    factory.add("cam", script);
    let hub = hub_with(factory, limits());
    hub.upsert("camera_1_src", &["fake:cam"]).unwrap();
    let relay = hub.upsert("camera_1", &["stream:camera_1_src?video"]).unwrap();
    let mut sub = relay.subscribe(&Want::all(), tag()).await.unwrap();
    match sub.recv().await.unwrap() {
        Event::Tracks(t) => assert_eq!((t.len(), t[0].codec), (1, Codec::H265), "video only, as asked"),
        other => panic!("{other:?}"),
    }
    let got = frames(&mut sub, 5).await;
    assert!(got.iter().all(|(t, _)| *t == 0));
}

#[tokio::test(start_paused = true)]
async fn bad_streams_are_refused() {
    let factory = Arc::new(FakeFactory::default());
    let hub = hub_with(factory, limits());
    assert_eq!(hub.upsert("a b", &["stream:x"]).unwrap_err(), HubError::BadName);
    assert_eq!(hub.upsert::<&str>("a", &[]).unwrap_err(), HubError::NoSources);
    assert_eq!(
        hub.upsert("a", &["stream:a"]).unwrap_err(),
        HubError::Spec { index: 0, error: SpecError::SelfReference }
    );
    assert!(matches!(
        hub.upsert("a", &["exec:rm -rf /"]).unwrap_err(),
        HubError::Spec { error: SpecError::UnknownScheme(_), .. }
    ));
}

#[tokio::test(start_paused = true)]
async fn malformed_frames_are_refused_and_counted() {
    use oxrtc_core::{
        BoxFuture, CancellationToken, Drivers, FactoryCtx, Frame, Hub, ProducerSink, SourceDriver, SourceError,
        SourceFactory, SourceSpec,
    };
    struct Liar;
    impl SourceDriver for Liar {
        fn run(
            self: Arc<Self>,
            sink: ProducerSink,
            _c: CancellationToken,
        ) -> BoxFuture<'static, Result<(), SourceError>> {
            Box::pin(async move {
                sink.set_tracks(vec![TrackInfo::video(Codec::H264)]).unwrap();
                let lie = bytes::Bytes::from_static(&[0, 0, 0x10, 0, 0x65]);
                assert!(sink.publish(Frame::new(0, 0, true, lie)).is_err());
                assert!(sink.publish(Frame::new(3, 0, true, bytes::Bytes::from_static(&[1]))).is_err());
                std::future::pending().await
            })
        }
    }
    struct LiarFactory;
    impl SourceFactory for LiarFactory {
        fn schemes(&self) -> &[&'static str] {
            &["liar"]
        }
        fn build(&self, _: &SourceSpec, _: &FactoryCtx) -> Result<Arc<dyn SourceDriver>, SpecError> {
            Ok(Arc::new(Liar))
        }
    }
    let mut drivers = Drivers::new();
    drivers.register(Arc::new(LiarFactory));
    let hub = Hub::new(drivers, limits());
    let stream = hub.upsert("x", &["liar:"]).unwrap();
    let _p = stream.preload(&Want::all()).await.unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(stream.snapshot().sources[0].rejected, 2);
}

#[tokio::test(start_paused = true)]
async fn preload_holds_the_source_and_is_listed() {
    let factory = Arc::new(FakeFactory::default());
    // 20 s of frames: a source silent past the stall timeout would be between
    // two runs at the 10 s check, depending on the backoff's random jitter.
    let fake = factory.add("cam", Script::video(Codec::H264, Script::gop(500)));
    let hub = hub_with(factory, limits());
    let stream = hub.upsert("cam", &["fake:cam"]).unwrap();
    let preload = stream.preload(&Want::all()).await.unwrap();
    tokio::time::sleep(Duration::from_secs(10)).await;
    assert_eq!(fake.active(), 1);
    let snap = stream.snapshot();
    assert_eq!(snap.consumers[0].kind, "preload");
    assert_eq!(snap.sources[0].state, SourceState::Running);
    assert_eq!(snap.sources[0].track_frames.len(), 1, "counted per track");
    assert!((200..=260).contains(&snap.sources[0].track_frames[0]), "{:?}", snap.sources[0].track_frames);
    drop(preload);
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert_eq!(fake.active(), 0);
}

#[tokio::test(start_paused = true)]
async fn the_consumer_limit_holds() {
    let factory = Arc::new(FakeFactory::default());
    factory.add("cam", Script::video(Codec::H264, Script::gop(5)));
    let hub = hub_with(factory, Limits { max_consumers_per_stream: 2, ..limits() });
    let stream = hub.upsert("cam", &["fake:cam"]).unwrap();
    let _a = stream.subscribe(&Want::all(), tag()).await.unwrap();
    let _b = stream.subscribe(&Want::all(), tag()).await.unwrap();
    assert_eq!(stream.subscribe(&Want::all(), tag()).await.unwrap_err(), SubscribeError::TooManyConsumers);
}
