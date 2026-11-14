//! A consumer's view of a stream: one merged, ordered sequence of events.

use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use futures_util::stream::{self, BoxStream, SelectAll, StreamExt};
use tokio::time::Instant;
use tokio_stream::wrappers::BroadcastStream;
use tokio_stream::wrappers::errors::BroadcastStreamRecvError;

use crate::channel::Item;
use crate::error::SubEnd;
use crate::frame::Frame;
use crate::media::{MediaKind, TrackInfo};
use crate::slot::DemandGuard;
use crate::stream::{Pick, Registration, Stream};
use crate::want::MediaWant;

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

type Input = BoxStream<'static, (usize, Result<Item, u64>)>;

struct Route {
    kinds: Vec<(MediaKind, MediaWant)>,
    tracks: Option<Arc<[TrackInfo]>>,
    chosen: Vec<(MediaKind, usize)>,
    map: Vec<Option<usize>>,
    need_keyframe: bool,
}

impl Route {
    fn ready(&self) -> bool {
        self.kinds.iter().all(|(kind, _)| self.chosen.iter().any(|(k, _)| k == kind))
    }

    fn resolve(&mut self, tracks: Arc<[TrackInfo]>) {
        self.chosen = self
            .kinds
            .iter()
            .filter_map(|(kind, wish)| {
                tracks.iter().position(|t| t.kind == *kind && wish.accepts(t.codec)).map(|i| (*kind, i))
            })
            .collect();
        self.tracks = Some(tracks);
        self.need_keyframe = true;
    }

    fn is_video(&self, source_track: usize) -> bool {
        self.tracks.as_ref().and_then(|t| t.get(source_track)).is_some_and(|t| t.kind == MediaKind::Video)
    }
}

/// A subscription to a stream. Dropping it releases the sources it holds.
pub struct Subscription {
    stream: Arc<Stream>,
    inputs: SelectAll<Input>,
    routes: Vec<Route>,
    tracks: Option<Arc<[TrackInfo]>>,
    pending: VecDeque<Event>,
    backlog: VecDeque<(usize, Arc<Frame>)>,
    deadline: Option<Instant>,
    registration: Registration,
    _guards: Vec<DemandGuard>,
}

impl std::fmt::Debug for Subscription {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Subscription").field("stream", &self.stream.name()).field("id", &self.registration.id).finish()
    }
}

impl Subscription {
    pub(crate) fn new(stream: Arc<Stream>, picks: Vec<Pick>, registration: Registration) -> Self {
        let mut inputs = SelectAll::new();
        let mut routes = Vec::with_capacity(picks.len());
        let mut guards = Vec::with_capacity(picks.len());
        for (index, pick) in picks.into_iter().enumerate() {
            let (tracks, gop, rx) = pick.slot.channel.attach();
            let prefix: Vec<Result<Item, u64>> =
                tracks.into_iter().map(Item::Tracks).chain(gop.into_iter().map(Item::Frame)).map(Ok).collect();
            let live = BroadcastStream::new(rx).map(|r| r.map_err(|BroadcastStreamRecvError::Lagged(n)| n));
            let input: Input = stream::iter(prefix).chain(live).map(move |item| (index, item)).boxed();
            inputs.push(input);
            routes.push(Route {
                kinds: pick.kinds,
                tracks: None,
                chosen: Vec::new(),
                map: Vec::new(),
                need_keyframe: true,
            });
            guards.push(pick.guard);
        }
        let deadline = Some(Instant::now() + stream.limits.probe_timeout);
        Self {
            stream,
            inputs,
            routes,
            tracks: None,
            pending: VecDeque::new(),
            backlog: VecDeque::new(),
            deadline,
            registration,
            _guards: guards,
        }
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
            let deadline = self.deadline;
            let next = tokio::select! {
                biased;
                () = closed.cancelled() => return Err(SubEnd::Closed),
                () = sleep_until(deadline) => {
                    self.deadline = None;
                    self.flush_partial();
                    continue;
                }
                item = self.inputs.next() => item,
            };
            let Some((input, item)) = next else { return Err(SubEnd::Closed) };
            self.handle(input, item);
        }
    }

    fn account(&self, event: &Event) {
        if let Event::Frame { frame, .. } = event {
            let stats = &self.registration.stats;
            stats.frames.fetch_add(1, Ordering::Relaxed);
            stats.bytes.fetch_add(frame.data.len() as u64, Ordering::Relaxed);
        }
    }

    fn handle(&mut self, input: usize, item: Result<Item, u64>) {
        match item {
            Err(lost) => {
                self.registration.stats.dropped.fetch_add(lost, Ordering::Relaxed);
                if let Some(route) = self.routes.get_mut(input) {
                    route.need_keyframe = true;
                }
                if self.tracks.is_some() {
                    self.pending.push_back(Event::Discontinuity);
                }
            }
            Ok(Item::Tracks(tracks)) => {
                if let Some(route) = self.routes.get_mut(input) {
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

    fn route_frame(&mut self, input: usize, frame: Arc<Frame>) -> Option<Event> {
        let route = self.routes.get_mut(input)?;
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

async fn sleep_until(deadline: Option<Instant>) {
    match deadline {
        Some(at) => tokio::time::sleep_until(at).await,
        None => std::future::pending().await,
    }
}
