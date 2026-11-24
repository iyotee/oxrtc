//! A stream's transcode of itself (Home Assistant's `ffmpeg:<camera>#audio=opus`
//! beside the camera, D48): a reader that feeds a transcode is never given
//! one, so the transcode reads the camera and never itself; any other reader
//! takes the stream's sources in order, as before.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing, clippy::panic)]

mod common;

use std::sync::Arc;
use std::time::Duration;

use common::{FakeFactory, Script, limits};
use oxrtc_core::{
    Codec, ConsumerTag, Drivers, Event, FactoryCtx, Hub, MediaKind, SourceDriver, SourceFactory, SourceSpec, SpecError,
    TrackInfo, Want,
};

/// The fakes under the `ffmpeg` scheme: `ffmpeg:<name>#...` is the fake
/// named `<name>`.
struct AsFfmpeg(Arc<FakeFactory>);

impl SourceFactory for AsFfmpeg {
    fn schemes(&self) -> &[&'static str] {
        &["ffmpeg"]
    }
    fn build(&self, spec: &SourceSpec, ctx: &FactoryCtx) -> Result<Arc<dyn SourceDriver>, SpecError> {
        self.0.build(spec, ctx)
    }
}

/// An Opus sound, declared as an `ffmpeg:...#audio=opus` declares it.
fn opus() -> Script {
    Script {
        declared: Some(vec![(MediaKind::Audio, Codec::Opus)]),
        tracks: vec![TrackInfo::audio(Codec::Opus, 48_000, 2)],
        frames: vec![(0, false); 500],
        ..Script::video(Codec::Opus, vec![])
    }
}

async fn tracks(stream: &Arc<oxrtc_core::Stream>, want: &str, tag: ConsumerTag) -> Vec<Codec> {
    let mut sub = stream.subscribe(&Want::from_query(want).unwrap(), tag).await.unwrap();
    match tokio::time::timeout(Duration::from_secs(30), sub.recv()).await.unwrap().unwrap() {
        Event::Tracks(tracks) => tracks.iter().map(|t| t.codec).collect(),
        other => panic!("{other:?}"),
    }
}

#[tokio::test(start_paused = true)]
async fn a_transcode_of_its_own_stream_never_reads_itself() {
    let factory = Arc::new(FakeFactory::default());
    let own = factory.add("own", opus());
    let camera = Script {
        tracks: vec![TrackInfo::video(Codec::H264), TrackInfo::audio(Codec::Pcma, 8000, 1)],
        frames: (0..500).map(|n| ((n % 2) as u8, n % 20 == 0)).collect(),
        ..Script::video(Codec::H264, vec![])
    };
    let cam = factory.add("cam", camera);
    let mut drivers = Drivers::new();
    drivers.register(factory.clone());
    drivers.register(Arc::new(AsFfmpeg(factory)));
    let hub = Hub::new(drivers, limits());
    // Listed first on purpose: order alone would give it to its own reader.
    let stream = hub.upsert("own", &["ffmpeg:own#audio=opus", "fake:cam"]).unwrap();
    // A viewer that asks for Opus is given the transcode, the first source.
    assert_eq!(tracks(&stream, "audio=opus", ConsumerTag::new("webrtc")).await, [Codec::Opus]);
    // The transcode's own reader (the relay's, saying `source=ffmpeg:`) asks
    // for the sound: the camera's, never the transcode's.
    let reader = ConsumerTag::new("rtsp").feeding_a_transcode();
    assert_eq!(tracks(&stream, "audio", reader.clone()).await, [Codec::Pcma]);
    assert_eq!(tracks(&stream, "video&audio", reader).await, [Codec::H264, Codec::Pcma]);
    assert!(own.runs() >= 1 && cam.runs() >= 1);
}

#[tokio::test(start_paused = true)]
async fn a_transcode_of_another_stream_is_still_read_by_a_transcode() {
    // `b: ffmpeg:a#audio=pcma` reading `a: [ffmpeg:b#audio=opus]`: a's source
    // transcodes another stream, not itself, and stays a candidate.
    let factory = Arc::new(FakeFactory::default());
    factory.add("b", opus());
    let mut drivers = Drivers::new();
    drivers.register(factory.clone());
    drivers.register(Arc::new(AsFfmpeg(factory)));
    let hub = Hub::new(drivers, limits());
    let stream = hub.upsert("a", &["ffmpeg:b#audio=opus"]).unwrap();
    let reader = ConsumerTag::new("rtsp").feeding_a_transcode();
    assert_eq!(tracks(&stream, "audio", reader).await, [Codec::Opus]);
}
