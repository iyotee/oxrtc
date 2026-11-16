//! The queue between a source's channel and one consumer, bounded by bytes
//! and by time for that consumer's class, not by a count of items.
//!
//! A count says nothing a consumer can rely on: 256 items is about 4 s for a
//! recorder reading a camera's picture and its Opus sound, and far less for a
//! camera that sends its frames in slices. A recorder (an RTSP reader: the
//! NVR's segmenter, a transcode) must ride out a pause of a few seconds
//! without losing a frame; a live viewer must not be shown seconds of the
//! past. So each consumer has its own queue, whose budget its class sets
//! ([`ConsumerClass`], [`QueueBudget`]), and the producer enforces it when it
//! publishes: a consumer that stops reading costs at most its budget, never
//! the producer's time.
//!
//! Beyond the budget, the queue keeps what follows its latest keyframe when
//! that fits, so the consumer resumes at once at a keyframe instead of
//! waiting for the camera's next one (up to 6 s on the owner's cameras), and
//! drops everything before it; with no keyframe to resume at, it drops
//! everything, and video resumes at the next one. A source without video
//! loses its oldest frames only.

use std::collections::VecDeque;
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use tokio::sync::Notify;
use tokio::time::Instant;

use crate::channel::Item;
use crate::error::WantError;
use crate::frame::Frame;

/// Which budget a consumer's queues have.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ConsumerClass {
    /// Shown as it happens (a WebRTC viewer, a still): a short queue, since
    /// a frame late by seconds is worth less than the next keyframe.
    #[default]
    Live,
    /// Writes what it reads (an RTSP reader that says nothing: a segmenter, a
    /// sound transcode): a deep queue, since every frame lost is a hole in a
    /// recording.
    Recorder,
}

/// The query key a reader names its class with (`?video&queue=live`). An
/// oxrtc-only key: go2rtc reads only `video` and `audio` from a reader's
/// query and ignores it, so a URL that carries it works on both.
pub const QUEUE_QUERY_KEY: &str = "queue";

impl ConsumerClass {
    /// Its name in the log, and in a reader's `queue=`.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Live => "live",
            Self::Recorder => "recorder",
        }
    }

    /// The class `queue=` names in a reader's query (go2rtc notation, with
    /// or without its `?`), `None` when the query does not say. Every value
    /// given must be `live` or `recorder`, and the same: a typo is refused
    /// rather than read as the default.
    pub fn from_query(query: &str) -> Result<Option<Self>, WantError> {
        let mut named = None;
        for pair in query.trim_start_matches('?').split('&') {
            let Some(value) = pair.strip_prefix(QUEUE_QUERY_KEY).and_then(|rest| rest.strip_prefix('=')) else {
                continue;
            };
            let class = match value {
                "live" => Self::Live,
                "recorder" => Self::Recorder,
                other => return Err(WantError::UnknownQueue(other.chars().take(32).collect())),
            };
            if named.is_some_and(|earlier| earlier != class) {
                return Err(WantError::UnknownQueue(format!(
                    "{} and {}",
                    Self::Live.as_str(),
                    Self::Recorder.as_str()
                )));
            }
            named = Some(class);
        }
        Ok(named)
    }
}

/// How much one consumer's queue may hold.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QueueBudget {
    /// Bytes queued, payloads and the bookkeeping of each item counted
    /// ([`ITEM_OVERHEAD`]), so a flood of tiny frames is bounded too.
    pub max_bytes: usize,
    /// How long the oldest queued item may have waited.
    pub max_age: Duration,
}

/// What an item costs beyond its payload: its place in the queue and its
/// share of the frame's allocation.
pub const ITEM_OVERHEAD: usize = size_of::<Queued>() + size_of::<Frame>() + 2 * size_of::<usize>();

#[derive(Debug)]
struct Queued {
    item: Item,
    at: Instant,
    cost: usize,
}

impl Queued {
    fn is_frame(&self) -> bool {
        matches!(self.item, Item::Frame(_))
    }

    fn is_key_of(&self, video: Option<u8>) -> bool {
        matches!(&self.item, Item::Frame(frame) if frame.keyframe && Some(frame.track) == video)
    }
}

#[derive(Debug, Default)]
struct State {
    items: VecDeque<Queued>,
    bytes: usize,
    /// Frames dropped and not yet reported to the consumer.
    lost: u64,
    closed: bool,
}

/// One consumer's queue. The channel pushes, the consumer's route pops.
#[derive(Debug)]
pub(crate) struct ConsumerQueue {
    state: Mutex<State>,
    notify: Notify,
    budget: QueueBudget,
}

fn cost(item: &Item) -> usize {
    match item {
        Item::Frame(frame) => frame.data.len().saturating_add(ITEM_OVERHEAD),
        Item::Tracks(_) => ITEM_OVERHEAD,
    }
}

impl ConsumerQueue {
    pub(crate) fn new(budget: QueueBudget) -> Self {
        Self { state: Mutex::new(State::default()), notify: Notify::new(), budget }
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Queues `item`, making room within the budget first. `video` is the
    /// source's video track, whose keyframes the queue can resume at.
    /// Returns the frames dropped to make room.
    pub(crate) fn push(&self, item: Item, video: Option<u8>, now: Instant) -> u64 {
        let incoming = Queued { cost: cost(&item), item, at: now };
        let dropped = {
            let mut state = self.lock();
            if state.closed {
                return 0;
            }
            let dropped = state.make_room(&incoming, video, self.budget, now);
            state.bytes = state.bytes.saturating_add(incoming.cost);
            state.items.push_back(incoming);
            dropped
        };
        self.notify.notify_one();
        dropped
    }

    /// No more items: the consumer reads what is queued, then the end.
    pub(crate) fn close(&self) {
        self.lock().closed = true;
        self.notify.notify_one();
    }

    /// The next item, `Err(n)` when `n` frames were dropped before it, or
    /// `None` once closed and empty. Cancel-safe: nothing is taken before
    /// it returns.
    pub(crate) async fn recv(&self) -> Option<Result<Item, u64>> {
        loop {
            {
                let mut state = self.lock();
                if state.lost > 0 {
                    return Some(Err(std::mem::take(&mut state.lost)));
                }
                if let Some(queued) = state.items.pop_front() {
                    state.bytes = state.bytes.saturating_sub(queued.cost);
                    return Some(Ok(queued.item));
                }
                if state.closed {
                    return None;
                }
            }
            // A notification sent between the check and here is kept (one
            // consumer per queue), so none is missed.
            self.notify.notified().await;
        }
    }

    #[cfg(test)]
    fn queued(&self) -> (usize, usize) {
        let state = self.lock();
        (state.items.len(), state.bytes)
    }
}

impl State {
    fn over(&self, incoming: &Queued, budget: QueueBudget, now: Instant) -> bool {
        let Some(front) = self.items.front() else { return false };
        self.bytes.saturating_add(incoming.cost) > budget.max_bytes
            || now.saturating_duration_since(front.at) > budget.max_age
    }

    fn make_room(&mut self, incoming: &Queued, video: Option<u8>, budget: QueueBudget, now: Instant) -> u64 {
        if !self.over(incoming, budget, now) {
            return 0;
        }
        let dropped = if video.is_none() {
            // Sound only: every frame stands alone, the oldest go first.
            let mut dropped = Vec::new();
            while self.over(incoming, budget, now) {
                let Some(old) = self.items.pop_front() else { break };
                self.bytes = self.bytes.saturating_sub(old.cost);
                dropped.push(old);
            }
            self.keep_tracks_dropped_with(dropped, now)
        } else if incoming.is_key_of(video) {
            self.drop_front(self.items.len(), now)
        } else {
            match self.items.iter().rposition(|q| q.is_key_of(video)) {
                Some(key) => {
                    let mut dropped = self.drop_front(key, now);
                    if self.over(incoming, budget, now) {
                        dropped = dropped.saturating_add(self.drop_front(self.items.len(), now));
                    }
                    dropped
                }
                None => self.drop_front(self.items.len(), now),
            }
        };
        self.lost = self.lost.saturating_add(dropped);
        dropped
    }

    /// Drops the first `count` items, keeping the latest track list among
    /// them: the frames after it name its tracks. Returns the frames dropped.
    fn drop_front(&mut self, count: usize, now: Instant) -> u64 {
        let dropped: Vec<Queued> = self.items.drain(..count.min(self.items.len())).collect();
        for old in &dropped {
            self.bytes = self.bytes.saturating_sub(old.cost);
        }
        self.keep_tracks_dropped_with(dropped, now)
    }

    /// Puts the latest track list of `dropped` back in front, dated like
    /// what now follows it (its own age is not the queue's). Returns the
    /// frames among `dropped`.
    fn keep_tracks_dropped_with(&mut self, dropped: Vec<Queued>, now: Instant) -> u64 {
        let frames = dropped.iter().filter(|q| q.is_frame()).count() as u64;
        if let Some(mut tracks) = dropped.into_iter().rev().find(|q| !q.is_frame()) {
            tracks.at = self.items.front().map_or(now, |q| q.at);
            self.bytes = self.bytes.saturating_add(tracks.cost);
            self.items.push_front(tracks);
        }
        frames
    }
}

#[cfg(test)]
#[path = "queue_tests.rs"]
mod tests;
