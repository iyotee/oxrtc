//! A playout-delay floor for a viewer that keeps asking for keyframes (W3,
//! D35).
//!
//! A browser holds each picture about as long as the network asks; when its
//! decoder falls behind (many viewers in one browser, a weak machine) a late
//! frame is dropped, the chain of pictures is broken until the next
//! keyframe, the browser asks for one and its decoder idles meanwhile. On
//! the bench's B05 (100 viewers in one browser) oxrtc's even sending left the
//! browser an 8 ms hold and it decoded 5 frames a second per viewer; the
//! references, slower to send, made its jitter estimate - its hold - 229 and
//! 628 ms deep, and it decoded 7 and 10. A hold of 600 ms asked through the
//! `playout-delay` header extension gave oxrtc's viewers 213 ms of hold and
//! 7.5 frames a second.
//!
//! So a session whose viewer asks for keyframes again and again is asked to
//! hold its picture longer: [`STEP`] more at each request that follows
//! another within [`REPEAT_WINDOW`] (at most one step per [`RAISE_EVERY`]),
//! up to [`MAX`]; once the viewer has not asked for [`QUIET`], one step less
//! every [`LOWER_EVERY`]. A viewer that never asks, or asks once (a lost
//! packet), is never delayed. Only with lip sync on: `webrtc.lip_sync: false`
//! sends 0 to 0 on every picture, the owner's choice of latency.

use std::time::{Duration, Instant};

/// What one step adds to the floor.
pub(crate) const STEP: Duration = Duration::from_millis(100);
/// The highest floor asked (MediaMTX's viewers were held 628 ms in B05).
pub(crate) const MAX: Duration = Duration::from_millis(600);
/// Two requests this close make a viewer that keeps asking.
pub(crate) const REPEAT_WINDOW: Duration = Duration::from_secs(5);
/// At most one step up this often.
pub(crate) const RAISE_EVERY: Duration = Duration::from_secs(1);
/// A viewer quiet this long is lowered...
pub(crate) const QUIET: Duration = Duration::from_secs(10);
/// ... one step this often.
pub(crate) const LOWER_EVERY: Duration = Duration::from_secs(5);
/// The ceiling sent with the floor: the browser's own default (10 s).
pub(crate) const CEILING: Duration = Duration::from_secs(10);

/// One session's floor.
#[derive(Debug, Default)]
pub(crate) struct Floor {
    floor: Duration,
    /// Whether a floor was ever asked: from then on every picture carries
    /// the current one, so that a lowered floor reaches the browser (it keeps
    /// the last value it was sent).
    asked: bool,
    last_request: Option<Instant>,
    last_change: Option<Instant>,
    highest: Duration,
}

impl Floor {
    /// A keyframe request at `now`.
    pub(crate) fn request(&mut self, now: Instant) {
        let repeated = self.last_request.is_some_and(|at| now.saturating_duration_since(at) < REPEAT_WINDOW);
        self.last_request = Some(now);
        let due = self.last_change.is_none_or(|at| now.saturating_duration_since(at) >= RAISE_EVERY);
        if repeated && due && self.floor < MAX {
            self.floor = self.floor.saturating_add(STEP).min(MAX);
            self.highest = self.highest.max(self.floor);
            self.asked = true;
            self.last_change = Some(now);
        }
    }

    /// The floor to send with a picture written at `now`, lowered first if
    /// the viewer has been quiet; `None` while none was ever asked.
    pub(crate) fn current(&mut self, now: Instant) -> Option<Duration> {
        if !self.asked {
            return None;
        }
        let quiet = self.last_request.is_none_or(|at| now.saturating_duration_since(at) >= QUIET);
        let due = self.last_change.is_none_or(|at| now.saturating_duration_since(at) >= LOWER_EVERY);
        if quiet && due && !self.floor.is_zero() {
            self.floor = self.floor.saturating_sub(STEP);
            self.last_change = Some(now);
        }
        Some(self.floor)
    }

    /// The highest floor this session asked (for its last log line).
    pub(crate) fn highest(&self) -> Duration {
        self.highest
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    const S: Duration = Duration::from_secs(1);

    #[test]
    fn a_viewer_that_never_asks_or_asks_once_is_not_delayed() {
        let now = Instant::now();
        let mut floor = Floor::default();
        assert_eq!(floor.current(now), None);
        floor.request(now);
        assert_eq!(floor.current(now + S), None, "one request: a lost packet");
        floor.request(now + REPEAT_WINDOW + S);
        assert_eq!(floor.current(now + REPEAT_WINDOW + S), None, "the next one too far from it");
    }

    #[test]
    fn a_viewer_that_keeps_asking_is_held_longer_up_to_the_ceiling() {
        let start = Instant::now();
        let mut floor = Floor::default();
        // Twice a second, as B05's viewers ask: one step a second at most.
        for half in 0..40u32 {
            floor.request(start + S * half / 2);
        }
        assert_eq!(floor.current(start + S * 20), Some(MAX));
        let mut steps = Floor::default();
        steps.request(start);
        steps.request(start + S / 2);
        assert_eq!(steps.current(start + S / 2), Some(STEP));
        steps.request(start + S * 3 / 4);
        assert_eq!(steps.current(start + S * 3 / 4), Some(STEP), "within a second of the last step");
        steps.request(start + S * 3 / 2);
        assert_eq!(steps.current(start + S * 3 / 2), Some(STEP * 2));
    }

    #[test]
    fn a_viewer_that_stops_asking_comes_back_down_to_zero() {
        let start = Instant::now();
        let mut floor = Floor::default();
        for n in 0..10u32 {
            floor.request(start + S * n);
        }
        let top = floor.current(start + S * 9).unwrap();
        assert_eq!(top, MAX, "nine steps a second apart, held at the ceiling");
        let last = start + S * 9;
        assert_eq!(floor.current(last + QUIET - S), Some(top), "not quiet yet");
        assert_eq!(floor.current(last + QUIET), Some(top - STEP));
        assert_eq!(floor.current(last + QUIET + S), Some(top - STEP), "one step per interval");
        let mut at = last + QUIET;
        for _ in 0..20 {
            at += LOWER_EVERY;
            floor.current(at);
        }
        assert_eq!(floor.current(at), Some(Duration::ZERO), "down to zero, and still sent so the browser resets");
        assert_eq!(floor.highest(), top, "the highest is kept for the session's last line");
    }
}
