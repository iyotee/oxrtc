//! Throttled reports of refusals, shared by every limit a listener enforces.
//!
//! A limit that refuses a connection (or a session) has a reason to say so:
//! the operator needs to know it is reached. It has no reason to say so once
//! per refusal: a connect flood over the bound would flood the log as well.
//! [`Refusals`] counts the refusals and lets one warning through per
//! [`REPORT_EVERY`], with how many refusals it stands for.

use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

/// Shortest time between two reports of refused connections: a flood over
/// the bound gets one warning per interval, not one per connection, so it
/// cannot flood the log as well.
pub const REPORT_EVERY: Duration = Duration::from_secs(1);

/// Counts refusals and says when to report them. Cloning shares the count.
#[derive(Clone)]
pub struct Refusals {
    inner: Arc<Mutex<State>>,
    every: Duration,
}

impl Default for Refusals {
    fn default() -> Self {
        Self::every(REPORT_EVERY)
    }
}

/// Refusals not reported yet, and when the last report was.
#[derive(Default)]
struct State {
    unreported: u64,
    last_report: Option<Instant>,
}

impl std::fmt::Debug for Refusals {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let state = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        f.debug_struct("Refusals").field("unreported", &state.unreported).finish()
    }
}

impl Refusals {
    /// No refusal counted yet.
    pub fn new() -> Self {
        Self::default()
    }

    /// The same, reporting at most once per `interval` (anything else a
    /// limit drops and must say so without a line per drop: a consumer
    /// that falls behind its queue's budget, [`crate::queue`]).
    pub fn every(interval: Duration) -> Self {
        Self { inner: Arc::default(), every: interval }
    }

    /// Counts one refusal and says whether to warn now: `Some(n)`, the
    /// refusals since the last warning (this one included), at most once per
    /// [`REPORT_EVERY`]; `None` otherwise.
    pub fn refused(&self) -> Option<u64> {
        self.refused_at(Instant::now())
    }

    /// [`refused`](Self::refused) at a given time.
    pub fn refused_at(&self, now: Instant) -> Option<u64> {
        let mut state = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        state.unreported = state.unreported.saturating_add(1);
        let due = state.last_report.is_none_or(|last| now.saturating_duration_since(last) >= self.every);
        if !due {
            return None;
        }
        state.last_report = Some(now);
        Some(std::mem::take(&mut state.unreported))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refusals_are_reported_once_per_interval_with_their_count() {
        // A connect flood over the bound must not write one warning per
        // connection: the first refusal is reported at once, the next ones
        // once per interval, with how many there were.
        let refusals = Refusals::new();
        let start = Instant::now();
        assert_eq!(refusals.refused_at(start), Some(1));
        for _ in 0..999 {
            assert_eq!(refusals.refused_at(start + Duration::from_millis(10)), None);
        }
        assert_eq!(refusals.refused_at(start + REPORT_EVERY), Some(1000));
        assert_eq!(refusals.refused_at(start + REPORT_EVERY), None);
        assert_eq!(refusals.refused_at(start + REPORT_EVERY * 2), Some(2));
        // Clones share the report.
        assert_eq!(refusals.clone().refused_at(start + REPORT_EVERY * 2), None);
        assert_eq!(format!("{refusals:?}"), "Refusals { unreported: 1 }");
    }

    #[test]
    fn a_clock_going_back_reports_nothing_twice() {
        let refusals = Refusals::new();
        let start = Instant::now() + REPORT_EVERY;
        assert_eq!(refusals.refused_at(start), Some(1));
        assert_eq!(refusals.refused_at(start - REPORT_EVERY), None, "an earlier instant is not a new interval");
    }
}
