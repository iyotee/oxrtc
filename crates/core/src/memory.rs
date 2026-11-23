//! One memory budget for every reader that writes over HTTP or a WebSocket
//! (`stream.mp4`, MSE, and later HLS and MJPEG): M5 item D (6), D45.
//!
//! Each consumer's queue is bounded by its class ([`crate::queue`]): 8 MiB
//! for a live reader, 32 MiB for a recorder. Alone, those caps let 64
//! responses of `queue=recorder` hold 2 GiB, more than a 4 GiB Raspberry Pi
//! can give. The budget is the process-wide bound above them, sized from
//! what the host gives the process ([`crate::host_memory`]): a fraction of
//! the memory available to it, refreshed so that it shrinks under memory
//! pressure and grows back, or a size the operator sets.
//!
//! A queue that draws from it is bounded by the smaller of its class's cap
//! and its fair share (the budget over the readers drawing from it), never
//! below one group of pictures of its stream, so that a reader is never cut
//! so short it skips for ever. When the budget is full all the same (the
//! groups of many readers), the reader furthest behind (the oldest frame
//! waiting) skips to its next keyframe, by the queue's own rule, with its
//! counter and its log line; the source is never made to wait, and a reader
//! that keeps up holds next to nothing and is never the one chosen.

use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError, Weak};
use std::time::Duration;

use crate::host_memory::{HostMemory, HostReading, MemorySource};
use crate::queue::ConsumerQueue;
use crate::refusals::Refusals;

/// The default share of the memory available to the process: one eighth
/// (D45: 290 MiB on a 4 GiB Raspberry Pi with openIVR beside, about 800 MiB
/// on an 8 GiB one).
pub const DEFAULT_FRACTION: f64 = 0.125;

/// The smallest budget: room for the groups of pictures of a few readers of
/// a 4 Mbit/s camera with 6.4 s groups (3.2 MB each).
pub const MIN_BUDGET: u64 = 32 * 1024 * 1024;

/// How often the server reads the host's memory again.
pub const REFRESH_EVERY: Duration = Duration::from_secs(5);

/// Shortest time between two log lines of the skips the budget forced.
const SKIP_REPORT_EVERY: Duration = Duration::from_secs(10);

/// How large the budget is.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum BudgetSize {
    /// This share of what the process may still take, plus what the budget
    /// holds (its own bytes are not taken from it twice), and never more
    /// than this share of the process's limit (`api.output_memory: 12.5%`).
    Fraction(f64),
    /// This many bytes, whatever the host (`api.output_memory: 256MiB`), but
    /// never more than the process may take.
    Bytes(u64),
}

impl Default for BudgetSize {
    fn default() -> Self {
        Self::Fraction(DEFAULT_FRACTION)
    }
}

/// What the budget is now, for `GET /api`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BudgetSnapshot {
    /// Its size, in bytes.
    pub budget: u64,
    /// The bytes its readers hold.
    pub held: u64,
    /// Its readers.
    pub readers: usize,
    /// The readers it made skip to their next keyframe since the start.
    pub skips: u64,
    /// The process's limit and what it could still take, at the last reading.
    pub reading: Option<HostReading>,
}

/// The process-wide budget.
pub struct MemoryBudget {
    size: BudgetSize,
    host: Box<dyn HostMemory>,
    total: AtomicU64,
    held: AtomicU64,
    /// The queues drawing from it, counted as they come and go (a share is
    /// computed at every push: no lock for it).
    readers: AtomicUsize,
    skips: AtomicU64,
    reading: Mutex<Option<HostReading>>,
    queues: Mutex<Vec<Weak<ConsumerQueue>>>,
    reports: Refusals,
}

impl std::fmt::Debug for MemoryBudget {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MemoryBudget").field("size", &self.size).field("snapshot", &self.snapshot()).finish()
    }
}

impl MemoryBudget {
    /// A budget of `size` over what `host` reads, read once now.
    pub fn new(size: BudgetSize, host: Box<dyn HostMemory>) -> Arc<Self> {
        let budget = Arc::new(Self {
            size,
            host,
            total: AtomicU64::new(MIN_BUDGET),
            held: AtomicU64::new(0),
            readers: AtomicUsize::new(0),
            skips: AtomicU64::new(0),
            reading: Mutex::new(None),
            queues: Mutex::new(Vec::new()),
            reports: Refusals::every(SKIP_REPORT_EVERY),
        });
        budget.refresh();
        budget
    }

    /// Reads the host again and sizes the budget from it; returns the size.
    pub fn refresh(&self) -> u64 {
        let reading = self.host.read();
        let held = self.held.load(Ordering::Relaxed);
        let total = size_from(self.size, reading, held);
        self.total.store(total, Ordering::Relaxed);
        *self.reading.lock().unwrap_or_else(PoisonError::into_inner) = reading;
        total
    }

    /// Where its numbers stand.
    pub fn snapshot(&self) -> BudgetSnapshot {
        BudgetSnapshot {
            budget: self.total.load(Ordering::Relaxed),
            held: self.held.load(Ordering::Relaxed),
            readers: self.readers(),
            skips: self.skips.load(Ordering::Relaxed),
            reading: *self.reading.lock().unwrap_or_else(PoisonError::into_inner),
        }
    }

    fn readers(&self) -> usize {
        self.readers.load(Ordering::Relaxed)
    }

    /// What one reader may hold: the budget over its readers.
    pub(crate) fn share(&self) -> usize {
        let readers = u64::try_from(self.readers().max(1)).unwrap_or(u64::MAX);
        usize::try_from(self.total.load(Ordering::Relaxed) / readers).unwrap_or(usize::MAX)
    }

    pub(crate) fn register(&self, queue: &Arc<ConsumerQueue>) {
        let mut queues = self.queues.lock().unwrap_or_else(PoisonError::into_inner);
        queues.retain(|q| q.strong_count() > 0);
        queues.push(Arc::downgrade(queue));
        self.readers.fetch_add(1, Ordering::Relaxed);
    }

    /// A queue that drew from it is gone, with the bytes it still held.
    pub(crate) fn unregister(&self, held: usize) {
        self.sub(held);
        let _ = self.readers.try_update(Ordering::Relaxed, Ordering::Relaxed, |n| Some(n.saturating_sub(1)));
    }

    pub(crate) fn add(&self, bytes: usize) {
        self.held.fetch_add(u64::try_from(bytes).unwrap_or(u64::MAX), Ordering::Relaxed);
    }

    pub(crate) fn sub(&self, bytes: usize) {
        let bytes = u64::try_from(bytes).unwrap_or(u64::MAX);
        let _ = self.held.try_update(Ordering::Relaxed, Ordering::Relaxed, |held| Some(held.saturating_sub(bytes)));
    }

    /// Makes room for `incoming` bytes: while the budget would be over, the
    /// reader furthest behind (its oldest frame waiting the longest) that
    /// holds anything drops what it holds and resumes at its next keyframe.
    /// Called with no queue's lock held.
    pub(crate) fn make_room(&self, incoming: usize) {
        let incoming = u64::try_from(incoming).unwrap_or(u64::MAX);
        let mut skipped = 0u64;
        while self.held.load(Ordering::Relaxed).saturating_add(incoming) > self.total.load(Ordering::Relaxed) {
            let victim = {
                let queues = self.queues.lock().unwrap_or_else(PoisonError::into_inner);
                queues
                    .iter()
                    .filter_map(Weak::upgrade)
                    .filter_map(|q| q.waiting_since().map(|since| (since, q)))
                    .min_by_key(|(since, _)| *since)
                    .map(|(_, q)| q)
            };
            let Some(victim) = victim else { break };
            if !victim.skip_all() {
                break;
            }
            skipped += 1;
            self.skips.fetch_add(1, Ordering::Relaxed);
        }
        if skipped > 0
            && let Some(skips) = self.reports.refused()
        {
            let snapshot = self.snapshot();
            tracing::info!(
                skips,
                budget = snapshot.budget,
                held = snapshot.held,
                readers = snapshot.readers,
                "output memory budget full (api.output_memory): the reader furthest behind skips to its next keyframe"
            );
        }
    }
}

/// The budget's size for `size`, a reading of the host (none: the smallest
/// budget, or the size given) and what the budget holds now.
pub fn size_from(size: BudgetSize, reading: Option<HostReading>, held: u64) -> u64 {
    let Some(reading) = reading else {
        return match size {
            BudgetSize::Fraction(_) => MIN_BUDGET,
            BudgetSize::Bytes(bytes) => bytes,
        };
    };
    let takeable = reading.available.saturating_add(held).min(reading.limit);
    match size {
        BudgetSize::Fraction(fraction) => {
            let fraction = if fraction.is_finite() { fraction.clamp(0.0, 1.0) } else { DEFAULT_FRACTION };
            let share = |bytes: u64| (bytes as f64 * fraction) as u64;
            share(takeable).min(share(reading.limit)).max(MIN_BUDGET)
        }
        BudgetSize::Bytes(bytes) => bytes.min(takeable).max(MIN_BUDGET.min(bytes)),
    }
}

/// The source of a budget's numbers, for the log.
pub fn source_name(reading: Option<HostReading>) -> &'static str {
    reading.map_or("unknown", |r| MemorySource::as_str(r.source))
}

#[cfg(test)]
#[path = "memory_tests.rs"]
mod tests;
