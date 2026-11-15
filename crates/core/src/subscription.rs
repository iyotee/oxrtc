//! A consumer's view of a stream: one merged, ordered sequence of events.

use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use futures_util::FutureExt;
use futures_util::stream::{SelectAll, StreamExt};
use tokio::time::Instant;

use crate::channel::Item;
use crate::driver::BoxFuture;
use crate::error::{SubEnd, SubscribeError};
use crate::frame::Frame;
use crate::media::{MediaKind, TrackInfo};
use crate::route::{Input, Route, watch_upstream};
use crate::select::Pick;
use crate::stream::{Registration, Stream};
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
    /// Frames were lost (the consumer lagged); video resumes at a keyframe.
    Discontinuity,
}

/// How often, until the first track list, the sources of routes still
/// unresolved are checked for a failed attempt.
const FAILURE_CHECK: Duration = Duration::from_millis(100);

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
    /// The next failure check; a field, since frames wake the loop far more
    /// often than the check's period and would otherwise push it back forever.
    next_check: Instant,
    /// Fires when a stream a route was resolved through (an alias's) closes.
    upstream: Option<BoxFuture<'static, ()>>,
    /// The media of the routes that closed with it, being resolved again.
    lost: Want,
    reselection: Option<Reselection>,
    registration: Registration,
}

impl std::fmt::Debug for Subscription {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Subscription").field("stream", &self.stream.name()).field("id", &self.registration.id).finish()
    }
}

impl Subscription {
    pub(crate) fn new(stream: Arc<Stream>, picks: Vec<Pick>, registration: Registration, deadline: Instant) -> Self {
        let mut subscription = Self {
            stream,
            inputs: SelectAll::new(),
            routes: Vec::with_capacity(picks.len()),
            next_route: 0,
            tracks: None,
            pending: VecDeque::new(),
            backlog: VecDeque::new(),
            deadline: Some(deadline),
            next_check: Instant::now() + FAILURE_CHECK,
            upstream: None,
            lost: Want::default(),
            reselection: None,
            registration,
        };
        subscription.add_routes(picks);
        subscription
    }

    fn add_routes(&mut self, picks: Vec<Pick>) {
        for pick in picks {
            let (route, input) = Route::attach(self.next_route, pick);
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

    /// Handles the items already waiting, without waiting for more: what a
    /// deadline decides must count the tracks that did arrive.
    fn drain_ready(&mut self) {
        while let Some(Some((input, item))) = self.inputs.next().now_or_never() {
            self.handle(input, item);
        }
    }

    /// Gives up the unresolved routes whose source failed an attempt, and
    /// sends the first track list once every other route is resolved. When
    /// nothing is resolved and every route given up reads this server, the
    /// subscription ends at once: waiting would only repeat that answer.
    fn give_up_failed(&mut self) {
        if self.tracks.is_some() {
            return;
        }
        for route in &mut self.routes {
            if !route.ready() && !route.given_up && route.slot.failures() > route.failures_before {
                tracing::debug!(source = %route.slot.spec, "source failed before its tracks; answering without it");
                route.given_up = true;
            }
        }
        if self.routes.iter().any(|r| r.given_up) && self.routes.iter().all(|r| r.ready() || r.given_up) {
            self.flush_partial();
            let hopeless = self.routes.iter().all(|r| r.given_up && r.slot.is_local());
            if self.tracks.is_none() && hopeless {
                self.deadline = None;
            }
        }
    }

    fn flush_partial(&mut self) {
        if self.tracks.is_none() && self.routes.iter().any(Route::ready) {
            self.rebuild(true);
        }
    }

    /// Recomputes the subscription's track list from the routes, and emits it
    /// when it changed (the first time, only once every route is resolved,
    /// unless `partial` - the probe deadline passed).
    fn rebuild(&mut self, partial: bool) {
        if self.tracks.is_none() && !partial && !self.routes.iter().all(Route::ready) {
            return;
        }
        let mut list = Vec::new();
        for route in &mut self.routes {
            route.map = vec![None; route.tracks.as_ref().map_or(0, |t| t.len())];
        }
        for kind in MediaKind::ALL {
            for route in &mut self.routes {
                let Some(&(_, source_track)) = route.chosen.iter().find(|(k, _)| *k == kind) else { continue };
                let Some(info) = route.tracks.as_ref().and_then(|t| t.get(source_track)) else { continue };
                if let Some(slot) = route.map.get_mut(source_track) {
                    *slot = Some(list.len());
                }
                list.push(info.clone());
            }
        }
        if list.is_empty() {
            return;
        }
        let list: Arc<[TrackInfo]> = list.into();
        if self.tracks.as_deref() == Some(&*list) {
            return;
        }
        let first = self.tracks.is_none();
        self.tracks = Some(list.clone());
        self.deadline = None;
        self.pending.push_back(Event::Tracks(list));
        if first {
            while let Some((input, frame)) = self.backlog.pop_front() {
                if let Some(event) = self.route_frame(input, frame) {
                    self.pending.push_back(event);
                }
            }
        }
    }

    fn route_frame(&mut self, input: u64, frame: Arc<Frame>) -> Option<Event> {
        let route = self.routes.iter_mut().find(|r| r.id == input)?;
        let source_track = usize::from(frame.track);
        let track = (*route.map.get(source_track)?)?;
        if route.is_video(source_track) && route.need_keyframe {
            if !frame.keyframe {
                return None;
            }
            route.need_keyframe = false;
        }
        Some(Event::Frame { track, frame })
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
