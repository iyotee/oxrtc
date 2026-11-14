//! Exponential backoff with jitter, for reconnecting sources.
//!
//! Jitter matters more than the curve: a hundred cameras that drop together on
//! a switch reboot must not all reconnect on the same tick.

use std::time::Duration;

/// Floor of the first step: a zero step would never grow.
const MIN_STEP: Duration = Duration::from_millis(1);

/// Exponential backoff: each step doubles up to a ceiling, and each delay is
/// the step multiplied by a random factor in `[0.75, 1.25)`, then capped.
#[derive(Debug, Clone)]
pub struct Backoff {
    initial: Duration,
    max: Duration,
    step: Duration,
    rng: fastrand::Rng,
}

impl Backoff {
    /// A backoff starting at `initial` and capped at `max`.
    pub fn new(initial: Duration, max: Duration) -> Self {
        Self::with_rng(initial, max, fastrand::Rng::new())
    }

    /// The same, with a deterministic jitter source (tests).
    pub fn with_seed(initial: Duration, max: Duration, seed: u64) -> Self {
        Self::with_rng(initial, max, fastrand::Rng::with_seed(seed))
    }

    fn with_rng(initial: Duration, max: Duration, rng: fastrand::Rng) -> Self {
        let max = max.max(MIN_STEP);
        let initial = initial.clamp(MIN_STEP, max);
        Self { initial, max, step: initial, rng }
    }

    /// The delay to wait before the next attempt.
    pub fn next_delay(&mut self) -> Duration {
        let base = self.step;
        self.step = self.step.saturating_mul(2).min(self.max);
        let factor = 0.75 + self.rng.f64() * 0.5;
        base.mul_f64(factor).min(self.max)
    }

    /// Back to the first step, after a connection that held.
    pub fn reset(&mut self) {
        self.step = self.initial;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grows_then_caps_with_bounded_jitter() {
        let mut b = Backoff::with_seed(Duration::from_millis(100), Duration::from_secs(2), 7);
        let mut step = 100.0_f64;
        for _ in 0..12 {
            let d = b.next_delay().as_secs_f64() * 1000.0;
            assert!(d >= step * 0.75 - 0.001, "{d} below {step}");
            assert!(d <= (step * 1.25).min(2000.0) + 0.001, "{d} above {step}");
            step = (step * 2.0).min(2000.0);
        }
    }

    #[test]
    fn reset_returns_to_the_first_step() {
        let mut b = Backoff::with_seed(Duration::from_millis(100), Duration::from_secs(30), 1);
        for _ in 0..6 {
            b.next_delay();
        }
        b.reset();
        assert!(b.next_delay() <= Duration::from_millis(125));
    }

    /// The bench's R01: with a 30 s ceiling, a camera back from a 60 s outage
    /// waited 18 s for its next attempt. With the defaults no attempt is ever
    /// more than 5 s after the previous one.
    #[test]
    fn with_the_default_limits_a_camera_back_is_retried_within_five_seconds() {
        let limits = crate::Limits::default();
        let mut b = Backoff::with_seed(limits.backoff_initial, limits.backoff_max, 11);
        let worst = (0..50).map(|_| b.next_delay()).max().unwrap_or_default();
        assert!(worst <= Duration::from_secs(5), "{worst:?}");
    }

    #[test]
    fn zero_and_inverted_bounds_do_not_stall() {
        let mut b = Backoff::with_seed(Duration::ZERO, Duration::ZERO, 3);
        assert!(b.next_delay() > Duration::ZERO);
        let mut b = Backoff::with_seed(Duration::from_secs(10), Duration::from_secs(1), 3);
        assert!(b.next_delay() <= Duration::from_secs(1));
    }
}
