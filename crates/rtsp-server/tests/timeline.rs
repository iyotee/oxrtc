//! R20: a long-lived RTSP reader across a source whose timestamps start again
//! (a camera reopened, a sound transcode restarted). Its RTP clock must not
//! jump, and its picture and sound must stay where the sender reports put
//! them - openIVR's segmenter reads exactly this, and with go2rtc measured a
//! sound 9.4 hours ahead of its picture.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing, clippy::panic)]

mod common;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use bytes::Bytes;
use common::{Client, config, start};
use oxrtc_core::{
    BoxFuture, CancellationToken, Codec, CodecParams, Drivers, FactoryCtx, Frame, Hub, Limits, MediaKind, ProducerSink,
    SourceDriver, SourceError, SourceFactory, SourceSpec, SpecError, TrackInfo,
};

const TICK: Duration = Duration::from_millis(40);
/// Seconds between 1900 (NTP) and 1970 (Unix).
const NTP_UNIX_OFFSET: f64 = 2_208_988_800.0;

/// Plays its tracks at 25 frames a second, every run from pts 0, for
/// `frames` frames a run (then ends, and the hub restarts it).
struct Restarting {
    tracks: Vec<TrackInfo>,
    frames: usize,
    /// The list of every run after the first, when it differs.
    later: Option<Vec<TrackInfo>>,
    runs: std::sync::atomic::AtomicUsize,
}

impl SourceDriver for Restarting {
    fn run(
        self: Arc<Self>,
        sink: ProducerSink,
        cancel: CancellationToken,
    ) -> BoxFuture<'static, Result<(), SourceError>> {
        Box::pin(async move {
            let run = self.runs.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let tracks = match &self.later {
                Some(later) if run > 0 => later.clone(),
                _ => self.tracks.clone(),
            };
            sink.set_tracks(tracks.clone()).map_err(|e| SourceError::Refused(e.to_string()))?;
            let mut ticker = tokio::time::interval(TICK);
            for n in 0..self.frames {
                tokio::select! {
                    () = cancel.cancelled() => return Ok(()),
                    _ = ticker.tick() => {}
                }
                let n = n as i64;
                for (index, track) in tracks.iter().enumerate() {
                    let frame = match track.kind {
                        MediaKind::Video => {
                            let key = n % 25 == 0;
                            let data: &'static [u8] =
                                if key { &[0, 0, 0, 2, 0x65, 0x88] } else { &[0, 0, 0, 2, 0x41, 0x9a] };
                            Frame::new(index as u8, n * 3600, key, Bytes::from_static(data))
                        }
                        MediaKind::Audio => {
                            let step = i64::from(track.clock_rate / 25);
                            Frame::new(index as u8, n * step, true, Bytes::from_static(&[0xd5; 320]))
                        }
                    };
                    sink.publish(frame).map_err(|e| SourceError::Refused(e.to_string()))?;
                }
            }
            Ok(())
        })
    }
}

#[derive(Default)]
struct Restarts(Mutex<HashMap<String, Arc<Restarting>>>);

impl SourceFactory for Restarts {
    fn schemes(&self) -> &[&'static str] {
        &["restart"]
    }

    fn build(&self, spec: &SourceSpec, _ctx: &FactoryCtx) -> Result<Arc<dyn SourceDriver>, SpecError> {
        let driver = self.0.lock().unwrap().get(spec.target()).cloned();
        let driver = driver.ok_or_else(|| SpecError::Invalid("no such test source".into()))?;
        Ok(driver)
    }
}

fn video() -> TrackInfo {
    TrackInfo::video(Codec::H264).with_params(CodecParams::H264 {
        sps: Bytes::from_static(&[0x67, 0x64, 0x00, 0x1f, 0xac]),
        pps: Bytes::from_static(&[0x68, 0xee, 0x3c, 0xb0]),
    })
}

fn sound() -> TrackInfo {
    TrackInfo::audio(Codec::Pcma, 8000, 1)
}

fn hub(sources: Vec<(&str, Vec<TrackInfo>, usize)>) -> Arc<Hub> {
    hub_of(sources.into_iter().map(|(name, tracks, frames)| (name, tracks, frames, None)).collect())
}

/// A test source: its name, its tracks, its frames a run, and the tracks of
/// every run after the first when they differ.
type Source<'a> = (&'a str, Vec<TrackInfo>, usize, Option<Vec<TrackInfo>>);

fn hub_of(sources: Vec<Source<'_>>) -> Arc<Hub> {
    let factory = Arc::new(Restarts::default());
    for (name, tracks, frames, later) in sources {
        let driver = Restarting { tracks, frames, later, runs: Default::default() };
        factory.0.lock().unwrap().insert(name.to_owned(), Arc::new(driver));
    }
    let mut drivers = Drivers::new();
    drivers.register(factory);
    Hub::new(
        drivers,
        Limits {
            probe_timeout: Duration::from_secs(3),
            backoff_initial: Duration::from_millis(100),
            backoff_max: Duration::from_millis(200),
            ..Limits::default()
        },
    )
}

/// One RTP packet as the reader saw it.
struct Packet {
    arrival: f64,
    ts: u32,
    /// Its time by the track's last sender report, as FFmpeg computes it
    /// (`(int32_t)(timestamp - last_rtcp_timestamp)`): `None` before any.
    mapped: Option<f64>,
}

#[derive(Default)]
struct Track {
    rate: f64,
    report: Option<(f64, u32)>,
    packets: Vec<Packet>,
}

fn unix_now() -> f64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs_f64()
}

/// Plays `url`'s two tracks (video on 0-1, sound on 2-3) for `wait` and
/// returns what each track received.
async fn read(url: &str, addr: std::net::SocketAddr, wait: Duration) -> [Track; 2] {
    let mut c = Client::connect(addr).await;
    let d = c.request("DESCRIBE", url, &["Accept: application/sdp"], "").await.unwrap();
    assert_eq!(d.status, 200, "{d:?}");
    let base = d.header("Content-Base").unwrap();
    let mut session = String::new();
    for (track, channels) in [(0, "0-1"), (1, "2-3")] {
        let transport = format!("Transport: RTP/AVP/TCP;unicast;interleaved={channels}");
        let mut headers = vec![transport];
        if !session.is_empty() {
            headers.push(format!("Session: {session}"));
        }
        let headers: Vec<&str> = headers.iter().map(String::as_str).collect();
        let s = c.request("SETUP", &format!("{base}trackID={track}"), &headers, "").await.unwrap();
        assert_eq!(s.status, 200, "{s:?}");
        session = s.header("Session").unwrap().split(';').next().unwrap().to_owned();
    }
    let session = format!("Session: {session}");
    assert_eq!(c.request("PLAY", &base, &[session.as_str()], "").await.unwrap().status, 200);
    let mut tracks = [Track { rate: 90_000.0, ..Track::default() }, Track { rate: 8000.0, ..Track::default() }];
    let end = tokio::time::Instant::now() + wait;
    while tokio::time::Instant::now() < end {
        let Some((channel, payload)) = c.interleaved().await else { break };
        let track = &mut tracks[usize::from(channel / 2)];
        if channel % 2 == 1 {
            if payload.get(1) == Some(&200) {
                let secs = f64::from(u32::from_be_bytes(payload[8..12].try_into().unwrap()));
                let frac = f64::from(u32::from_be_bytes(payload[12..16].try_into().unwrap())) / 4_294_967_296.0;
                let rtp = u32::from_be_bytes(payload[16..20].try_into().unwrap());
                track.report = Some((secs + frac - NTP_UNIX_OFFSET, rtp));
            }
            continue;
        }
        let ts = u32::from_be_bytes(payload[4..8].try_into().unwrap());
        let mapped = track.report.map(|(ntp, rtp)| ntp + f64::from(ts.wrapping_sub(rtp) as i32) / track.rate);
        track.packets.push(Packet { arrival: unix_now(), ts, mapped });
    }
    tracks
}

fn step(track: &Track, pair: &[Packet]) -> f64 {
    f64::from(pair[1].ts.wrapping_sub(pair[0].ts) as i32) / track.rate
}

/// Each track's RTP clock never goes back, and never runs ahead of the wall
/// clock between two packets (one-sided, with room: a reader that stalls only
/// makes that wall time longer).
fn assert_no_jump(tracks: &[Track; 2]) {
    for (name, track) in ["video", "sound"].iter().zip(tracks) {
        assert!(track.packets.len() > 40, "{name}: {} packets", track.packets.len());
        for pair in track.packets.windows(2) {
            let (media, wall) = (step(track, pair), pair[1].arrival - pair[0].arrival);
            assert!(media > -0.001 && media < wall + 1.0, "{name}'s RTP clock jumped {media:+.3} s over {wall:.3} s");
        }
    }
}

/// When the restarting track came back: the arrival after its largest step.
fn restart_at(track: &Track) -> f64 {
    let pairs = track.packets.windows(2);
    let pair = pairs.max_by(|a, b| step(track, a).total_cmp(&step(track, b))).unwrap();
    pair[1].arrival
}

/// Picture and sound sit at the same distance from their arrivals by their
/// sender reports - one before each track's first packet - before the
/// restart and after it. The smallest distance of each track is the one
/// measured: a reader's stall only makes a packet late.
fn assert_aligned(tracks: &[Track; 2], restarting: &Track) {
    let at = restart_at(restarting);
    for (window, after) in [("before", false), ("after", true)] {
        let keep = |t: f64| if after { t >= at } else { t < at - 0.1 };
        let lag = |name: &str, track: &Track| {
            let lags =
                track.packets.iter().filter(|p| keep(p.arrival)).map(|p| {
                    p.arrival - p.mapped.unwrap_or_else(|| panic!("{name}: a packet before any sender report"))
                });
            lags.fold(f64::MAX, f64::min)
        };
        let (video, sound) = (lag("video", &tracks[0]), lag("sound", &tracks[1]));
        assert!(video < 1.0 && sound < 1.0, "{window} the restart: no packet");
        assert!((video - sound).abs() < 0.1, "{window} the restart, picture {video:+.3} s and sound {sound:+.3} s");
    }
}

#[tokio::test]
async fn a_source_that_starts_its_timestamps_again_moves_no_reader_clock() {
    // 1.6 s, then pts from 0 again: a camera reopened under a recording.
    let hub = hub(vec![("cam", vec![video(), sound()], 40)]);
    hub.upsert("cam", &["restart:cam"]).unwrap();
    let server = start(hub, config()).await;
    let tracks = read(&server.url("cam"), server.addr, Duration::from_millis(3200)).await;
    assert_no_jump(&tracks);
    assert_aligned(&tracks, &tracks[0]);
}

#[tokio::test]
async fn a_sound_transcode_that_restarts_stays_with_its_picture() {
    // openIVR's layout: the picture from the camera, the sound from a
    // transcode that restarts (pts from 0) while the picture runs on.
    let hub = hub(vec![("picture", vec![video()], usize::MAX), ("sound", vec![sound()], 40)]);
    hub.upsert("cam", &["restart:picture", "restart:sound"]).unwrap();
    let server = start(hub, config()).await;
    let tracks = read(&server.url("cam"), server.addr, Duration::from_millis(3200)).await;
    assert_no_jump(&tracks);
    assert_aligned(&tracks, &tracks[1]);
}

#[tokio::test]
async fn a_sound_whose_clock_rate_changes_ends_the_reading() {
    // The camera comes back with its sound at 16 kHz instead of 8: the
    // reader was described 8 kHz, and the timeline's 16 kHz times read at
    // 8 kHz run twice as fast and start twice as far. It must describe again.
    let later = vec![video(), TrackInfo::audio(Codec::Pcma, 16_000, 1)];
    let hub = hub_of(vec![("cam", vec![video(), sound()], 20, Some(later))]);
    hub.upsert("cam", &["restart:cam"]).unwrap();
    let server = start(hub, config()).await;
    let started = tokio::time::Instant::now();
    let tracks = read(&server.url("cam"), server.addr, Duration::from_millis(4000)).await;
    assert!(started.elapsed() < Duration::from_millis(3000), "the reading went on, at the wrong clock rate");
    assert!(tracks[1].packets.len() >= 10, "the first run was read: {}", tracks[1].packets.len());
}
