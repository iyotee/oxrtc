//! A consumer's view of a stream: one merged, ordered sequence of events.

use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use futures_util::stream::{SelectAll, StreamExt};
use tokio::time::Instant;

use crate::channel::Item;
use crate::driver::BoxFuture;
use crate::error::{SubEnd, SubscribeError};
use crate::frame::Frame;
use crate::media::{MediaKind, TrackInfo};
use crate::queue::ConsumerClass;
use crate::refusals::Refusals;
use crate::route::{Input, Route, watch_upstream};
use crate::select::Pick;
use crate::stream::{ConsumerTag, Registration, Stream};
use crate::want::Want;

/// What a subscription delivers.
#[derive(Debug, Clone)]
pub enum Event {
    /// The tracks of the subscription: video first, then audio. Always the
    /// first event; sent again whenever a source reconnects with different
    /// tracks. Frame track indexes refer to the latest list.
    Tracks(Arc<[TrackInfo]>),
    /// A frame of track `track` of the latest list.
    Frame {
        /// Index in the latest track list.
        track: usize,
        /// The frame; its own `track` field is the source's index, ignore it.
        frame: Arc<Frame>,
    },
    /// Frames were lost (the consumer fell behind its queue's budget,
    /// [`crate::queue`]); video resumes at a keyframe.
    Discontinuity,
}

/// How often, until the first track list, the sources of routes still
/// unresolved are checked for a failed attempt.
const FAILURE_CHECK: Duration = Duration::from_millis(100);

/// Shortest time between two warnings of one consumer's dropped frames: one
/// that keeps falling behind overflows about once per group of pictures.
pub const LAG_REPORT_EVERY: Duration = Duration::from_secs(10);

type Reselection = BoxFuture<'static, Result<Vec<Pick>, SubscribeError>>;

/// A subscription to a stream. Dropping it releases the sources it holds.
pub struct Subscription {
    stream: Arc<Stream>,
    inputs: SelectAll<Input>,
    routes: Vec<Route>,
    next_route: u64,
    tracks: Option<Arc<[TrackInfo]>>,
    pending: VecDeque<Event>,
    backlog: VecDeque<(u64, Arc<Frame>)>,
    deadline: Option<Instant>,
    /// Where `deadline` goes, once, when it passes with no track resolved and
    /// a source still starting ([`crate::limits::Limits::first_track_timeout`]).
    hard_deadline: Instant,
    /// `deadline` was moved to `hard_deadline`.
    extended: bool,
    /// The next failure check; a field, since frames wake the loop far more
    /// often than the check's period and would otherwise push it back forever.
    next_check: Instant,
    /// Fires when a stream a route was resolved through (an alias's) closes.
    upstream: Option<BoxFuture<'static, ()>>,
    /// The media of the routes that closed with it, being resolved again.
    lost: Want,
    reselection: Option<Reselection>,
    registration: Registration,
    /// Who reads, for its queue budget and the log.
    tag: ConsumerTag,
    /// Overflows of its queues, reported at most once per
    /// [`LAG_REPORT_EVERY`]; and the frames they dropped since the last
    /// report.
    lags: Refusals,
    unreported_drops: u64,
}

impl std::fmt::Debug for Subscription {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Subscription").field("stream", &self.stream.name()).field("id", &self.registration.id).finish()
    }
}

impl Subscription {
    pub(crate) fn new(
        stream: Arc<Stream>,
        picks: Vec<Pick>,
        registration: Registration,
        deadline: Instant,
        tag: ConsumerTag,
    ) -> Self {
        let (stream_probe, stream_first) = (stream.limits.probe_timeout, stream.limits.first_track_timeout);
        let mut subscription = Self {
            stream,
            inputs: SelectAll::new(),
            routes: Vec::with_capacity(picks.len()),
            next_route: 0,
            tracks: None,
            pending: VecDeque::new(),
            backlog: VecDeque::new(),
            deadline: Some(deadline),
            hard_deadline: deadline
                .checked_sub(stream_probe)
                .and_then(|start| start.checked_add(stream_first))
                .unwrap_or(deadline),
            extended: false,
            next_check: Instant::now() + FAILURE_CHECK,
            upstream: None,
            lost: Want::default(),
            reselection: None,
            registration,
            tag,
            lags: Refusals::every(LAG_REPORT_EVERY),
            unreported_drops: 0,
        };
        subscription.add_routes(picks);
        subscription
    }

    fn add_routes(&mut self, picks: Vec<Pick>) {
        let budget = self.stream.limits.queue_budget(self.tag.class);
        for pick in picks {
            let (route, input) = Route::attach(self.next_route, pick, budget);
            self.next_route += 1;
            self.inputs.push(input);
            self.routes.push(route);
        }
        self.upstream = watch_upstream(&self.routes);
    }

    /// The stream this subscription reads.
    pub fn stream(&self) -> &Arc<Stream> {
        &self.stream
    }

    /// The latest track list, once the first [`Event::Tracks`] was delivered.
    pub fn tracks(&self) -> Option<&Arc<[TrackInfo]>> {
        self.tracks.as_ref()
    }

    /// The cached group of pictures of the video this subscription reads,
    /// from its keyframe to the latest frame published, as frames of its
    /// track list (their `pts` on the timeline): what a viewer whose decoder
    /// lost its state (a keyframe request) can be sent again. Empty before
    /// the first track list, and when nothing is cached (no keyframe yet, or
    /// a group larger than `gop_cache_bytes`).
    pub fn cached_video(&self) -> Vec<(usize, Arc<Frame>)> {
        let mut out = Vec::new();
        if self.tracks.is_none() {
            return out;
        }
        for route in &self.routes {
            for frame in route.slot.channel.gop() {
                let source_track = usize::from(frame.track);
                if !route.is_video(source_track) {
                    continue;
                }
                if let Some(&Some(track)) = route.map.get(source_track) {
                    out.push((track, frame));
                }
            }
        }
        out
    }

    /// The consumer was sent the cached group of pictures
    /// ([`Subscription::cached_video`]) and decodes video from there: after
    /// a loss, video no longer waits for a keyframe.
    pub fn video_resumed(&mut self) {
        for route in &mut self.routes {
            route.need_keyframe = false;
        }
    }

    /// The next event. Cancel-safe.
    pub async fn recv(&mut self) -> Result<Event, SubEnd> {
        let closed = self.stream.closed.clone();
        loop {
            if let Some(event) = self.pending.pop_front() {
                self.account(&event);
                return Ok(event);
            }
            // The deadline passed and nothing was resolved: no track list
            // will be sent (a later one would only follow a first).
            if self.deadline.is_none() && self.tracks.is_none() {
                return Err(SubEnd::NoTracks);
            }
            if self.routes.is_empty() && self.reselection.is_none() {
                return Err(SubEnd::Closed);
            }
            let deadline = self.deadline;
            let check = deadline.map(|at| at.min(self.next_check));
            let next = tokio::select! {
                biased;
                () = closed.cancelled() => return Err(SubEnd::Closed),
                () = sleep_until(check) => {
                    self.drain_ready();
                    if deadline.is_some_and(|at| Instant::now() >= at) {
                        self.deadline = None;
                        self.flush_partial();
                        self.extend_for_starting_sources();
                    } else {
                        self.next_check = Instant::now() + FAILURE_CHECK;
                        self.give_up_failed();
                    }
                    continue;
                }
                () = poll_held(&mut self.upstream) => {
                    self.reroute();
                    continue;
                }
                picks = poll_held(&mut self.reselection) => {
                    self.reselection = None;
                    match picks {
                        Ok(picks) => {
                            self.lost = Want::default();
                            self.add_routes(picks);
                        }
                        // Removed, or nothing in its successor serves, and
                        // nothing else is read: the consumer is told, as for
                        // its own stream.
                        Err(error) if self.routes.is_empty() => {
                            tracing::debug!(stream = self.stream.name(), %error, "a stream read through an alias is gone");
                            return Err(SubEnd::Closed);
                        }
                        // The other media go on (a picture of this stream's
                        // own never was the lost sound's), and the lost ones
                        // are asked for again, as a source is retried: the
                        // stream lending them may come back.
                        Err(error) => {
                            tracing::debug!(stream = self.stream.name(), %error, "a stream read through an alias is gone; asking again later");
                            self.reselect(self.stream.limits.backoff_max);
                        }
                    }
                    continue;
                }
                item = self.inputs.next(), if !self.inputs.is_empty() => item,
            };
            // An input ends only when its route was dropped.
            if let Some((route, item)) = next {
                self.handle(route, item);
            }
        }
    }

    /// Drops the routes resolved through a stream that closed (replaced or
    /// removed), and resolves their media again in this stream: an alias then
    /// reads the stream now under that name.
    fn reroute(&mut self) {
        let lost = &mut self.lost;
        self.routes.retain(|route| {
            if !route.upstream_closed() {
                return true;
            }
            for (kind, wish) in &route.kinds {
                match kind {
                    MediaKind::Video => lost.video = wish.clone(),
                    MediaKind::Audio => lost.audio = wish.clone(),
                }
            }
            false
        });
        self.upstream = watch_upstream(&self.routes);
        if self.lost.is_empty() {
            return;
        }
        tracing::debug!(stream = self.stream.name(), "a stream read through an alias was replaced; reading it again");
        self.reselect(Duration::ZERO);
    }

    /// Resolves the lost media again in this stream, `after` from now.
    fn reselect(&mut self, after: Duration) {
        let (stream, want) = (self.stream.clone(), self.lost.clone());
        self.reselection = Some(Box::pin(async move {
            if !after.is_zero() {
                tokio::time::sleep(after).await;
            }
            let deadline = Instant::now() + stream.limits.probe_timeout;
            stream.select(&want, deadline).await
        }));
    }

    fn account(&self, event: &Event) {
        if let Event::Frame { frame, .. } = event {
            let stats = &self.registration.stats;
            stats.frames.fetch_add(1, Ordering::Relaxed);
            stats.bytes.fetch_add(frame.data.len() as u64, Ordering::Relaxed);
        }
    }

    fn handle(&mut self, input: u64, item: Result<Item, u64>) {
        match item {
            Err(lost) => {
                self.registration.stats.dropped.fetch_add(lost, Ordering::Relaxed);
                self.report_lag(lost);
                if let Some(route) = self.routes.iter_mut().find(|r| r.id == input) {
                    route.need_keyframe = true;
                }
                if self.tracks.is_some() {
                    self.pending.push_back(Event::Discontinuity);
                }
            }
            Ok(Item::Tracks(tracks)) => {
                if let Some(route) = self.routes.iter_mut().find(|r| r.id == input) {
                    route.resolve(tracks);
                }
                self.rebuild(false);
            }
            Ok(Item::Frame(frame)) => {
                if self.tracks.is_some() {
                    if let Some(event) = self.route_frame(input, frame) {
                        self.pending.push_back(event);
                    }
                } else {
                    if self.backlog.len() >= self.stream.limits.max_backlog_frames {
                        self.backlog.clear();
                        for route in &mut self.routes {
                            route.need_keyframe = true;
                        }
                    }
                    self.backlog.push_back((input, frame));
                }
            }
        }
    }

    /// Reports that frames were dropped for this consumer: at once the
    /// first time, then at most once per [`LAG_REPORT_EVERY`], with the
    /// count. A recorder's loss is a hole in a recording, a warning; a live
    /// consumer's is debug (a WebRTC session reads nothing until it is
    /// connected, and one connecting through a relay can take longer than
    /// its budget: it starts at the next keyframe, which is what a viewer
    /// wants), and counted in the stream's listing (`dropped`) either way.
    fn report_lag(&mut self, lost: u64) {
        self.unreported_drops = self.unreported_drops.saturating_add(lost);
        let Some(overflows) = self.lags.refused() else { return };
        let budget = self.stream.limits.queue_budget(self.tag.class);
        let (stream, consumer) = (self.stream.name(), self.tag.kind.as_str());
        let remote = self.tag.remote.as_deref().unwrap_or("-");
        let dropped = std::mem::take(&mut self.unreported_drops);
        let max_age_ms = u64::try_from(budget.max_age.as_millis()).unwrap_or(u64::MAX);
        let message = "consumer fell behind its queue budget: frames dropped, video resumes at a keyframe";
        match self.tag.class {
            ConsumerClass::Recorder => tracing::warn!(
                stream,
                consumer,
                remote,
                class = "recorder",
                dropped,
                overflows,
                max_bytes = budget.max_bytes,
                max_age_ms,
                "{message}"
            ),
            ConsumerClass::Live => tracing::debug!(
                stream,
                consumer,
                remote,
                class = "live",
                dropped,
                overflows,
                max_bytes = budget.max_bytes,
                max_age_ms,
                "{message}"
            ),
        }
    }
}

/// Polls the future `held` holds, never if it holds none. The future stays
/// where it is between polls, so a cancelled `recv` loses nothing.
async fn poll_held<T>(held: &mut Option<BoxFuture<'static, T>>) -> T {
    match held {
        Some(future) => future.await,
        None => std::future::pending().await,
    }
}

async fn sleep_until(deadline: Option<Instant>) {
    match deadline {
        Some(at) => tokio::time::sleep_until(at).await,
        None => std::future::pending().await,
    }
}

#[path = "subscription_tracks.rs"]
mod tracks;
