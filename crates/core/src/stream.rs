//! A named stream: an ordered list of sources and the consumers reading them.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use crate::error::SubscribeError;
use crate::info::{ConsumerSnapshot, SourceSnapshot, StreamSnapshot};
use crate::limits::Limits;
use crate::media::MediaKind;
use crate::slot::{DemandGuard, Offer, Slot};
use crate::spec::SourceSpec;
use crate::subscription::Subscription;
use crate::want::{MediaWant, Want};

/// Who a consumer is, for the stream's listing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConsumerTag {
    /// `webrtc`, `rtsp`, `preload`, `internal`...
    pub kind: String,
    /// The peer's address, if known.
    pub remote: Option<String>,
}

impl ConsumerTag {
    /// A consumer of this kind.
    pub fn new(kind: impl Into<String>) -> Self {
        Self { kind: kind.into(), remote: None }
    }

    /// The same, with the peer's address.
    pub fn with_remote(mut self, remote: impl Into<String>) -> Self {
        self.remote = Some(remote.into());
        self
    }
}

/// Counters of one consumer.
#[derive(Debug, Default)]
pub(crate) struct ConsumerStats {
    pub(crate) frames: AtomicU64,
    pub(crate) bytes: AtomicU64,
    pub(crate) dropped: AtomicU64,
}

struct ConsumerEntry {
    id: u64,
    tag: ConsumerTag,
    stats: Arc<ConsumerStats>,
}

/// A named stream.
pub struct Stream {
    name: String,
    slots: Vec<Arc<Slot>>,
    consumers: Mutex<Vec<ConsumerEntry>>,
    next_consumer: AtomicU64,
    pub(crate) closed: CancellationToken,
    pub(crate) limits: Limits,
}

impl std::fmt::Debug for Stream {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Stream").field("name", &self.name).field("sources", &self.slots.len()).finish()
    }
}

/// The sources a subscription reads, with what it takes from each.
pub(crate) struct Pick {
    pub(crate) slot: Arc<Slot>,
    pub(crate) kinds: Vec<(MediaKind, MediaWant)>,
    pub(crate) guard: DemandGuard,
    /// The source's failed attempts when it was picked (or started to be
    /// probed): later ones are this subscription's.
    pub(crate) failures_before: u64,
}

/// A consumer's place in the stream's listing; removed on drop.
pub(crate) struct Registration {
    stream: Arc<Stream>,
    pub(crate) id: u64,
    pub(crate) stats: Arc<ConsumerStats>,
}

impl Drop for Registration {
    fn drop(&mut self) {
        self.stream.consumers_lock().retain(|c| c.id != self.id);
    }
}

/// Keeps a stream's sources running without reading them, so that the next
/// consumer starts at once (go2rtc's `preload`). Dropping it releases them.
pub struct Preload {
    _picks: Vec<Pick>,
    _registration: Registration,
}

impl Stream {
    pub(crate) fn new(name: String, slots: Vec<Arc<Slot>>, limits: Limits) -> Arc<Self> {
        Arc::new(Self {
            name,
            slots,
            consumers: Mutex::new(Vec::new()),
            next_consumer: AtomicU64::new(1),
            closed: CancellationToken::new(),
            limits,
        })
    }

    /// The stream's name.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Its sources, in order.
    pub fn specs(&self) -> impl Iterator<Item = &SourceSpec> {
        self.slots.iter().map(|s| &s.spec)
    }

    /// Whether the stream was removed or replaced.
    pub fn is_closed(&self) -> bool {
        self.closed.is_cancelled()
    }

    pub(crate) fn close(&self) {
        self.closed.cancel();
        for slot in &self.slots {
            slot.shutdown();
        }
    }

    fn consumers_lock(&self) -> MutexGuard<'_, Vec<ConsumerEntry>> {
        self.consumers.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn register(self: &Arc<Self>, tag: ConsumerTag) -> Result<Registration, SubscribeError> {
        let mut consumers = self.consumers_lock();
        if consumers.len() >= self.limits.max_consumers_per_stream {
            return Err(SubscribeError::TooManyConsumers);
        }
        let id = self.next_consumer.fetch_add(1, Ordering::Relaxed);
        let stats = Arc::new(ConsumerStats::default());
        consumers.push(ConsumerEntry { id, tag, stats: stats.clone() });
        drop(consumers);
        Ok(Registration { stream: self.clone(), id, stats })
    }

    /// Subscribes to the media `want` asks for. For each wanted kind, the first
    /// source that offers an accepted codec is taken; a source whose codecs are
    /// unknown is started to find out, and stopped again if it does not fit.
    /// A source that only declares its codecs, with another candidate after
    /// it, is started too, and passed over if its first attempt fails (go2rtc
    /// falls through its sources the same way). The first event of the
    /// subscription is always its track list, within `probe_timeout` of this
    /// call whenever one source delivered.
    pub async fn subscribe(self: &Arc<Self>, want: &Want, tag: ConsumerTag) -> Result<Subscription, SubscribeError> {
        let registration = self.register(tag)?;
        let deadline = Instant::now() + self.limits.probe_timeout;
        let picks = self.select(want, deadline).await?;
        Ok(Subscription::new(self.clone(), picks, registration, deadline))
    }

    /// Holds the sources `want` selects open, without reading them.
    pub async fn preload(self: &Arc<Self>, want: &Want) -> Result<Preload, SubscribeError> {
        let registration = self.register(ConsumerTag::new("preload"))?;
        let picks = self.select(want, Instant::now() + self.limits.probe_timeout).await?;
        Ok(Preload { _picks: picks, _registration: registration })
    }

    async fn select(self: &Arc<Self>, want: &Want, deadline: Instant) -> Result<Vec<Pick>, SubscribeError> {
        if want.is_empty() {
            return Err(SubscribeError::NothingWanted);
        }
        let closed = self.closed.clone();
        tokio::select! {
            biased;
            () = closed.cancelled() => Err(SubscribeError::Closed),
            picks = self.select_open(want, deadline) => picks,
        }
    }

    async fn select_open(self: &Arc<Self>, want: &Want, deadline: Instant) -> Result<Vec<Pick>, SubscribeError> {
        let mut probes: HashMap<usize, (DemandGuard, u64)> = HashMap::new();
        let mut picks: Vec<(usize, Pick)> = Vec::new();
        for kind in MediaKind::ALL {
            let wish = want.get(kind);
            if !wish.is_wanted() {
                continue;
            }
            let Some(index) = self.first_offer(kind, wish, &mut probes, deadline).await else { continue };
            if let Some((_, pick)) = picks.iter_mut().find(|(i, _)| *i == index) {
                pick.kinds.push((kind, wish.clone()));
                continue;
            }
            let Some(slot) = self.slots.get(index) else { continue };
            let (guard, failures_before) = probes.remove(&index).unwrap_or_else(|| (slot.acquire(), slot.failures()));
            let kinds = vec![(kind, wish.clone())];
            picks.push((index, Pick { slot: slot.clone(), kinds, guard, failures_before }));
        }
        if picks.is_empty() {
            return Err(SubscribeError::NoMatchingSource);
        }
        Ok(picks.into_iter().map(|(_, p)| p).collect())
    }

    async fn first_offer(
        &self,
        kind: MediaKind,
        wish: &MediaWant,
        probes: &mut HashMap<usize, (DemandGuard, u64)>,
        deadline: Instant,
    ) -> Option<usize> {
        for (index, slot) in self.slots.iter().enumerate() {
            let offer = slot.offers(kind, wish);
            if offer == Offer::No {
                continue;
            }
            // Somewhere to go if this one fails. Without it, a camera that
            // fails its first attempt keeps its chance until the deadline (it
            // may refuse one connection and take the next); a source that
            // reads this server does not, its failure being the server's own.
            let fallback = self.slots.iter().skip(index + 1).any(|s| s.offers(kind, wish) != Offer::No);
            let decisive = fallback || slot.is_local();
            let proven = slot.channel.tracks().is_some();
            if offer == Offer::Yes && (proven || !fallback) {
                return Some(index);
            }
            if decisive && slot.has_failed() {
                continue;
            }
            probes.entry(index).or_insert_with(|| (slot.acquire(), slot.failures()));
            match prove(slot, decisive, deadline).await {
                Proof::Tracks if slot.offers(kind, wish) == Offer::Yes => return Some(index),
                // Declared, still starting: taken on its word, as before.
                Proof::Late if offer == Offer::Yes => return Some(index),
                Proof::Tracks | Proof::Failed | Proof::Late => {}
            }
        }
        None
    }

    /// The stream as it is now.
    pub fn snapshot(&self) -> StreamSnapshot {
        let sources = self
            .slots
            .iter()
            .map(|slot| SourceSnapshot {
                url: slot.spec.redacted(),
                state: slot.state(),
                tracks: slot.channel.tracks().map(|t| t.to_vec()).unwrap_or_default(),
                track_frames: slot.channel.track_frames(),
                frames: slot.channel.counters.frames.load(Ordering::Relaxed),
                bytes: slot.channel.counters.bytes.load(Ordering::Relaxed),
                rejected: slot.channel.counters.rejected.load(Ordering::Relaxed),
                demand: slot.demand(),
                failures: slot.failures(),
                last_error: slot.last_error(),
                last_frame_age: slot.channel.last_frame().map(|at| at.elapsed()),
            })
            .collect();
        let consumers = self
            .consumers_lock()
            .iter()
            .map(|c| ConsumerSnapshot {
                id: c.id,
                kind: c.tag.kind.clone(),
                remote: c.tag.remote.clone(),
                frames: c.stats.frames.load(Ordering::Relaxed),
                bytes: c.stats.bytes.load(Ordering::Relaxed),
                dropped: c.stats.dropped.load(Ordering::Relaxed),
            })
            .collect();
        StreamSnapshot { name: self.name.clone(), sources, consumers }
    }
}

/// What a started source showed before the deadline.
enum Proof {
    /// Its tracks.
    Tracks,
    /// A failed attempt (only reported when it is decisive).
    Failed,
    /// Neither, by the deadline.
    Late,
}

/// Waits for a started source's tracks, or with `decisive` for its first
/// failed attempt too.
async fn prove(slot: &Slot, decisive: bool, deadline: Instant) -> Proof {
    let mut tracks = slot.channel.watch_tracks();
    let mut failures = slot.watch_failures();
    let before = *failures.borrow_and_update();
    let wait = async {
        loop {
            if tracks.borrow_and_update().is_some() {
                return Proof::Tracks;
            }
            if decisive && *failures.borrow_and_update() > before {
                return Proof::Failed;
            }
            tokio::select! {
                changed = tracks.changed() => if changed.is_err() { return Proof::Late },
                changed = failures.changed() => if changed.is_err() { return Proof::Late },
            }
        }
    };
    tokio::time::timeout_at(deadline, wait).await.unwrap_or(Proof::Late)
}
