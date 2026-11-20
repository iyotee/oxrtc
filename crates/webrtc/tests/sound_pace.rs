//! The sound of a session leaves on its stream's own clock (D30, roadmap
//! W1b). A camera's sound comes in pairs, and a camera on a poor link stops
//! for a second and then delivers that second at once; a browser's audio
//! buffer follows the worst lateness it sees, and holds the picture as long
//! since the two are one `MediaStream`. On the wire, to a real client:
//! frames delivered in pairs arrive one frame apart, and with
//! `Playout::sound_late_max` the sound a stall kept back is not sent.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing, clippy::panic)]

mod common;

use std::sync::Arc;
use std::time::{Duration, Instant};

use bytes::Bytes;
use common::*;
use oxrtc_core::{
    BoxFuture, CancellationToken, Codec, Drivers, FactoryCtx, Frame, Hub, Limits, ProducerSink, SourceDriver,
    SourceError, SourceFactory, SourceSpec, SpecError, TrackInfo,
};
use oxrtc_webrtc::{Playout, WebRtcConfig, WebRtcServer};
use str0m::media::MediaKind as RtcKind;

const FRAME: Duration = Duration::from_millis(20);
const OPUS_SILENCE: &[u8] = &[0xf8, 0xff, 0xfe];

/// A camera: one keyframe per batch of sound, and the sound's 20 ms frames
/// delivered `batch` at a time. After `stall_after`
/// batches it stops for `stall`, then delivers what the stall kept back at
/// once, as a camera catching up over TCP does, and goes on.
#[derive(Clone)]
struct Camera {
    batch: u64,
    stall_after: Option<u64>,
    stall: Duration,
}

fn refused(error: impl std::fmt::Display) -> SourceError {
    SourceError::Refused(error.to_string())
}

impl SourceDriver for Camera {
    fn declared(&self) -> Option<oxrtc_core::Declared> {
        None
    }

    fn run(
        self: Arc<Self>,
        sink: ProducerSink,
        _cancel: CancellationToken,
    ) -> BoxFuture<'static, Result<(), SourceError>> {
        Box::pin(async move {
            let (params, video) = synthetic_h264();
            sink.set_tracks(vec![
                TrackInfo::video(Codec::H264).with_params(params),
                TrackInfo::audio(Codec::Opus, 48_000, 2),
            ])
            .map_err(refused)?;
            let key = video.iter().find(|f| f.keyframe).unwrap().clone();
            let every = FRAME * u32::try_from(self.batch).unwrap();
            let start = tokio::time::Instant::now();
            let (mut sound, mut picture) = (0i64, 0i64);
            let mut batches = 0u64;
            loop {
                batches += 1;
                let mut count = self.batch;
                if self.stall_after == Some(batches) {
                    tokio::time::sleep(self.stall).await;
                }
                // What the camera's own clock says should be out by now.
                tokio::time::sleep_until(start + every * u32::try_from(batches).unwrap()).await;
                if self.stall_after == Some(batches) {
                    count += u64::try_from(self.stall.as_millis() / FRAME.as_millis()).unwrap();
                }
                sink.publish(Frame::new(0, picture, true, key.data.clone())).map_err(refused)?;
                picture += 3600;
                for _ in 0..count {
                    sink.publish(Frame::new(1, sound, false, Bytes::from_static(OPUS_SILENCE))).map_err(refused)?;
                    sound += 960;
                }
                if self.stall_after == Some(batches) {
                    // The camera is back on its clock: the batches the stall
                    // covered are out.
                    batches += u64::try_from(self.stall.as_millis() / every.as_millis()).unwrap();
                }
            }
        })
    }
}

struct Cameras(Camera);

impl SourceFactory for Cameras {
    fn schemes(&self) -> &[&'static str] {
        &["cam"]
    }

    fn build(&self, _spec: &SourceSpec, _ctx: &FactoryCtx) -> Result<Arc<dyn SourceDriver>, SpecError> {
        Ok(Arc::new(self.0.clone()))
    }
}

async fn watch(camera: Camera, playout: Playout) -> (Client, WebRtcServer, Arc<Hub>) {
    watch_with(camera, playout, browser(false)).await
}

async fn watch_with(camera: Camera, playout: Playout, offer: str0m::RtcConfig) -> (Client, WebRtcServer, Arc<Hub>) {
    let mut drivers = Drivers::new();
    drivers.register(Arc::new(Cameras(camera)));
    let hub = Hub::new(drivers, Limits::default());
    let stream = hub.upsert("cam", &["cam:one"]).unwrap();
    let config = WebRtcConfig {
        listen: "127.0.0.1:0".parse().unwrap(),
        answer_timeout: WAIT,
        playout,
        ..WebRtcConfig::default()
    };
    let server = WebRtcServer::bind(config, CancellationToken::new()).await.unwrap();
    let client = connect(&server, stream, offer).await.unwrap();
    (client, server, hub)
}

/// The arrival and the RTP time of each sound frame the client receives for
/// `window`, from the first one.
async fn sound(client: &mut Client, window: Duration) -> Vec<(Instant, u64)> {
    let mut frames: Vec<(Instant, u64)> = Vec::new();
    let deadline = tokio::time::Instant::now() + WAIT + window;
    while tokio::time::Instant::now() < deadline {
        let Ok(Some(event)) = tokio::time::timeout(Duration::from_secs(2), client.events.recv()).await else { break };
        if let ClientEvent::Media { kind: RtcKind::Audio, at, time, .. } = event {
            if frames.first().is_some_and(|(first, _)| at > *first + window) {
                break;
            }
            frames.push((at, time));
        }
    }
    frames
}

/// The middle value of how far each gap between two arrivals is from one
/// frame: a figure a slow machine's few long gaps do not move.
fn typical_unevenness(frames: &[(Instant, u64)]) -> Duration {
    let mut off: Vec<Duration> = frames.windows(2).map(|w| (w[1].0 - w[0].0).abs_diff(FRAME)).collect();
    off.sort();
    off[off.len() / 2]
}

const PAIRS: Camera = Camera { batch: 2, stall_after: None, stall: Duration::ZERO };

/// The sound paced (off by default since the review of W1b).
const PACED: Playout = Playout {
    pace_sound: true,
    sound_late_max: Some(oxrtc_webrtc::DEFAULT_SOUND_LATE_MAX),
    lip_sync: true,
    sound_lag: Duration::ZERO,
};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sound_delivered_in_pairs_arrives_one_frame_apart() {
    let (mut client, _server, _hub) = watch(PAIRS, PACED).await;
    let frames = sound(&mut client, Duration::from_secs(3)).await;
    assert!(frames.len() >= 100, "{} sound frames in 3 s", frames.len());
    // Pairs as they come are 0 ms and 40 ms apart: 20 ms from one frame,
    // every time. On its own clock the sound is one frame apart, to the
    // timer's precision: about a millisecond on Linux; on Windows the
    // default timer ticks every 15.6 ms (GitHub's runner: 9.9 ms typical),
    // which still halves what the pairs give.
    let uneven = typical_unevenness(&frames);
    let precision = Duration::from_millis(if cfg!(windows) { 14 } else { 8 });
    assert!(uneven < precision, "the sound's gaps are typically {uneven:?} from one frame");
}

/// The same camera with the pacing off, the default: what the test above
/// would see without the pacing (so that it is known to test it).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn without_its_clock_the_sound_arrives_as_the_pairs_it_came_in() {
    let (mut client, _server, _hub) = watch(PAIRS, Playout::default()).await;
    let frames = sound(&mut client, Duration::from_secs(3)).await;
    assert!(frames.len() >= 100, "{} sound frames in 3 s", frames.len());
    let uneven = typical_unevenness(&frames);
    assert!(uneven > Duration::from_millis(12), "pairs, yet the gaps are typically {uneven:?} from one frame");
}

/// A camera that stops for 1.2 s after its 50th pair (2 s), then delivers
/// the 60 frames the stall kept back at once.
const STALLING: Camera = Camera { batch: 2, stall_after: Some(50), stall: Duration::from_millis(1200) };

/// The RTP times missing between consecutive frames, in frames.
fn missing(frames: &[(Instant, u64)]) -> u64 {
    frames.windows(2).map(|w| (w[1].1 - w[0].1) / 960).filter(|steps| *steps > 1).map(|steps| steps - 1).sum()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_sound_a_stall_kept_back_is_not_sent_beyond_the_bound() {
    let bound = Duration::from_millis(200);
    let (mut client, _server, _hub) = watch(STALLING, Playout { sound_late_max: Some(bound), ..PACED }).await;
    let frames = sound(&mut client, Duration::from_secs(5)).await;
    // 1.2 s kept back, 200 ms of it fresh enough: about 50 frames not sent
    // (the session connects somewhere in the first two seconds, so the
    // stall is in what it receives).
    let lost = missing(&frames);
    assert!((40..=58).contains(&lost), "{lost} frames of sound not sent, of the 60 the stall kept back");
    // And no burst: never more than the bound's worth of sound in 100 ms.
    let most = frames
        .iter()
        .map(|(at, _)| frames.iter().filter(|(b, _)| b >= at && *b < *at + Duration::from_millis(100)).count())
        .max()
        .unwrap();
    assert!(most <= 20, "{most} sound frames within 100 ms: a burst");
}

/// The bound does not need the pacing: unpaced (the default), the sound
/// leaves as it came (pairs) and what a stall kept back beyond the bound is
/// still not sent.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_bound_holds_without_the_pacing() {
    let bound = Duration::from_millis(200);
    let playout = Playout { sound_late_max: Some(bound), ..Playout::default() };
    let (mut client, _server, _hub) = watch(STALLING, playout).await;
    let frames = sound(&mut client, Duration::from_secs(5)).await;
    let lost = missing(&frames);
    assert!((40..=58).contains(&lost), "{lost} frames of sound not sent, of the 60 the stall kept back");
    let uneven = typical_unevenness(&frames);
    assert!(uneven > Duration::from_millis(12), "unpaced, yet the gaps are typically {uneven:?} from one frame");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn without_a_bound_all_the_sound_of_a_stall_is_sent() {
    let (mut client, _server, _hub) = watch(STALLING, Playout { sound_late_max: None, ..PACED }).await;
    let frames = sound(&mut client, Duration::from_secs(5)).await;
    assert_eq!(missing(&frames), 0, "no bound: every frame is sent, late or not");
    let most = frames
        .iter()
        .map(|(at, _)| frames.iter().filter(|(b, _)| b >= at && *b < *at + Duration::from_millis(100)).count())
        .max()
        .unwrap();
    assert!(most >= 40, "{most} sound frames within 100 ms: the stall's sound did not come at once");
}

/// What the client receives for `window`: (pictures, pictures carrying a
/// playout delay of 0 to 0, sound frames carrying one).
async fn delays(client: &mut Client, window: Duration) -> (usize, usize, usize) {
    let (mut pictures, mut zero, mut sound) = (0, 0, 0);
    let deadline = tokio::time::Instant::now() + WAIT + window;
    while tokio::time::Instant::now() < deadline && pictures < 40 {
        let Ok(Some(event)) = tokio::time::timeout(Duration::from_secs(2), client.events.recv()).await else { break };
        match event {
            ClientEvent::Media { kind: RtcKind::Video, playout_delay, .. } => {
                pictures += 1;
                zero += usize::from(playout_delay == Some((0, 0)));
            }
            ClientEvent::Media { kind: RtcKind::Audio, playout_delay, .. } => {
                sound += usize::from(playout_delay.is_some())
            }
            _ => {}
        }
    }
    (pictures, zero, sound)
}

/// A browser's offer: Chromium offers `playout-delay` on its video line.
fn offering_playout_delay() -> str0m::RtcConfig {
    browser(false).set_extension(5, str0m::rtp::Extension::PlayoutDelay)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn lip_sync_off_asks_for_every_picture_to_be_shown_at_once() {
    let live = Playout { lip_sync: false, ..Playout::default() };
    let (mut client, _server, _hub) = watch_with(PAIRS, live, offering_playout_delay()).await;
    let (pictures, zero, sound) = delays(&mut client, Duration::from_secs(3)).await;
    assert!(pictures >= 20, "{pictures} pictures");
    assert_eq!(zero, pictures, "every picture carries a playout delay of 0 to 0");
    assert_eq!(sound, 0, "the sound carries none");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn lip_sync_on_by_default_sends_no_playout_delay() {
    let (mut client, _server, _hub) = watch_with(PAIRS, Playout::default(), offering_playout_delay()).await;
    let (pictures, zero, sound) = delays(&mut client, Duration::from_secs(3)).await;
    assert!(pictures >= 20, "{pictures} pictures");
    assert_eq!((zero, sound), (0, 0), "the extension is not negotiated: the browser holds the picture for the sound");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn lip_sync_off_with_a_client_that_does_not_offer_the_extension_still_plays() {
    let live = Playout { lip_sync: false, ..Playout::default() };
    let (mut client, _server, _hub) = watch_with(PAIRS, live, browser(false)).await;
    let (pictures, zero, _) = delays(&mut client, Duration::from_secs(3)).await;
    assert!(pictures >= 20, "{pictures} pictures");
    assert_eq!(zero, 0, "nothing the offer did not name");
}
