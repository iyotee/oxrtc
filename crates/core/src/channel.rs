//! Per-source fan-out: a queue per consumer, bounded by its class's budget
//! ([`crate::queue`]), plus the cache of the current group of pictures.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, Weak};

use tokio::sync::watch;
use tokio::time::Instant;

use crate::error::PublishError;
use crate::frame::{Frame, validate_avcc};
use crate::limits::Limits;
use crate::media::{MediaKind, TrackInfo};
use crate::queue::{ConsumerQueue, QueueBudget};
use crate::timeline::Timelines;

/// What travels through a consumer's queue. A new track list goes through
/// the same queue as the frames, so a consumer sees them in order.
#[derive(Debug, Clone)]
pub(crate) enum Item {
    Tracks(Arc<[TrackInfo]>),
    Frame(Arc<Frame>),
}

/// Counters of one source.
#[derive(Debug, Default)]
pub(crate) struct Counters {
    pub(crate) frames: AtomicU64,
    pub(crate) bytes: AtomicU64,
    pub(crate) rejected: AtomicU64,
}

#[derive(Debug, Default)]
struct Inner {
    tracks: Option<Arc<[TrackInfo]>>,
    gop_track: Option<u8>,
    gop: Vec<Arc<Frame>>,
    gop_bytes: usize,
    gop_open: bool,
    last_frame: Option<Instant>,
    track_frames: Vec<u64>,
    /// Outlives the source's connections ([`crate::timeline`]).
    timelines: Timelines,
    /// The consumers' queues; a consumer gone leaves a dead entry, pruned
    /// at the next item.
    consumers: Vec<Weak<ConsumerQueue>>,
}

impl Inner {
    fn has_consumers(&self) -> bool {
        self.consumers.iter().any(|queue| queue.strong_count() > 0)
    }

    /// Hands `item` to every consumer, each within its own budget.
    fn deliver(&mut self, item: &Item) {
        let (now, video) = (Instant::now(), self.gop_track);
        self.consumers.retain(|queue| match queue.upgrade() {
            Some(queue) => {
                queue.push(item.clone(), video, now);
                true
            }
            None => false,
        });
    }
}

/// A source's channel. One producer, any number of consumers.
#[derive(Debug)]
pub(crate) struct Channel {
    inner: Mutex<Inner>,
    tracks_tx: watch::Sender<Option<Arc<[TrackInfo]>>>,
    pub(crate) counters: Counters,
    max_frame_bytes: usize,
    max_tracks: usize,
    gop_cache_bytes: usize,
    /// The hub's epoch, which every track's timeline counts from.
    epoch: Instant,
    /// The source, redacted, as the log names it.
    label: String,
    /// Whether a group of pictures larger than the cache was logged: once
    /// per source, reconnections included.
    gop_overflow_logged: AtomicBool,
}

/// What a new consumer starts from: the current tracks, the cached group of
/// pictures, and its queue, which receives what follows them.
pub(crate) type Attachment = (Option<Arc<[TrackInfo]>>, Vec<Arc<Frame>>, Arc<ConsumerQueue>);

impl Drop for Channel {
    fn drop(&mut self) {
        let inner = self.inner.get_mut().unwrap_or_else(PoisonError::into_inner);
        for queue in inner.consumers.iter().filter_map(Weak::upgrade) {
            queue.close();
        }
    }
}

impl Channel {
    /// The channel of the source `label` (redacted: it is logged).
    pub(crate) fn new(limits: &Limits, epoch: Instant, label: String) -> Arc<Self> {
        let (tracks_tx, _) = watch::channel(None);
        Arc::new(Self {
            inner: Mutex::new(Inner::default()),
            tracks_tx,
            counters: Counters::default(),
            max_frame_bytes: limits.max_frame_bytes,
            max_tracks: limits.max_tracks_per_source,
            gop_cache_bytes: limits.gop_cache_bytes,
            epoch,
            label,
            gop_overflow_logged: AtomicBool::new(false),
        })
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Forgets the connection's tracks and cache, before a reconnection. The
    /// timelines stay while a consumer is attached, which reads across the
    /// restart: the next run continues after what it was given. With none,
    /// nobody saw that history, and the next run starts on the wall clock.
    pub(crate) fn reset(&self) {
        {
            let mut inner = self.lock();
            let mut timelines = std::mem::take(&mut inner.timelines);
            if inner.has_consumers() {
                timelines.restart();
            } else {
                timelines = Timelines::default();
            }
            let consumers = std::mem::take(&mut inner.consumers);
            *inner = Inner { timelines, consumers, ..Inner::default() };
        }
        self.tracks_tx.send_replace(None);
    }

    pub(crate) fn set_tracks(&self, tracks: Vec<TrackInfo>) -> Result<(), PublishError> {
        if tracks.is_empty() {
            return Err(PublishError::InvalidTracks("no track"));
        }
        if tracks.len() > self.max_tracks {
            return Err(PublishError::InvalidTracks("too many tracks"));
        }
        let tracks: Arc<[TrackInfo]> = tracks.into();
        let gop_track = tracks.iter().position(|t| t.kind == MediaKind::Video).and_then(|p| u8::try_from(p).ok());
        {
            let mut inner = self.lock();
            inner.tracks = Some(tracks.clone());
            inner.track_frames = vec![0; tracks.len()];
            inner.timelines.set_tracks(&tracks);
            inner.gop_track = gop_track;
            inner.gop.clear();
            inner.gop_bytes = 0;
            inner.gop_open = false;
            inner.deliver(&Item::Tracks(tracks.clone()));
        }
        self.tracks_tx.send_replace(Some(tracks));
        Ok(())
    }

    pub(crate) fn publish(&self, frame: Frame) -> Result<(), PublishError> {
        let result = self.try_publish(frame);
        if result.is_err() {
            self.counters.rejected.fetch_add(1, Ordering::Relaxed);
        }
        result
    }

    fn try_publish(&self, mut frame: Frame) -> Result<(), PublishError> {
        let size = frame.data.len();
        if size == 0 {
            return Err(PublishError::Malformed(crate::error::FrameError::Empty));
        }
        if size > self.max_frame_bytes {
            return Err(PublishError::TooLarge { size, limit: self.max_frame_bytes });
        }
        let mut inner = self.lock();
        let codec = inner
            .tracks
            .as_ref()
            .ok_or(PublishError::NoTracks)?
            .get(usize::from(frame.track))
            .ok_or(PublishError::UnknownTrack(frame.track))?
            .codec;
        if codec.is_nal_based() {
            validate_avcc(&frame.data)?;
        }
        frame.pts = inner.timelines.map(usize::from(frame.track), frame.pts, frame.received, self.epoch);
        let frame = Arc::new(frame);
        if self.gop_cache_bytes > 0 {
            if inner.gop_track == Some(frame.track) && frame.keyframe {
                inner.gop.clear();
                inner.gop_bytes = 0;
                inner.gop_open = true;
            }
            if inner.gop_open {
                if inner.gop_bytes + size > self.gop_cache_bytes {
                    // The group as it outgrew the cache: this frame included,
                    // in its count as in its bytes.
                    let (frames, bytes) = (inner.gop.len() + 1, inner.gop_bytes + size);
                    inner.gop.clear();
                    inner.gop_bytes = 0;
                    inner.gop_open = false;
                    self.gop_overflowed(frames, bytes);
                } else {
                    inner.gop_bytes += size;
                    inner.gop.push(frame.clone());
                }
            }
        }
        inner.last_frame = Some(Instant::now());
        if let Some(count) = inner.track_frames.get_mut(usize::from(frame.track)) {
            *count += 1;
        }
        self.counters.frames.fetch_add(1, Ordering::Relaxed);
        self.counters.bytes.fetch_add(size as u64, Ordering::Relaxed);
        inner.deliver(&Item::Frame(frame));
        Ok(())
    }

    /// Says, once per source, that its group of pictures outgrew the cache:
    /// until its next keyframe, a new consumer starts at that keyframe and a
    /// viewer's keyframe request goes unanswered, which nothing else shows.
    fn gop_overflowed(&self, frames: usize, bytes: usize) {
        if !self.gop_overflow_logged.swap(true, Ordering::Relaxed) {
            tracing::warn!(
                source = %self.label,
                frames,
                bytes,
                limit = self.gop_cache_bytes,
                "group of pictures larger than the keyframe cache (api.gop_cache_mib): new consumers wait for the \
                 next keyframe; said once per source"
            );
        }
    }

    /// A new consumer with a queue of `budget`, and what it starts from.
    pub(crate) fn attach(&self, budget: QueueBudget) -> Attachment {
        let mut inner = self.lock();
        let gop = if inner.gop_open { inner.gop.clone() } else { Vec::new() };
        let queue = Arc::new(ConsumerQueue::new(budget));
        inner.consumers.push(Arc::downgrade(&queue));
        (inner.tracks.clone(), gop, queue)
    }

    /// The cached group of pictures as it is now: from the last keyframe of
    /// the video track to the latest frame, every track's frames in order.
    /// Empty when no keyframe has come yet, or the group outgrew the cache.
    pub(crate) fn gop(&self) -> Vec<Arc<Frame>> {
        let inner = self.lock();
        if inner.gop_open { inner.gop.clone() } else { Vec::new() }
    }

    pub(crate) fn tracks(&self) -> Option<Arc<[TrackInfo]>> {
        self.lock().tracks.clone()
    }

    /// Frames published per track on the current connection.
    pub(crate) fn track_frames(&self) -> Vec<u64> {
        self.lock().track_frames.clone()
    }

    pub(crate) fn last_frame(&self) -> Option<Instant> {
        self.lock().last_frame
    }

    pub(crate) fn watch_tracks(&self) -> watch::Receiver<Option<Arc<[TrackInfo]>>> {
        self.tracks_tx.subscribe()
    }
}

/// Where a [`crate::SourceDriver`] publishes: first a track list, then frames
/// that name tracks of that list. A new track list (after a renegotiation)
/// may be published at any time; consumers are told.
#[derive(Debug, Clone)]
pub struct ProducerSink {
    channel: Arc<Channel>,
}

impl ProducerSink {
    pub(crate) fn new(channel: Arc<Channel>) -> Self {
        Self { channel }
    }

    /// Announces the source's tracks. Clears the keyframe cache.
    pub fn set_tracks(&self, tracks: Vec<TrackInfo>) -> Result<(), PublishError> {
        self.channel.set_tracks(tracks)
    }

    /// Publishes a frame, its `pts` in the source's own clock: consumers see
    /// it on the hub's timeline ([`crate::timeline`]). Refused if it names an
    /// unknown track, exceeds the size limit, or (H.264/H.265) its length
    /// prefixes do not add up.
    pub fn publish(&self, frame: Frame) -> Result<(), PublishError> {
        self.channel.publish(frame)
    }

    /// The tracks announced so far on this connection.
    pub fn tracks(&self) -> Option<Arc<[TrackInfo]>> {
        self.channel.tracks()
    }
}

#[cfg(test)]
#[path = "channel_tests.rs"]
mod tests;
