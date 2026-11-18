//! How long a source waits before its next attempt.
//!
//! Four cadences, chosen by why the last run ended:
//!
//! - refused credentials: the slow authentication backoff, always (a camera
//!   that counts failed logins locks the account);
//! - declined (a 4XX) by a source that has delivered nothing since its demand
//!   began: the slow declined backoff (a sound transcode of a camera with no
//!   microphone);
//! - any other failure of a source that dropped after it delivered, while
//!   each attempt costs nothing but a refusal: a short, constant cadence
//!   ([`Limits::quick_retry`]) for [`Limits::quick_retry_for`] after the
//!   drop, so that a camera back from an outage is read within half a second
//!   of its return (the bench's R01 and R02, D34);
//! - everything else: the exponential backoff, up to [`Limits::backoff_max`].

use std::time::Duration;

use tokio::time::Instant;

use crate::backoff::Backoff;
use crate::driver::SourceError;
use crate::limits::Limits;

/// The backoffs of one supervision (one demand on a source).
pub(crate) struct Retry {
    backoff: Backoff,
    auth: Backoff,
    declined: Backoff,
    quick: Backoff,
    quick_for: Duration,
    cheap_attempt: Duration,
    /// Whether the driver's attempts are cheap ([`crate::SourceDriver::cheap_attempts`]).
    cheap: bool,
    /// When a run that delivered last ended: the short cadence counts from it.
    dropped: Option<Instant>,
}

/// How a run ended, as far as the next delay is concerned.
#[derive(Debug, Clone, Copy)]
pub(crate) struct RunEnd {
    /// The run delivered a frame.
    pub(crate) delivered: bool,
    /// A run delivered since the demand began (this one or an earlier one).
    pub(crate) ever_delivered: bool,
    /// How long the run lasted.
    pub(crate) ran: Duration,
}

impl Retry {
    pub(crate) fn new(limits: &Limits, cheap: bool) -> Self {
        // A constant cadence: its jitter alone, so a hundred cameras that
        // dropped together on a switch's reboot do not all ask on one tick.
        let quick = Backoff::new(limits.quick_retry, limits.quick_retry);
        Self {
            backoff: Backoff::new(limits.backoff_initial, limits.backoff_max),
            auth: Backoff::new(limits.auth_backoff_initial, limits.auth_backoff_max),
            declined: Backoff::new(limits.declined_backoff_initial, limits.declined_backoff_max),
            quick,
            quick_for: limits.quick_retry_for,
            cheap_attempt: limits.cheap_attempt,
            cheap,
            dropped: None,
        }
    }

    /// A connection that held: every backoff starts over.
    pub(crate) fn reset(&mut self) {
        self.backoff.reset();
        self.auth.reset();
        self.declined.reset();
    }

    /// The delay before the attempt that follows a run ending with `error`.
    pub(crate) fn delay(&mut self, error: &SourceError, end: RunEnd) -> Duration {
        if end.delivered {
            self.dropped = Some(Instant::now());
        }
        match error {
            SourceError::Auth => self.auth.next_delay(),
            SourceError::Declined(_) if !end.ever_delivered => self.declined.next_delay(),
            SourceError::Declined(_) => self.backoff.next_delay(),
            _ if self.quick(end) => self.quick.next_delay(),
            _ => self.backoff.next_delay(),
        }
    }

    /// Whether the short cadence applies: a cheap driver, a source that
    /// dropped less than `quick_retry_for` ago, and a run that delivered or
    /// was refused at once (a connection timed out is not asked again so often).
    fn quick(&self, end: RunEnd) -> bool {
        self.cheap
            && self.dropped.is_some_and(|at| at.elapsed() < self.quick_for)
            && (end.delivered || end.ran < self.cheap_attempt)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const REFUSED: SourceError = SourceError::Ended;

    fn end(delivered: bool, ran_ms: u64) -> RunEnd {
        RunEnd { delivered, ever_delivered: true, ran: Duration::from_millis(ran_ms) }
    }

    fn retry(cheap: bool) -> Retry {
        Retry::new(&Limits::default(), cheap)
    }

    #[tokio::test(start_paused = true)]
    async fn a_source_that_dropped_is_asked_again_twice_a_second_for_two_minutes() {
        let mut r = retry(true);
        let first =
            r.delay(&SourceError::Silent { silent_ms: 5000, allowed_ms: 5000, longest_gap_ms: 80 }, end(true, 60_000));
        assert!(first <= Duration::from_millis(500), "{first:?}");
        let mut waited = first;
        while waited < Duration::from_secs(119) {
            let delay = r.delay(&REFUSED, end(false, 3));
            assert!((Duration::from_millis(375)..=Duration::from_millis(500)).contains(&delay), "{delay:?}");
            tokio::time::sleep(delay).await;
            waited += delay;
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
        // Past the two minutes: the backoff, from its first step.
        let after: Vec<Duration> = (0..6).map(|_| r.delay(&REFUSED, end(false, 3))).collect();
        assert!(after[5] > Duration::from_secs(3), "{after:?}");
    }

    #[tokio::test(start_paused = true)]
    async fn an_attempt_that_took_its_time_keeps_the_backoff() {
        let mut r = retry(true);
        r.delay(&SourceError::Stalled, end(true, 60_000));
        let delays: Vec<Duration> =
            (0..6).map(|_| r.delay(&SourceError::Unreachable("timeout".into()), end(false, 2_500))).collect();
        assert!(delays[5] > Duration::from_secs(3), "{delays:?}");
    }

    #[tokio::test(start_paused = true)]
    async fn credentials_and_declines_keep_their_slow_backoffs() {
        let mut r = retry(true);
        r.delay(&SourceError::Stalled, end(true, 60_000));
        assert!(r.delay(&SourceError::Auth, end(false, 3)) >= Duration::from_secs(22));
        let mut never = retry(true);
        let delay =
            never.delay(&SourceError::Declined("455".into()), RunEnd { ever_delivered: false, ..end(false, 3) });
        assert!(delay >= Duration::from_millis(3750), "{delay:?}");
    }

    #[tokio::test(start_paused = true)]
    async fn a_process_or_a_source_that_never_delivered_keeps_the_backoff() {
        let mut process = retry(false);
        process.delay(&SourceError::Stalled, end(true, 60_000));
        let mut never = retry(true);
        for r in [&mut process, &mut never] {
            let delays: Vec<Duration> = (0..6).map(|_| r.delay(&REFUSED, end(false, 3))).collect();
            assert!(delays[5] > Duration::from_secs(3), "{delays:?}");
        }
    }
}
