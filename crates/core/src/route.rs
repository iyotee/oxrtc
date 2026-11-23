//! One source a subscription reads: its input, what it takes from it, and
//! how its tracks map onto the subscription's.

use std::sync::Arc;

use futures_util::stream::{self, BoxStream, FuturesUnordered, StreamExt};
use tokio_util::sync::CancellationToken;

use crate::channel::Item;
use crate::driver::BoxFuture;
use crate::media::{MediaKind, TrackInfo};
use crate::memory::MemoryBudget;
use crate::queue::QueueBudget;
use crate::select::{Kinds, Pick};
use crate::slot::{DemandGuard, Slot};
use crate::stream::Stream;

/// A route's items, tagged with its id: `Err(n)` when `n` frames were
/// dropped for the consumer's budget. Ends when the route is dropped.
pub(crate) type Input = BoxStream<'static, (u64, Result<Item, u64>)>;

pub(crate) struct Route {
    pub(crate) id: u64,
    pub(crate) slot: Arc<Slot>,
    /// Its source failed before delivering tracks: the first track list
    /// goes out without it (a camera without a microphone, whose sound
    /// transcode exits at once, must not hold the picture back).
    pub(crate) given_up: bool,
    /// The source's failed attempts when the route was made.
    pub(crate) failures_before: u64,
    pub(crate) kinds: Kinds,
    /// The streams the route was resolved through ([`Pick::via`]).
    pub(crate) via: Vec<Arc<Stream>>,
    pub(crate) tracks: Option<Arc<[TrackInfo]>>,
    pub(crate) chosen: Vec<(MediaKind, usize)>,
    pub(crate) map: Vec<Option<usize>>,
    pub(crate) need_keyframe: bool,
    _guard: DemandGuard,
    /// Ends the route's input.
    stop: CancellationToken,
}

impl Drop for Route {
    fn drop(&mut self) {
        self.stop.cancel();
    }
}

impl Route {
    /// A route for `pick`, and its input: the source's current tracks and,
    /// with `cached`, its cached group of pictures, then its live items
    /// through a queue of `budget` (without the cache, video starts at the
    /// next keyframe, as any route's does).
    pub(crate) fn attach(
        id: u64,
        pick: Pick,
        budget: QueueBudget,
        pool: Option<Arc<MemoryBudget>>,
        cached: bool,
    ) -> (Self, Input) {
        let (tracks, mut gop, queue) = pick.slot.channel.attach(budget, pool);
        if !cached {
            gop.clear();
        }
        let prefix: Vec<Result<Item, u64>> =
            tracks.into_iter().map(Item::Tracks).chain(gop.into_iter().map(Item::Frame)).map(Ok).collect();
        let live = stream::unfold(queue, |queue| async move { queue.recv().await.map(|next| (next, queue)) });
        let stop = CancellationToken::new();
        let input: Input = stream::iter(prefix)
            .chain(live)
            .take_until(stop.clone().cancelled_owned())
            .map(move |item| (id, item))
            .boxed();
        let route = Self {
            id,
            slot: pick.slot,
            given_up: false,
            failures_before: pick.failures_before,
            kinds: pick.kinds,
            via: pick.via,
            tracks: None,
            chosen: Vec::new(),
            map: Vec::new(),
            need_keyframe: true,
            _guard: pick.guard,
            stop,
        };
        (route, input)
    }

    pub(crate) fn ready(&self) -> bool {
        self.kinds.iter().all(|(kind, _)| self.chosen.iter().any(|(k, _)| k == kind))
    }

    pub(crate) fn resolve(&mut self, tracks: Arc<[TrackInfo]>) {
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

    pub(crate) fn is_video(&self, source_track: usize) -> bool {
        self.tracks.as_ref().and_then(|t| t.get(source_track)).is_some_and(|t| t.kind == MediaKind::Video)
    }

    /// Whether a stream it was resolved through was replaced or removed.
    pub(crate) fn upstream_closed(&self) -> bool {
        self.via.iter().any(|stream| stream.is_closed())
    }
}

/// Resolves when a stream one of `routes` was resolved through closes;
/// `None` when they read their own stream only.
pub(crate) fn watch_upstream(routes: &[Route]) -> Option<BoxFuture<'static, ()>> {
    let tokens: Vec<CancellationToken> =
        routes.iter().flat_map(|route| route.via.iter().map(|stream| stream.closed.clone())).collect();
    if tokens.is_empty() {
        return None;
    }
    Some(Box::pin(async move {
        let mut closed: FuturesUnordered<_> = tokens.iter().map(CancellationToken::cancelled).collect();
        closed.next().await;
    }))
}
