//! The registry of streams.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, PoisonError, RwLock, RwLockReadGuard, RwLockWriteGuard, Weak};

use tokio::time::Instant;

use crate::driver::{Drivers, FactoryCtx};
use crate::error::{HubError, SpecError};
use crate::limits::Limits;
use crate::slot::Slot;
use crate::spec::SourceSpec;
use crate::stream::{Source, Stream};

/// Longest stream name.
pub const MAX_NAME_LEN: usize = 128;

/// All the streams of one server.
pub struct Hub {
    streams: RwLock<BTreeMap<String, Arc<Stream>>>,
    drivers: Drivers,
    limits: Limits,
    /// Where every track's timeline counts from ([`crate::timeline`]): one
    /// time base for every source of the hub.
    epoch: Instant,
    me: Weak<Hub>,
}

impl std::fmt::Debug for Hub {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Hub").field("streams", &self.len()).finish()
    }
}

/// Checks a stream name: 1 to 128 characters of `A-Z a-z 0-9 _ . -`.
pub fn validate_name(name: &str) -> Result<(), HubError> {
    let valid = !name.is_empty()
        && name.len() <= MAX_NAME_LEN
        && name.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'-'));
    if valid { Ok(()) } else { Err(HubError::BadName) }
}

impl Hub {
    /// A hub building sources with `drivers`.
    pub fn new(drivers: Drivers, limits: Limits) -> Arc<Self> {
        Arc::new_cyclic(|me| Self {
            streams: RwLock::new(BTreeMap::new()),
            drivers,
            limits,
            epoch: Instant::now(),
            me: me.clone(),
        })
    }

    /// The instant every track's timeline counts from: a frame received then
    /// would be at 0 ([`crate::timeline`]).
    pub fn epoch(&self) -> Instant {
        self.epoch
    }

    /// The limits every stream of this hub enforces.
    pub fn limits(&self) -> &Limits {
        &self.limits
    }

    fn read(&self) -> RwLockReadGuard<'_, BTreeMap<String, Arc<Stream>>> {
        self.streams.read().unwrap_or_else(PoisonError::into_inner)
    }

    fn write(&self) -> RwLockWriteGuard<'_, BTreeMap<String, Arc<Stream>>> {
        self.streams.write().unwrap_or_else(PoisonError::into_inner)
    }

    /// Creates or replaces a stream. Replacing a stream with the very same
    /// sources changes nothing (its consumers keep watching); replacing it
    /// with different ones closes the old stream, and its consumers end
    /// (those reading it through an alias move to the new one). A stream
    /// whose aliases would lead back to it is refused ([`SpecError::Cycle`]).
    pub fn upsert<S: AsRef<str>>(&self, name: &str, sources: &[S]) -> Result<Arc<Stream>, HubError> {
        validate_name(name)?;
        if sources.is_empty() {
            return Err(HubError::NoSources);
        }
        if sources.len() > self.limits.max_sources_per_stream {
            return Err(HubError::TooManySources);
        }
        let specs = sources
            .iter()
            .enumerate()
            .map(|(index, raw)| SourceSpec::parse(raw.as_ref()).map_err(|error| HubError::Spec { index, error }))
            .collect::<Result<Vec<_>, _>>()?;
        if let Some(existing) = self.get(name)
            && existing.specs().eq(specs.iter())
        {
            return Ok(existing);
        }
        let ctx = FactoryCtx { hub: self.me.clone(), stream: name.to_owned() };
        let sources = specs
            .into_iter()
            .enumerate()
            .map(|(index, spec)| {
                let driver = self.drivers.build(&spec, &ctx).map_err(|error| HubError::Spec { index, error })?;
                Ok(match driver.alias() {
                    Some(alias) => Source::Alias { spec, alias: alias.clone() },
                    None => Source::Slot(Slot::new(spec, driver, &self.limits, self.epoch)),
                })
            })
            .collect::<Result<Vec<_>, HubError>>()?;
        let stream = Stream::new(name.to_owned(), sources, self.limits.clone(), self.me.clone());
        let previous = {
            let mut streams = self.write();
            if !streams.contains_key(name) && streams.len() >= self.limits.max_streams {
                return Err(HubError::TooManyStreams);
            }
            // Under the write lock: two upserts that each close half of a
            // cycle cannot both pass.
            if let Some((index, path)) = cycle(&streams, &stream) {
                return Err(HubError::Spec { index, error: SpecError::Cycle(path) });
            }
            streams.insert(name.to_owned(), stream.clone())
        };
        if let Some(previous) = previous {
            previous.close();
        }
        Ok(stream)
    }

    /// Removes a stream; its consumers end. Whether it existed.
    pub fn remove(&self, name: &str) -> bool {
        let removed = self.write().remove(name);
        match removed {
            Some(stream) => {
                stream.close();
                true
            }
            None => false,
        }
    }

    /// A stream by name.
    pub fn get(&self, name: &str) -> Option<Arc<Stream>> {
        self.read().get(name).cloned()
    }

    /// Every stream, by name.
    pub fn streams(&self) -> Vec<Arc<Stream>> {
        self.read().values().cloned().collect()
    }

    /// Number of streams.
    pub fn len(&self) -> usize {
        self.read().len()
    }

    /// Whether there is no stream.
    pub fn is_empty(&self) -> bool {
        self.read().is_empty()
    }

    /// Closes every stream.
    pub fn shutdown(&self) {
        let streams = std::mem::take(&mut *self.write());
        for stream in streams.values() {
            stream.close();
        }
    }
}

/// The first alias of `stream` through which it would read itself, with the
/// path from that alias back to it (`b -> c -> a`). Each stream is entered
/// once per alias of `stream`, so the walk is bounded by the hub's streams.
fn cycle(streams: &BTreeMap<String, Arc<Stream>>, stream: &Stream) -> Option<(usize, String)> {
    let name = stream.name();
    for (index, first) in stream.aliases() {
        let mut seen = BTreeSet::from([first]);
        let mut paths: Vec<Vec<&str>> = vec![vec![first]];
        while let Some(path) = paths.pop() {
            let Some(&last) = path.last() else { continue };
            if last == name {
                return Some((index, path.join(" -> ")));
            }
            let Some(next) = streams.get(last) else { continue };
            for (_, target) in next.aliases() {
                if seen.insert(target) {
                    let mut longer = path.clone();
                    longer.push(target);
                    paths.push(longer);
                }
            }
        }
    }
    None
}
