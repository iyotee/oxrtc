//! Streams made from a stream by a source of their own, which nobody names
//! in a configuration: the Motion JPEG transcode of a stream whose sources
//! offer no JPEG (`ffmpeg:<stream>#video=mjpeg`, M5 F5, D46).
//!
//! A derived stream is kept with the stream it is made from, under a key
//! (`mjpeg`), so that every reader of it shares one source: one FFmpeg for
//! every Motion JPEG reader of a camera. Its source is an ordinary slot, run
//! on demand as every source is: started by its first reader, stopped after
//! its idle grace when the last leaves (a transcode's, D36), restarted on the
//! backoff when it fails. It is not in the hub's list (no route names it, and
//! no operator's stream can take its name, which holds a `#`), and it is
//! closed with the stream it is made from: a stream replaced by other
//! sources ends its derived streams' readers, as its own. The stream's
//! listing shows a derived stream's source and readers among its own.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use crate::driver::FactoryCtx;
use crate::error::{HubError, SpecError};
use crate::hub::Hub;
use crate::slot::Slot;
use crate::spec::SourceSpec;
use crate::stream::{Source, Stream};

/// Most derived streams one stream keeps (one per key).
pub const MAX_DERIVED: usize = 4;

/// The derived streams of one stream, by key.
#[derive(Default)]
pub(crate) struct Derived {
    streams: Mutex<BTreeMap<String, Arc<Stream>>>,
}

impl Derived {
    fn lock(&self) -> MutexGuard<'_, BTreeMap<String, Arc<Stream>>> {
        self.streams.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Every derived stream, by key.
    pub(crate) fn all(&self) -> Vec<Arc<Stream>> {
        self.lock().values().cloned().collect()
    }

    /// Closes them all (their stream closes).
    pub(crate) fn close(&self) {
        let streams = std::mem::take(&mut *self.lock());
        for stream in streams.values() {
            stream.close();
        }
    }
}

impl Hub {
    /// The stream made from `base` by `source` alone, kept with `base` under
    /// `key`: the same one for every caller while `base` lasts, so that its
    /// readers share one source. Its name is `base`'s with `#key` after it.
    /// Refused for a source that is not one (an alias), for more than
    /// [`MAX_DERIVED`] keys, and once `base` is closed.
    pub fn derived(&self, base: &Arc<Stream>, key: &str, source: &str) -> Result<Arc<Stream>, HubError> {
        let invalid = |what: &str| HubError::Spec { index: 0, error: SpecError::Invalid(what.to_owned()) };
        if let Some(found) = base.derived.lock().get(key).filter(|s| !s.is_closed()) {
            return Ok(found.clone());
        }
        let spec = SourceSpec::parse(source).map_err(|error| HubError::Spec { index: 0, error })?;
        let name = format!("{}#{key}", base.name());
        let ctx = FactoryCtx { hub: self.me(), stream: name.clone() };
        let driver = self.drivers().build(&spec, &ctx).map_err(|error| HubError::Spec { index: 0, error })?;
        if driver.alias().is_some() {
            return Err(invalid("a derived stream needs a source of its own, not an alias"));
        }
        let slot = Slot::new(spec, driver, self.limits(), self.epoch());
        let stream = Stream::new(name, vec![Source::Slot(slot)], self.limits().clone(), self.me());
        let mut derived = base.derived.lock();
        // Under the lock `close` takes: a base closed now closes nothing
        // added after it.
        if base.is_closed() {
            stream.close();
            return Err(invalid("the stream was removed or replaced"));
        }
        if let Some(found) = derived.get(key).filter(|s| !s.is_closed()) {
            stream.close();
            return Ok(found.clone());
        }
        if !derived.contains_key(key) && derived.len() >= MAX_DERIVED {
            stream.close();
            return Err(invalid("too many derived streams"));
        }
        derived.insert(key.to_owned(), stream.clone());
        Ok(stream)
    }
}
