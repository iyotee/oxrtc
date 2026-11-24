//! One HLS client's session (go2rtc's `id=`): its feed, written into its
//! window by a task of its own, and what its requests wait for. The session
//! ends after [`Session::idle`] without a request, when its feed fails, or
//! when the server stops; its subscription ends with it.

use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, SystemTime};

use bytes::Bytes;
use oxrtc_core::memory::Held;
use oxrtc_core::{CancellationToken, PerIpPermit};
use tokio::sync::watch;
use tokio::time::Instant;

use super::playlist::Kind;
use super::window::{Cut, Settings, Window};
use crate::fmp4::container::Container;
use crate::fmp4::muxer::Out;
use crate::fmp4::{FeedOf, Step};

/// go2rtc's `keepalive`: a session no request has touched for this long
/// ends (longer for long segments: [`Session::idle`]).
pub const IDLE: Duration = Duration::from_secs(5);

/// What the session's task and its requests share.
#[derive(Debug)]
pub(crate) struct Shared {
    pub window: Window,
    /// The current init segment (fMP4); the window keeps those its segments
    /// are read with.
    pub init: Option<Bytes>,
    /// The tracks' codec strings, once known.
    pub codecs: Option<Vec<String>>,
    /// The wall-clock time of the session's zero.
    pub zero: Option<SystemTime>,
    /// Why it ended, once it has.
    pub ended: Option<String>,
}

/// One session.
#[derive(Debug)]
pub(crate) struct Session {
    pub id: String,
    pub kind: Kind,
    pub low_latency: bool,
    pub stream: String,
    shared: Mutex<Shared>,
    changed: watch::Sender<u64>,
    touched: Mutex<Instant>,
    idle: Duration,
    stop: CancellationToken,
    _address: PerIpPermit,
}

impl Session {
    pub(crate) fn new(
        id: String,
        kind: Kind,
        stream: String,
        address: PerIpPermit,
        settings: Settings,
        idle: Duration,
    ) -> Self {
        Self {
            id,
            kind,
            low_latency: true,
            stream,
            shared: Mutex::new(Shared {
                window: Window::new(settings),
                init: None,
                codecs: None,
                zero: None,
                ended: None,
            }),
            changed: watch::Sender::new(0),
            touched: Mutex::new(Instant::now()),
            idle,
            stop: CancellationToken::new(),
            _address: address,
        }
    }

    /// Reads the shared state.
    pub(crate) fn read<T>(&self, f: impl FnOnce(&Shared) -> T) -> T {
        f(&self.shared.lock().unwrap_or_else(PoisonError::into_inner))
    }

    fn write<T>(&self, f: impl FnOnce(&mut Shared) -> T) -> T {
        let out = f(&mut self.shared.lock().unwrap_or_else(PoisonError::into_inner));
        self.changed.send_modify(|v| *v = v.wrapping_add(1));
        out
    }

    /// A request came: the session lives on.
    pub(crate) fn touch(&self) {
        *self.touched.lock().unwrap_or_else(PoisonError::into_inner) = Instant::now();
    }

    /// How long it lives without a request: go2rtc's 5 s (`HlsLimits::idle`), or three of its
    /// longest segments (a player reloads its playlist once per target
    /// duration: a camera's 6.4 s groups of pictures make 6.4 s segments).
    pub(crate) fn idle(&self) -> Duration {
        self.idle.max(self.read(|s| s.window.longest()) * 3)
    }

    /// Ends it (the server's stop, its removal).
    pub(crate) fn end(&self) {
        self.stop.cancel();
        self.changed.send_modify(|v| *v = v.wrapping_add(1));
    }

    /// Whether it has ended.
    pub(crate) fn ended(&self) -> bool {
        self.stop.is_cancelled() || self.read(|s| s.ended.is_some())
    }

    /// Waits until `ready` holds on the shared state, the session ends or
    /// `limit` passes; whether `ready` holds.
    pub(crate) async fn wait(&self, limit: Duration, ready: impl Fn(&Shared) -> bool) -> bool {
        let mut changes = self.changed.subscribe();
        let deadline = Instant::now() + limit;
        loop {
            if self.read(&ready) {
                return true;
            }
            if self.ended() {
                return false;
            }
            tokio::select! {
                () = tokio::time::sleep_until(deadline) => return self.read(&ready),
                changed = changes.changed() => if changed.is_err() { return false },
            }
        }
    }

    /// The task that writes the feed into the window, until the session
    /// ends: idle, its feed failed, `shutdown`, or [`Session::end`].
    pub(crate) async fn run<C: Container>(
        self: Arc<Self>,
        mut feed: FeedOf<C>,
        epoch_wall: SystemTime,
        shutdown: CancellationToken,
        budget: Option<Held>,
    ) {
        let mut held = budget;
        let why = loop {
            let deadline = *self.touched.lock().unwrap_or_else(PoisonError::into_inner) + self.idle();
            let step = tokio::select! {
                () = shutdown.cancelled() => break "the server is stopping".to_owned(),
                () = self.stop.cancelled() => break "ended".to_owned(),
                () = tokio::time::sleep_until(deadline) => {
                    if Instant::now() >= *self.touched.lock().unwrap_or_else(PoisonError::into_inner) + self.idle() {
                        break "no request for too long".to_owned();
                    }
                    continue;
                }
                step = feed.next() => step,
            };
            match step {
                Step::Failed(reason) => break reason,
                Step::Send(outs) => {
                    for out in outs {
                        self.take(&mut feed, out, epoch_wall, held.as_mut());
                    }
                }
            }
        };
        tracing::debug!(stream = %self.stream, "api: hls session ended: {why}");
        self.write(|s| s.ended = Some(why));
        self.end();
    }

    fn take<C: Container>(&self, feed: &mut FeedOf<C>, out: Out, epoch_wall: SystemTime, held: Option<&mut Held>) {
        match out {
            Out::Mime(_) => {}
            Out::Init(init) => {
                let muxer = feed.muxer();
                let (codecs, video) = (muxer.codecs(), muxer.has_video());
                self.write(|s| {
                    s.init = Some(init).filter(|i| !i.is_empty());
                    s.window.new_init(video, s.init.clone());
                    s.codecs = Some(codecs);
                });
            }
            Out::Binary(bytes) => {
                let muxer = feed.muxer();
                let (lead, key, at) = (muxer.last_lead, muxer.last_key, muxer.latest);
                let zero = muxer.zero().map(|(pts, clock)| {
                    epoch_wall + Duration::from_secs_f64(pts.max(0) as f64 / f64::from(clock.max(1)))
                });
                let cut = self.read(|s| s.window.cut(lead, key, at));
                // A TS segment, and a TS part a player may start at, begins
                // with the program's tables.
                let mut chunks = Vec::with_capacity(2);
                if cut == Cut::Segment || (cut == Cut::Part && key) {
                    chunks.extend(muxer.tables());
                }
                chunks.push(bytes);
                let share = held.as_ref().map_or(usize::MAX, |h| h.share());
                let size = self.write(|s| {
                    let zero = *s.zero.get_or_insert(zero.unwrap_or(epoch_wall));
                    s.window.push(chunks, lead, key, at, zero + at);
                    s.window.evict(share);
                    s.window.size()
                });
                if let Some(held) = held {
                    held.set(size);
                }
            }
        }
    }
}
