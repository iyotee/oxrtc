//! Preloads: streams held open with no viewer, so the next one starts at once.
//!
//! A preload is a [`Subscription`] tagged `preload`, drained by a background
//! task that discards its frames. Each stream has at most one hold; a second
//! `PUT` replaces it once the new one holds, and only then is the old one
//! released, so the camera is never left unheld between the two (go2rtc
//! removes the old hold first). A new request that fails leaves the old hold
//! in place. Of two requests still starting, the newer wins. There is no
//! lock held across streams: the registry's mutex is only taken for map
//! updates, never across an `.await`, so one camera's slow start never
//! delays another's preload.

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

/// One preload request: the one that holds, or the one starting.
struct Preload {
    generation: u64,
    query: String,
    medias: Vec<String>,
    cancel: CancellationToken,
    stream: Weak<Stream>,
}

impl Preload {
    fn of(&self, stream: &Arc<Stream>) -> bool {
        self.stream.upgrade().is_some_and(|s| Arc::ptr_eq(&s, stream))
    }
}

/// A stream's preloads: its hold, and the request that will replace it.
#[derive(Default)]
struct Entry {
    held: Option<Preload>,
    starting: Option<Preload>,
}

impl Entry {
    fn is_empty(&self) -> bool {
        self.held.is_none() && self.starting.is_none()
    }

    fn cancel(&self) {
        for preload in [&self.held, &self.starting].into_iter().flatten() {
            preload.cancel.cancel();
        }
    }
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
            entry.cancel();
        }
    }
}

/// Removes a starting preload when its request goes away before it holds
/// (failure, client gone, request timeout); the hold it was to replace stays.
struct Pending {
    preloads: Arc<Preloads>,
    name: String,
    generation: u64,
    armed: bool,
}

impl Drop for Pending {
    fn drop(&mut self) {
        if self.armed {
            self.preloads.forget(&self.name, self.generation);
        }
    }
}

impl Preloads {
    /// A registry of at most `max` preloaded streams.
    pub(crate) fn new(max: usize) -> Self {
        Self { entries: Mutex::new(HashMap::new()), next: AtomicU64::new(1), max }
    }

    fn lock(&self) -> MutexGuard<'_, HashMap<String, Entry>> {
        self.entries.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Forgets preload `generation` of `name`, held or starting.
    fn forget(&self, name: &str, generation: u64) {
        let mut entries = self.lock();
        let Some(entry) = entries.get_mut(name) else { return };
        for slot in [&mut entry.held, &mut entry.starting] {
            if slot.as_ref().is_some_and(|p| p.generation == generation) {
                *slot = None;
            }
        }
        if entry.is_empty() {
            entries.remove(name);
        }
    }

    /// Number of preloaded streams, starting or held.
    pub(crate) fn len(&self) -> usize {
        self.lock().len()
    }

    /// Starts holding `stream` with `want` and waits (at most `wait`) until
    /// every media `query` names has a track (any track, when it names none).
    /// Then it replaces the stream's earlier hold, which is released only
    /// now. Returns the medias it holds.
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
            let starting = Preload {
                generation,
                query,
                medias: preload_medias(&want, None),
                cancel: cancel.clone(),
                stream: Arc::downgrade(&stream),
            };
            // Of two requests starting, the newer wins; the hold stays.
            if let Some(older) = entries.entry(name.clone()).or_default().starting.replace(starting) {
                older.cancel.cancel();
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
        let replaced = {
            let mut entries = self.lock();
            let Some(entry) = entries.get_mut(&name) else { return Err(PreloadError::Cancelled) };
            let Some(mut held) = entry.starting.take_if(|p| p.generation == generation) else {
                return Err(PreloadError::Cancelled);
            };
            held.medias = medias.clone();
            entry.held.replace(held)
        };
        pending.armed = false;
        tokio::spawn(drain(Arc::downgrade(self), name, generation, subscription, cancel));
        // The new hold's subscription holds the stream already: no gap.
        if let Some(old) = replaced {
            old.cancel.cancel();
        }
        Ok(medias)
    }

    /// Releases a stream's preload, and the one starting. Whether there was one.
    pub(crate) fn release(&self, name: &str) -> bool {
        let removed = self.lock().remove(name);
        removed.map(|e| e.cancel()).is_some()
    }

    /// What the preload of `stream` lists as its medias, if it has one: its
    /// hold's, else those of the request starting.
    pub(crate) fn medias_of(&self, stream: &Arc<Stream>) -> Option<Vec<String>> {
        let entries = self.lock();
        let entry = entries.get(stream.name())?;
        [&entry.held, &entry.starting].into_iter().flatten().find(|p| p.of(stream)).map(|p| p.medias.clone())
    }

    /// Every preloaded stream, by name: its hold, else the request starting.
    pub(crate) fn list(&self) -> BTreeMap<String, PreloadInfo> {
        let line = |p: &Preload, held| PreloadInfo { query: p.query.clone(), medias: p.medias.clone(), held };
        self.lock()
            .iter()
            .filter_map(|(name, e)| {
                let info = match (&e.held, &e.starting) {
                    (Some(held), _) => line(held, true),
                    (None, Some(starting)) => line(starting, false),
                    (None, None) => return None,
                };
                Some((name.clone(), info))
            })
            .collect()
    }

    /// Releases every preload.
    pub(crate) fn clear(&self) {
        for (_, entry) in self.lock().drain() {
            entry.cancel();
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
        preloads.forget(&name, generation);
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
