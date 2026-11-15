//! A named stream: an ordered list of sources and the consumers reading them.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, Weak};

use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use crate::error::SubscribeError;
use crate::hub::Hub;
use crate::info::{ConsumerSnapshot, SourceSnapshot, SourceState, StreamSnapshot};
use crate::limits::Limits;
use crate::local::Alias;
use crate::select::{self, Pick};
use crate::slot::Slot;
use crate::spec::SourceSpec;
use crate::subscription::Subscription;
use crate::want::Want;

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

/// One source of a stream: run by its driver, or an alias read through the
/// stream it names ([`crate::local`]).
pub(crate) enum Source {
    Slot(Arc<Slot>),
    Alias { spec: SourceSpec, alias: Alias },
}

impl Source {
    fn spec(&self) -> &SourceSpec {
        match self {
            Self::Slot(slot) => &slot.spec,
            Self::Alias { spec, .. } => spec,
        }
    }
}

/// A named stream.
pub struct Stream {
    name: String,
    sources: Vec<Source>,
    hub: Weak<Hub>,
    consumers: Mutex<Vec<ConsumerEntry>>,
    next_consumer: AtomicU64,
    pub(crate) closed: CancellationToken,
    pub(crate) limits: Limits,
}

impl std::fmt::Debug for Stream {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Stream").field("name", &self.name).field("sources", &self.sources.len()).finish()
    }
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
    pub(crate) fn new(name: String, sources: Vec<Source>, limits: Limits, hub: Weak<Hub>) -> Arc<Self> {
        Arc::new(Self {
            name,
            sources,
            hub,
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
        self.sources.iter().map(Source::spec)
    }

    pub(crate) fn sources(&self) -> &[Source] {
        &self.sources
    }

    pub(crate) fn hub(&self) -> Option<Arc<Hub>> {
        self.hub.upgrade()
    }

    /// Its aliases, with each one's position.
    pub(crate) fn aliases(&self) -> impl Iterator<Item = (usize, &Alias)> {
        self.sources.iter().enumerate().filter_map(|(index, source)| match source {
            Source::Alias { alias, .. } => Some((index, alias)),
            Source::Slot(_) => None,
        })
    }

    /// Whether the stream was removed or replaced.
    pub fn is_closed(&self) -> bool {
        self.closed.is_cancelled()
    }

    pub(crate) fn close(&self) {
        self.closed.cancel();
        for source in &self.sources {
            if let Source::Slot(slot) = source {
                slot.shutdown();
            }
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

    /// The sources `want` reads, unless the stream closes first.
    pub(crate) async fn select(self: &Arc<Self>, want: &Want, deadline: Instant) -> Result<Vec<Pick>, SubscribeError> {
        if want.is_empty() {
            return Err(SubscribeError::NothingWanted);
        }
        let closed = self.closed.clone();
        tokio::select! {
            biased;
            () = closed.cancelled() => Err(SubscribeError::Closed),
            picks = select::select(self, want, deadline) => picks,
        }
    }

    /// The stream as it is now. An alias shows the source it reads in the
    /// stream it names (its state and counters), with the tracks it passes.
    pub fn snapshot(&self) -> StreamSnapshot {
        let sources = self
            .sources
            .iter()
            .map(|source| match source {
                Source::Slot(slot) => slot_snapshot(&slot.spec, slot),
                Source::Alias { spec, alias } => self.alias_snapshot(spec, alias),
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

    fn alias_snapshot(&self, spec: &SourceSpec, alias: &Alias) -> SourceSnapshot {
        let reads = Some(alias.target().to_owned());
        match select::alias_view(self, alias) {
            Ok((slot, kinds)) => {
                let mut snapshot = slot_snapshot(spec, &slot);
                let passed: Vec<bool> = snapshot
                    .tracks
                    .iter()
                    .map(|t| kinds.iter().any(|(kind, wish)| *kind == t.kind && wish.accepts(t.codec)))
                    .collect();
                let mut keep = passed.iter().copied();
                snapshot.tracks.retain(|_| keep.next().unwrap_or(false));
                let mut keep = passed.iter().copied();
                snapshot.track_frames.retain(|_| keep.next().unwrap_or(false));
                SourceSnapshot { reads, ..snapshot }
            }
            Err(why) => SourceSnapshot {
                url: spec.redacted(),
                state: SourceState::Idle,
                tracks: Vec::new(),
                track_frames: Vec::new(),
                frames: 0,
                bytes: 0,
                rejected: 0,
                demand: 0,
                failures: 0,
                last_error: Some(why),
                last_frame_age: None,
                reads,
            },
        }
    }
}

fn slot_snapshot(spec: &SourceSpec, slot: &Slot) -> SourceSnapshot {
    SourceSnapshot {
        url: spec.redacted(),
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
        reads: None,
    }
}
