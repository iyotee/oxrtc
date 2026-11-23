//! A scripted driver for the core's integration tests.
#![allow(dead_code, missing_docs)]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing, clippy::panic)]

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bytes::Bytes;
use oxrtc_core::{
    BoxFuture, CancellationToken, Codec, Declared, Drivers, FactoryCtx, Frame, Hub, Limits, MediaKind, ProducerSink,
    SourceDriver, SourceError, SourceFactory, SourceSpec, SpecError, TrackInfo,
};

pub const KEY: &[u8] = &[0, 0, 0, 2, 0x65, 0x88];
pub const DELTA: &[u8] = &[0, 0, 0, 2, 0x41, 0x9a];

#[derive(Clone)]
pub enum After {
    Hang,
    End,
}

#[derive(Clone)]
pub struct Script {
    pub declared: Option<Declared>,
    pub tracks: Vec<TrackInfo>,
    /// (track, keyframe) per frame; video tracks get AVCC payloads.
    pub frames: Vec<(u8, bool)>,
    pub interval: Duration,
    pub fail_first: usize,
    /// How long a run takes before it knows its tracks (a transcode probing
    /// a slow camera's picture); a run that fails does so after it.
    pub start_delay: Duration,
    /// Runs (counted from 1) that fail as well: an outage after a run that ended.
    pub outage: std::ops::Range<usize>,
    pub after: After,
    pub stall_timeout: Option<Duration>,
    /// What the driver gives itself to deliver a first frame, beyond the
    /// limits' connect timeout.
    pub start_timeout: Option<Duration>,
    /// Plays a source that reads this server (an FFmpeg of a relay).
    pub local: bool,
    /// The scripted failures are refused credentials.
    pub auth: bool,
    /// The scripted failures are the peer declining the request (a 4XX).
    pub declined: bool,
    /// A refused attempt costs nothing (a protocol client, not a process).
    pub cheap: bool,
    /// Its runs transcode (an `ffmpeg:` source): stopped after the
    /// transcode grace, not the camera's.
    pub transcodes: bool,
    /// From this frame on, frames come this far apart (a camera whose rate
    /// drops at night).
    pub slow_after: Option<(usize, Duration)>,
    /// Before this frame, the source holds back for this long, then goes on
    /// (a camera that pauses and resumes by itself).
    pub pause_at: Option<(usize, Duration)>,
}

impl Script {
    pub fn video(codec: Codec, frames: Vec<(u8, bool)>) -> Self {
        Self {
            declared: None,
            tracks: vec![TrackInfo::video(codec)],
            frames,
            interval: Duration::from_millis(40),
            fail_first: 0,
            start_delay: Duration::ZERO,
            outage: 0..0,
            after: After::Hang,
            stall_timeout: None,
            start_timeout: None,
            local: false,
            auth: false,
            declined: false,
            cheap: false,
            transcodes: false,
            slow_after: None,
            pause_at: None,
        }
    }

    /// A GOP of `n` frames on track 0, keyframe first.
    pub fn gop(n: usize) -> Vec<(u8, bool)> {
        (0..n).map(|i| (0, i == 0)).collect()
    }
}

pub struct Fake {
    pub script: Script,
    pub runs: AtomicUsize,
    pub active: AtomicUsize,
    /// While set, every run fails at once (after `start_delay`): a camera down.
    pub down: AtomicBool,
    /// When each run that delivered published its first frame.
    pub first_frames: Mutex<Vec<tokio::time::Instant>>,
}

impl Fake {
    pub fn new(script: Script) -> Arc<Self> {
        Arc::new(Self {
            script,
            runs: AtomicUsize::new(0),
            active: AtomicUsize::new(0),
            down: AtomicBool::new(false),
            first_frames: Mutex::new(Vec::new()),
        })
    }
    pub fn set_down(&self, down: bool) {
        self.down.store(down, Ordering::SeqCst);
    }
    pub fn first_frames(&self) -> Vec<tokio::time::Instant> {
        self.first_frames.lock().unwrap().clone()
    }
    pub fn runs(&self) -> usize {
        self.runs.load(Ordering::SeqCst)
    }
    pub fn active(&self) -> usize {
        self.active.load(Ordering::SeqCst)
    }
}

struct ActiveGuard<'a>(&'a AtomicUsize);
impl Drop for ActiveGuard<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

pub struct FakeDriver(Arc<Fake>);

impl SourceDriver for FakeDriver {
    fn declared(&self) -> Option<Declared> {
        self.0.script.declared.clone()
    }

    fn stall_timeout(&self) -> Option<Duration> {
        self.0.script.stall_timeout
    }

    fn start_timeout(&self) -> Option<Duration> {
        self.0.script.start_timeout
    }

    fn local(&self) -> bool {
        self.0.script.local
    }

    fn cheap_attempts(&self) -> bool {
        self.0.script.cheap
    }

    fn transcodes(&self) -> bool {
        self.0.script.transcodes
    }

    fn run(
        self: Arc<Self>,
        sink: ProducerSink,
        _cancel: CancellationToken,
    ) -> BoxFuture<'static, Result<(), SourceError>> {
        Box::pin(async move {
            let fake = &self.0;
            let run = fake.runs.fetch_add(1, Ordering::SeqCst) + 1;
            fake.active.fetch_add(1, Ordering::SeqCst);
            let _active = ActiveGuard(&fake.active);
            tokio::time::sleep(fake.script.start_delay).await;
            let down = fake.down.load(Ordering::SeqCst);
            if down || run <= fake.script.fail_first || fake.script.outage.contains(&run) {
                if fake.script.auth {
                    return Err(SourceError::Auth);
                }
                if fake.script.declined {
                    return Err(SourceError::Declined(format!("scripted refusal {run}")));
                }
                return Err(SourceError::Unreachable(format!("scripted failure {run}")));
            }
            sink.set_tracks(fake.script.tracks.clone()).map_err(|e| SourceError::Refused(e.to_string()))?;
            for (pts, (track, key)) in fake.script.frames.iter().enumerate() {
                let mut interval = match fake.script.slow_after {
                    Some((from, slow)) if pts >= from => slow,
                    _ => fake.script.interval,
                };
                if let Some((at, pause)) = fake.script.pause_at
                    && pts == at
                {
                    interval += pause;
                }
                tokio::time::sleep(interval).await;
                if pts == 0 {
                    fake.first_frames.lock().unwrap().push(tokio::time::Instant::now());
                }
                let kind = fake.script.tracks[usize::from(*track)].kind;
                let data = match (kind, key) {
                    (MediaKind::Video, true) => Bytes::from_static(KEY),
                    (MediaKind::Video, false) => Bytes::from_static(DELTA),
                    (MediaKind::Audio, _) => Bytes::from_static(&[0xd5; 160]),
                };
                sink.publish(Frame::new(*track, pts as i64 * 3600, *key, data))
                    .map_err(|e| SourceError::Refused(e.to_string()))?;
            }
            // With an outage, the run before it ends and the one after it holds.
            let ends = match fake.script.after {
                After::End => fake.script.outage.is_empty() || run < fake.script.outage.start,
                After::Hang => false,
            };
            if ends { Ok(()) } else { std::future::pending().await }
        })
    }
}

#[derive(Default)]
pub struct FakeFactory {
    fakes: Mutex<HashMap<String, Arc<Fake>>>,
}

impl FakeFactory {
    pub fn add(&self, name: &str, script: Script) -> Arc<Fake> {
        let fake = Fake::new(script);
        self.fakes.lock().unwrap().insert(name.to_owned(), fake.clone());
        fake
    }
}

impl SourceFactory for FakeFactory {
    fn schemes(&self) -> &[&'static str] {
        &["fake"]
    }
    fn build(&self, spec: &SourceSpec, _ctx: &FactoryCtx) -> Result<Arc<dyn SourceDriver>, SpecError> {
        let fake =
            self.fakes.lock().unwrap().get(spec.target()).cloned().ok_or(SpecError::Invalid("no fake".into()))?;
        Ok(Arc::new(FakeDriver(fake)))
    }
}

pub fn limits() -> Limits {
    Limits {
        idle_grace: Duration::from_secs(1),
        probe_timeout: Duration::from_secs(2),
        connect_timeout: Duration::from_secs(3),
        stall_timeout: Duration::from_secs(3),
        backoff_initial: Duration::from_millis(100),
        backoff_max: Duration::from_secs(1),
        ..Limits::default()
    }
}

pub fn hub_with(factory: Arc<FakeFactory>, limits: Limits) -> Arc<Hub> {
    let mut drivers = Drivers::new();
    drivers.register(factory);
    Hub::new(drivers, limits)
}

/// Every log line written on this thread while the guard lives.
#[derive(Clone, Default)]
pub struct Logs(Arc<Mutex<Vec<u8>>>);

impl Logs {
    pub fn capture() -> (Self, tracing::subscriber::DefaultGuard) {
        let logs = Self::default();
        let writer = logs.clone();
        let subscriber = tracing_subscriber::fmt().with_ansi(false).with_writer(move || writer.clone()).finish();
        // A second dispatcher, as `crates/core/src/channel_tests.rs` explains:
        // a callsite another test's thread reaches first must not stay
        // disabled for this one.
        static SECOND: std::sync::OnceLock<tracing::Dispatch> = std::sync::OnceLock::new();
        SECOND.get_or_init(|| tracing::Dispatch::new(tracing::subscriber::NoSubscriber::default()));
        let guard = tracing::subscriber::set_default(subscriber);
        tracing::callsite::rebuild_interest_cache();
        (logs, guard)
    }

    pub fn lines(&self, needle: &str) -> Vec<String> {
        let text = String::from_utf8_lossy(&self.0.lock().unwrap()).into_owned();
        text.lines().filter(|l| l.contains(needle)).map(str::to_owned).collect()
    }
}

impl std::io::Write for Logs {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
