//! A viewer's keyframe request (PLI or FIR), answered from the cached group
//! of pictures (R24).
//!
//! A browser that lost a packet of the picture cannot decode until the next
//! keyframe, and asks for one. oxrtc has no upstream to ask (a camera read
//! through retina takes no RTCP feedback, a transcode none either), and the
//! camera's next keyframe can be 6 s away. But the core keeps the current
//! group of pictures for new viewers: sent again to this session, from its
//! keyframe to the latest frame, it brings the decoder back to the present
//! at once, with the frames that follow it.
//!
//! Its frames were sent once already, with their own RTP times; a browser
//! drops a frame older than one it decoded. So they are sent again with
//! times of their own, spread between the last time this session sent and
//! the next frame to come, and the queued frames they repeat are skipped
//! ([`crate::payload::TrackOut::repeats`]). The audio is not replayed.
//!
//! A request costs a group of pictures (up to `Limits::gop_cache_bytes`), so
//! a session gets at most one replay per [`REPLAY_INTERVAL`]; requests in
//! between are answered together at its end. Browsers repeat their request
//! while they wait, so none is lost.

use std::time::{Duration, Instant};

/// Shortest time between two replays to one session.
pub(crate) const REPLAY_INTERVAL: Duration = Duration::from_secs(1);
/// Longest group of pictures replayed, in frames: 40 s at 25 frames a
/// second, beyond any camera's keyframe interval (and the cache's bytes).
pub(crate) const MAX_REPLAY_FRAMES: usize = 1000;

/// One session's keyframe requests and when the next may be answered.
#[derive(Debug, Default)]
pub(crate) struct Replays {
    wanted: bool,
    last: Option<Instant>,
    /// Requests received.
    pub(crate) requested: u64,
    /// Replays sent.
    pub(crate) sent: u64,
}

impl Replays {
    /// A keyframe request from the viewer.
    pub(crate) fn request(&mut self) {
        self.wanted = true;
        self.requested = self.requested.saturating_add(1);
    }

    /// Whether to replay now; when it says so, the replay is counted as
    /// sent and the next one waits [`REPLAY_INTERVAL`].
    pub(crate) fn take(&mut self, now: Instant) -> bool {
        if !self.wanted || self.wake_at().is_some_and(|at| now < at) {
            return false;
        }
        self.wanted = false;
        self.last = Some(now);
        self.sent = self.sent.saturating_add(1);
        true
    }

    /// When a request waiting for its interval is due.
    pub(crate) fn wake_at(&self) -> Option<Instant> {
        if !self.wanted {
            return None;
        }
        self.last.and_then(|last| last.checked_add(REPLAY_INTERVAL))
    }
}

/// The RTP times of `count` replayed frames, for a track that last sent
/// `last_sent` and whose replay ends with the frame of time `replay_end`
/// (its own time, already sent or still queued): evenly spread after
/// `last_sent` and before `replay_end + interval`, where the next new frame
/// is expected (`interval`, the track's last step). `None` when they do not
/// fit, one tick each at least.
pub(crate) fn stamps(last_sent: u64, replay_end: u64, interval: u64, count: usize) -> Option<Vec<u64>> {
    if count == 0 || count > MAX_REPLAY_FRAMES {
        return None;
    }
    let room = replay_end.saturating_sub(last_sent).saturating_add(interval.max(1));
    let parts = u64::try_from(count).ok()?.checked_add(1)?;
    let step = room / parts;
    if step == 0 {
        return None;
    }
    (1..parts).map(|i| step.checked_mul(i).and_then(|offset| last_sent.checked_add(offset))).collect()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::indexing_slicing)]

    use super::*;

    #[test]
    fn a_replay_waits_for_its_interval_and_requests_meanwhile_make_one() {
        let start = Instant::now();
        let mut replays = Replays::default();
        assert!(!replays.take(start), "nothing asked");
        replays.request();
        assert!(replays.take(start));
        assert!(!replays.take(start), "answered");
        // Three requests inside the interval: one replay, at its end.
        for _ in 0..3 {
            replays.request();
        }
        assert_eq!(replays.wake_at(), Some(start + REPLAY_INTERVAL));
        assert!(!replays.take(start + REPLAY_INTERVAL / 2));
        assert!(replays.take(start + REPLAY_INTERVAL));
        assert_eq!((replays.requested, replays.sent), (4, 2));
        assert_eq!(replays.wake_at(), None);
    }

    #[test]
    fn replayed_times_fall_between_the_last_sent_and_the_next_frame() {
        // 25 frames a second at 90 kHz: 3600 ticks apart. The session sent
        // up to 360_000; the cache holds 50 frames up to 363_600.
        let times = stamps(360_000, 363_600, 3600, 50).unwrap();
        assert_eq!(times.len(), 50);
        assert!(times[0] > 360_000);
        assert!(times.windows(2).all(|w| w[0] < w[1]), "in order, never the same");
        assert!(*times.last().unwrap() < 363_600 + 3600, "before the next new frame");
    }

    #[test]
    fn a_replay_that_cannot_fit_is_not_stamped() {
        assert_eq!(stamps(1000, 1000, 10, 10), None, "ten frames in ten ticks");
        assert!(stamps(1000, 1000, 11, 10).is_some());
        assert_eq!(stamps(0, 0, 3600, 0), None);
        assert_eq!(stamps(0, u64::MAX / 2, 3600, MAX_REPLAY_FRAMES + 1), None);
        assert_eq!(stamps(u64::MAX - 10, u64::MAX, 3600, 5), None, "no overflow");
    }
}
