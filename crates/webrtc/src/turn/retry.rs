//! When a failed TURN allocation is tried again: the core's jittered
//! backoff (`oxrtc_core::backoff`), as for a camera. One relay serves every
//! installation paired with it; without jitter, all of them would come back
//! on the same second after its outage.

use std::time::Duration;

use oxrtc_core::backoff::Backoff;

use super::allocation::Failure;

/// Retries of a failed allocation: about 1 s doubling to a minute...
const BACKOFF_INITIAL: Duration = Duration::from_secs(1);
const BACKOFF_MAX: Duration = Duration::from_secs(60);
/// ...and credentials refused, which only a new configuration changes:
/// every 4 to 5 minutes.
const REFUSED_RETRY: Duration = Duration::from_secs(300);
/// An allocation that lived this long resets the backoff.
const HEALTHY_AFTER: Duration = Duration::from_secs(60);

pub(crate) struct Retry {
    failed: Backoff,
    refused: Backoff,
}

impl Retry {
    pub(crate) fn new() -> Self {
        Self { failed: Backoff::new(BACKOFF_INITIAL, BACKOFF_MAX), refused: Backoff::new(REFUSED_RETRY, REFUSED_RETRY) }
    }

    #[cfg(test)]
    fn with_seed(seed: u64) -> Self {
        Self {
            failed: Backoff::with_seed(BACKOFF_INITIAL, BACKOFF_MAX, seed),
            refused: Backoff::with_seed(REFUSED_RETRY, REFUSED_RETRY, seed),
        }
    }

    /// The wait before the next attempt, after an allocation that ended
    /// with `failure` having lived `lived`.
    pub(crate) fn after(&mut self, failure: &Failure, lived: Duration) -> Duration {
        if lived >= HEALTHY_AFTER {
            self.failed.reset();
        }
        match failure {
            Failure::Refused => self.refused.next_delay(),
            _ => self.failed.next_delay(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FAILED: Failure = Failure::NoAnswer;

    #[test]
    fn installations_that_lost_the_same_relay_do_not_come_back_together() {
        // One relay serves every installation: after its outage, the same
        // curve without jitter brings them all back on the same second.
        let waits: Vec<Duration> = (0..8).map(|seed| Retry::with_seed(seed).after(&FAILED, Duration::ZERO)).collect();
        let distinct = waits.iter().filter(|w| **w != waits[0]).count();
        assert!(distinct > 0, "every installation waits {:?}", waits[0]);
        let refused: Vec<Duration> =
            (0..8).map(|seed| Retry::with_seed(seed).after(&Failure::Refused, Duration::ZERO)).collect();
        assert!(refused.iter().any(|w| *w != refused[0]), "refused credentials too: {:?}", refused[0]);
    }

    #[test]
    fn the_wait_doubles_to_a_minute_and_resets_after_a_healthy_allocation() {
        let mut retry = Retry::with_seed(3);
        let mut step = BACKOFF_INITIAL.as_secs_f64();
        for _ in 0..10 {
            let wait = retry.after(&FAILED, Duration::ZERO).as_secs_f64();
            assert!(wait >= step * 0.75 - 1e-3 && wait <= (step * 1.25).min(60.0) + 1e-3, "{wait} for {step}");
            step = (step * 2.0).min(BACKOFF_MAX.as_secs_f64());
        }
        assert!(retry.after(&FAILED, HEALTHY_AFTER) <= Duration::from_millis(1250));
        let refused = retry.after(&Failure::Refused, Duration::ZERO);
        assert!(refused >= REFUSED_RETRY.mul_f64(0.75) && refused <= REFUSED_RETRY, "{refused:?}");
    }
}
