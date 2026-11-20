//! The sound is dated a little later than the picture that arrived with it
//! (D30, roadmap W1c), on the wire: a browser synchronises a session's two
//! tracks from their RTCP sender reports, each saying "this RTP time is this
//! wall clock", and holds the picture until the sound of the same date
//! plays. With `Playout::sound_lag`, a picture and a sound frame the camera
//! delivered at the same instant are dated that far apart by their tracks'
//! reports, the sound the later; with zero, or lip sync off, at the same
//! instant.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing, clippy::panic)]

mod common;

use std::sync::Arc;
use std::time::{Duration, Instant, UNIX_EPOCH};

use bytes::Bytes;
use common::*;
use oxrtc_core::{
    BoxFuture, CancellationToken, Codec, Drivers, FactoryCtx, Frame, Hub, Limits, ProducerSink, SourceDriver,
    SourceError, SourceFactory, SourceSpec, SpecError, TrackInfo,
};
use oxrtc_webrtc::{Playout, WebRtcConfig, WebRtcServer};
use str0m::media::MediaKind as RtcKind;

const OPUS_SILENCE: &[u8] = &[0xf8, 0xff, 0xfe];

/// A camera whose picture and sound are filmed together: every 20 ms a
/// keyframe and a sound frame, published at the same instant, each with that
/// instant as its time (so a late timer moves both alike).
struct Camera;

fn refused(error: impl std::fmt::Display) -> SourceError {
    SourceError::Refused(error.to_string())
}

impl SourceDriver for Camera {
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
            let start = tokio::time::Instant::now();
            let mut ticker = tokio::time::interval(Duration::from_millis(20));
            loop {
                ticker.tick().await;
                let micros = i64::try_from(start.elapsed().as_micros()).unwrap();
                sink.publish(Frame::new(0, micros * 90 / 1000, true, key.data.clone())).map_err(refused)?;
                sink.publish(Frame::new(1, micros * 48 / 1000, false, Bytes::from_static(OPUS_SILENCE)))
                    .map_err(refused)?;
            }
        })
    }
}

struct Cameras;

impl SourceFactory for Cameras {
    fn schemes(&self) -> &[&'static str] {
        &["cam"]
    }

    fn build(&self, _spec: &SourceSpec, _ctx: &FactoryCtx) -> Result<Arc<dyn SourceDriver>, SpecError> {
        Ok(Arc::new(Camera))
    }
}

/// A frame as a client knows it: when it arrived, and the wall clock its
/// track's last sender report gives its RTP time, in seconds.
struct Dated {
    at: Instant,
    date: f64,
}

/// How much later than its picture the sound is dated, in milliseconds: the
/// middle value over the pairs that arrived within 5 ms of each other, once
/// both tracks had a sender report (the sound's comes every 5 s).
async fn sound_dated_after_the_picture(playout: Playout) -> f64 {
    let mut drivers = Drivers::new();
    drivers.register(Arc::new(Cameras));
    let hub = Hub::new(drivers, Limits::default());
    let stream = hub.upsert("cam", &["cam:one"]).unwrap();
    let config = WebRtcConfig {
        listen: "127.0.0.1:0".parse().unwrap(),
        answer_timeout: WAIT,
        playout,
        ..WebRtcConfig::default()
    };
    let server = WebRtcServer::bind(config, CancellationToken::new()).await.unwrap();
    let mut client = connect(&server, stream, browser(false)).await.unwrap();
    let (mut pictures, mut sounds): (Vec<Dated>, Vec<Dated>) = (Vec::new(), Vec::new());
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    while sounds.len() < 60 && tokio::time::Instant::now() < deadline {
        let Ok(Some(event)) = tokio::time::timeout(Duration::from_secs(2), client.events.recv()).await else { break };
        let ClientEvent::Media { kind, time, at, sender: Some((wall, rtp)), .. } = event else { continue };
        let rate = if kind == RtcKind::Video { 90_000.0 } else { 48_000.0 };
        let since_report = f64::from((time as u32).wrapping_sub(rtp) as i32) / rate;
        let dated = Dated { at, date: wall.duration_since(UNIX_EPOCH).unwrap().as_secs_f64() + since_report };
        if kind == RtcKind::Video { pictures.push(dated) } else { sounds.push(dated) }
    }
    let mut lags: Vec<f64> = sounds
        .iter()
        .filter_map(|sound| {
            let apart = |picture: &Dated| picture.at.max(sound.at).duration_since(picture.at.min(sound.at));
            let picture = pictures.iter().min_by_key(|picture| apart(picture))?;
            (apart(picture) < Duration::from_millis(5)).then_some((sound.date - picture.date) * 1000.0)
        })
        .collect();
    assert!(
        lags.len() >= 30,
        "{} pairs dated by both tracks' sender reports ({} sound frames)",
        lags.len(),
        sounds.len()
    );
    lags.sort_by(f64::total_cmp);
    lags[lags.len() / 2]
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_sound_is_dated_after_its_picture_by_the_lag_and_no_more() {
    let lag = |ms| Playout { sound_lag: Duration::from_millis(ms), ..Playout::default() };
    let live = Playout { lip_sync: false, ..lag(30) };
    let (none, thirty, most, off) = tokio::join!(
        sound_dated_after_the_picture(lag(0)),
        sound_dated_after_the_picture(lag(30)),
        // More than the owner allowed is never given: 40 ms, whatever is asked.
        sound_dated_after_the_picture(lag(200)),
        sound_dated_after_the_picture(live),
    );
    assert!(none.abs() < 3.0, "no lag: the sound and the picture dated together, got {none:+.1} ms");
    assert!((thirty - 30.0).abs() < 3.0, "30 ms asked: the sound dated {thirty:+.1} ms after its picture");
    assert!((most - 40.0).abs() < 3.0, "200 ms asked: the sound dated {most:+.1} ms after its picture");
    assert!(off.abs() < 3.0, "lip sync off: nothing to move, got {off:+.1} ms");
}

/// The default is the owner's answer of 2026-11-19: 20 to 40 ms.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn by_default_the_sound_is_dated_20_to_40_ms_after_its_picture() {
    let asked = Playout::default().sound_lag.as_secs_f64() * 1000.0;
    assert!((20.0..=40.0).contains(&asked), "{asked} ms");
    let dated = sound_dated_after_the_picture(Playout::default()).await;
    assert!((dated - asked).abs() < 3.0, "{asked} ms by default: the sound dated {dated:+.1} ms after its picture");
}
