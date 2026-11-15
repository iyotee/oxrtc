//! Preloads: streams held open with no viewer, so the next one starts at once.
//!
//! A preload is a [`Subscription`] tagged `preload`, drained by a background
//! task that discards its frames. Each stream has at most one; a second `PUT`
//! replaces the first. There is no lock held across streams: the registry's
//! mutex is only taken for map updates, never across an `.await`, so one
//! camera's slow start never delays another's preload.

use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, Weak};
use std::time::Duration;

use oxrtc_core::{
    CancellationToken, ConsumerTag, Event, MediaKind, Stream, SubEnd, SubscribeError, Subscription, TrackInfo, Want,
};
use serde::Serialize;
use thiserror::Error;

use crate::info::preload_medias;

/// How long a `PUT /api/preload` waits for the stream's tracks.
pub const PRELOAD_WAIT: Duration = Duration::from_secs(30);

/// Why a preload could not be held.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub(crate) enum PreloadError {
    #[error("too many preloads")]
    TooMany,
    #[error("preload replaced or released before it started")]
    Cancelled,
    #[error("{0}")]
    Subscribe(#[from] SubscribeError),
    #[error("no track within {0:?}")]
    TimedOut(Duration),
    #[error("stream closed")]
    Closed,
    #[error("no source delivered a wanted track")]
    NoTracks,
    #[error("the stream has no {0} track matching the request")]
    Missing(&'static str),
}

struct Entry {
    generation: u64,
    query: String,
    medias: Vec<String>,
    held: bool,
    cancel: CancellationToken,
    stream: Weak<Stream>,
}

/// One line of `GET /api/preload`.
#[derive(Debug, Serialize)]
pub(crate) struct PreloadInfo {
    query: String,
    medias: Vec<String>,
    held: bool,
}

/// The preloads of one server, by stream name.
pub(crate) struct Preloads {
    entries: Mutex<HashMap<String, Entry>>,
    next: AtomicU64,
    max: usize,
}

impl Drop for Preloads {
    /// The registry goes with the last router holding it: its preloads end
    /// too, instead of holding cameras open with nothing left to release them.
    fn drop(&mut self) {
        let entries = self.entries.get_mut().unwrap_or_else(PoisonError::into_inner);
        for (_, entry) in entries.drain() {
            entry.cancel.cancel();
        }
    }
}

/// Removes a starting preload's entry when its request goes away before the
/// preload is held (failure, client gone, request timeout).
struct Pending {
    preloads: Arc<Preloads>,
    name: String,
    generation: u64,
    armed: bool,
}

impl Drop for Pending {
    fn drop(&mut self) {
        if self.armed {
            self.preloads.remove_if(&self.name, self.generation);
        }
    }
}

impl Preloads {
    /// A registry of at most `max` preloads.
    pub(crate) fn new(max: usize) -> Self {
        Self { entries: Mutex::new(HashMap::new()), next: AtomicU64::new(1), max }
    }

    fn lock(&self) -> MutexGuard<'_, HashMap<String, Entry>> {
        self.entries.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn remove_if(&self, name: &str, generation: u64) {
        let mut entries = self.lock();
        if entries.get(name).is_some_and(|e| e.generation == generation) {
            entries.remove(name);
        }
    }

    /// Number of preloads, starting or held.
    pub(crate) fn len(&self) -> usize {
        self.lock().len()
    }

    /// Starts holding `stream` with `want`, replacing its earlier preload, and
    /// waits (at most `wait`) until every media `query` names has a track (any
    /// track, when it names none). Returns the medias it holds.
    pub(crate) async fn start(
        self: &Arc<Self>,
        stream: Arc<Stream>,
        want: Want,
        query: String,
        wait: Duration,
    ) -> Result<Vec<String>, PreloadError> {
        let name = stream.name().to_owned();
        // A query naming no media wants whatever the stream has.
        let strict = !query.is_empty();
        let generation = self.next.fetch_add(1, Ordering::Relaxed);
        let cancel = CancellationToken::new();
        {
            let mut entries = self.lock();
            if !entries.contains_key(&name) && entries.len() >= self.max {
                return Err(PreloadError::TooMany);
            }
            let entry = Entry {
                generation,
                query,
                medias: preload_medias(&want, None),
                held: false,
                cancel: cancel.clone(),
                stream: Arc::downgrade(&stream),
            };
            if let Some(old) = entries.insert(name.clone(), entry) {
                old.cancel.cancel();
            }
        }
        let mut pending = Pending { preloads: self.clone(), name: name.clone(), generation, armed: true };
        let attempt = tokio::time::timeout(wait, first_tracks(&stream, &want));
        let (subscription, tracks) = tokio::select! {
            biased;
            () = cancel.cancelled() => return Err(PreloadError::Cancelled),
            outcome = attempt => outcome.map_err(|_| PreloadError::TimedOut(wait))??,
        };
        for kind in MediaKind::ALL {
            let wish = want.get(kind);
            if strict && wish.is_wanted() && !tracks.iter().any(|t| t.kind == kind && wish.accepts(t.codec)) {
                return Err(PreloadError::Missing(kind.as_str()));
            }
        }
        let medias = preload_medias(&want, Some(&tracks));
        {
            let mut entries = self.lock();
            let Some(entry) = entries.get_mut(&name).filter(|e| e.generation == generation) else {
                return Err(PreloadError::Cancelled);
            };
            entry.held = true;
            entry.medias = medias.clone();
        }
        pending.armed = false;
        tokio::spawn(drain(Arc::downgrade(self), name, generation, subscription, cancel));
        Ok(medias)
    }

    /// Releases a stream's preload. Whether there was one.
    pub(crate) fn release(&self, name: &str) -> bool {
        let removed = self.lock().remove(name);
        removed.map(|e| e.cancel.cancel()).is_some()
    }

    /// What the preload of `stream` lists as its medias, if it has one.
    pub(crate) fn medias_of(&self, stream: &Arc<Stream>) -> Option<Vec<String>> {
        let entries = self.lock();
        let entry = entries.get(stream.name())?;
        let same = entry.stream.upgrade().is_some_and(|s| Arc::ptr_eq(&s, stream));
        same.then(|| entry.medias.clone())
    }

    /// Every preload, by stream name.
    pub(crate) fn list(&self) -> BTreeMap<String, PreloadInfo> {
        self.lock()
            .iter()
            .map(|(name, e)| {
                (name.clone(), PreloadInfo { query: e.query.clone(), medias: e.medias.clone(), held: e.held })
            })
            .collect()
    }

    /// Releases every preload.
    pub(crate) fn clear(&self) {
        for (_, entry) in self.lock().drain() {
            entry.cancel.cancel();
        }
    }
}

async fn first_tracks(stream: &Arc<Stream>, want: &Want) -> Result<(Subscription, Arc<[TrackInfo]>), PreloadError> {
    let mut subscription = stream.subscribe(want, ConsumerTag::new("preload")).await?;
    loop {
        match subscription.recv().await {
            Ok(Event::Tracks(tracks)) => return Ok((subscription, tracks)),
            Ok(_) => continue,
            Err(SubEnd::Closed) => return Err(PreloadError::Closed),
            Err(SubEnd::NoTracks) => return Err(PreloadError::NoTracks),
        }
    }
}

/// Reads and discards a held preload's events until it is released, the
/// stream closes, or the registry itself is dropped (it holds only a weak
/// reference, so that dropping the router releases every preload).
async fn drain(
    preloads: Weak<Preloads>,
    name: String,
    generation: u64,
    mut subscription: Subscription,
    cancel: CancellationToken,
) {
    loop {
        tokio::select! {
            biased;
            () = cancel.cancelled() => break,
            event = subscription.recv() => if event.is_err() { break },
        }
    }
    drop(subscription);
    if let Some(preloads) = preloads.upgrade() {
        preloads.remove_if(&name, generation);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxrtc_core::{
        BoxFuture, Drivers, FactoryCtx, Hub, Limits, ProducerSink, SourceDriver, SourceError, SourceFactory,
        SourceSpec, SpecError,
    };

    /// `hang:` sources connect forever: a preload of one stays pending. (A
    /// `stream:` of a missing stream would not: this server's own refusal is
    /// final at once.)
    struct Hang;

    impl SourceFactory for Hang {
        fn schemes(&self) -> &[&'static str] {
            &["hang"]
        }
        fn build(&self, _spec: &SourceSpec, _ctx: &FactoryCtx) -> Result<Arc<dyn SourceDriver>, SpecError> {
            Ok(Arc::new(Hang))
        }
    }

    impl SourceDriver for Hang {
        fn run(
            self: Arc<Self>,
            _sink: ProducerSink,
            _cancel: CancellationToken,
        ) -> BoxFuture<'static, Result<(), SourceError>> {
            Box::pin(std::future::pending())
        }
    }

    #[tokio::test]
    async fn the_registry_is_bounded() {
        let limits = Limits { probe_timeout: Duration::from_secs(5), ..Limits::default() };
        let mut drivers = Drivers::new();
        drivers.register(Arc::new(Hang));
        let hub = Hub::new(drivers, limits);
        let a = hub.upsert("a", &["hang:camera"]).unwrap();
        let b = hub.upsert("b", &["hang:camera"]).unwrap();

        let none = Arc::new(Preloads::new(0));
        let refused = none.start(a.clone(), Want::all(), String::new(), PRELOAD_WAIT).await;
        assert_eq!(refused.err(), Some(PreloadError::TooMany));

        let one = Arc::new(Preloads::new(1));
        let pending = tokio::spawn({
            let one = one.clone();
            let a = a.clone();
            async move { one.start(a, Want::all(), String::new(), Duration::from_secs(3)).await }
        });
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(one.len(), 1, "a starting preload counts");
        let refused = one.start(b, Want::all(), String::new(), PRELOAD_WAIT).await;
        assert_eq!(refused.err(), Some(PreloadError::TooMany));
        // Replacing the same stream is not a new entry.
        let replacing = tokio::spawn({
            let one = one.clone();
            async move { one.start(a, Want::all(), String::new(), Duration::from_millis(100)).await }
        });
        assert_eq!(pending.await.unwrap().err(), Some(PreloadError::Cancelled));
        assert!(matches!(replacing.await.unwrap(), Err(PreloadError::TimedOut(_))));
        assert_eq!(one.len(), 0, "failed preloads leave no entry");
    }
}
