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
//! ([`crate::payload::TrackOut::repeats`]). A new frame that comes earlier
//! than expected (a camera's steps vary) is written just after the replay's
//! last time, never before it ([`crate::payload::TrackOut::time_of`]). The
//! audio is not replayed.
//!
//! The same replay answers a cut of the session's own queue (a session that
//! connected later than its live budget, a task held up): the subscription
//! would otherwise wait for the camera's next keyframe, which a cut on a
//! cold start leaves nothing before.
//!
//! A replay is not always the answer (W3, D35). A browser that cannot
//! decode as fast as the camera sends (many viewers in one browser, a weak
//! machine) drops frames and asks for a keyframe with nothing lost on the
//! wire: a group written to it in one frame interval is a burst it cannot
//! decode either, so it drops it, asks again and is replayed again (B06, 50
//! viewers of one camera in one browser: 1.5 times the camera's frames
//! received, twice the freezes of go2rtc, which answers nothing). So a
//! request is answered by the camera ([`Answer::Camera`]) - the picture
//! stops until the camera's next keyframe, since nothing sent before it can
//! be decoded - when that keyframe is due within [`CAMERA_KEYFRAME_SOON`]
//! ([`KeyClock`], from the steps between the keyframes the session saw), or
//! when the viewer asks again within [`BEHIND_AFTER_REPLAY`] of a replay
//! (the replay did not bring it back: it is behind). Otherwise, a group that
//! is long or not known yet, the cached group is replayed, as before.
//!
//! A request costs a group of pictures (up to `Limits::gop_cache_bytes`), so
//! a session gets at most one replay per [`REPLAY_INTERVAL`]; requests in
//! between are answered together at its end. Browsers repeat their request
//! while they wait, so none is lost.

use std::time::{Duration, Instant};

/// Where a video track is in its group of pictures, from the frames a
/// session saw: the step between its last two keyframes is taken as the next
/// one's, which is how far a camera's keyframe interval is known (they are
/// regular; one that is not is replayed when its group runs long).
#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct KeyClock {
    last_key: Option<i64>,
    gop: Option<i64>,
    latest: Option<i64>,
}

impl KeyClock {
    /// A frame of the track, of time `pts` (the hub's timeline).
    pub(crate) fn saw(&mut self, pts: i64, keyframe: bool) {
        if keyframe {
            match self.last_key {
                Some(key) if pts <= key => {}
                Some(key) => {
                    self.gop = Some(pts - key);
                    self.last_key = Some(pts);
                }
                None => self.last_key = Some(pts),
            }
        }
        self.latest = Some(self.latest.map_or(pts, |l| l.max(pts)));
    }

    /// Ticks until the next keyframe is expected (0 when it is a little
    /// late); `None` until two keyframes were seen, and again once the
    /// expected one is later than [`KEYFRAME_LATE`] of its interval: the
    /// clock was wrong (a group longer than the last, a keyframe a source
    /// sent early when it restarted or at a change of scene) and the next
    /// keyframe may be a whole group away, so a request is replayed as R24
    /// would, not left to a keyframe that is not coming.
    pub(crate) fn until_next(&self) -> Option<u64> {
        let (key, gop, latest) = (self.last_key?, self.gop?, self.latest?);
        let elapsed = latest.saturating_sub(key);
        if elapsed > gop.saturating_add(gop / KEYFRAME_LATE) {
            return None;
        }
        u64::try_from(gop.saturating_sub(elapsed).max(0)).ok()
    }
}

/// A keyframe expected by [`KeyClock`] is still taken as due while it is
/// late by less than a quarter of its interval (a camera's groups vary: the
/// owner's from 4.8 to 6 s); later than that, the interval is unknown again.
const KEYFRAME_LATE: i64 = 4;

/// Shortest time between two replays to one session.
pub(crate) const REPLAY_INTERVAL: Duration = Duration::from_secs(1);
/// Longest group of pictures replayed, in frames: 40 s at 25 frames a
/// second, beyond any camera's keyframe interval (and the cache's bytes).
pub(crate) const MAX_REPLAY_FRAMES: usize = 1000;

/// A request is left to the camera when its next keyframe is due this soon:
/// a viewer waits no longer than a replay would have cost it in a burst (the
/// bench's camera sends one a second, so its viewers are never replayed).
pub(crate) const CAMERA_KEYFRAME_SOON: Duration = Duration::from_secs(1);
/// A viewer that asks again this soon after a replay was not brought back by
/// it: it is behind, and another group would be another burst.
pub(crate) const BEHIND_AFTER_REPLAY: Duration = Duration::from_secs(2);

/// How a keyframe request is answered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Answer {
    /// The cached group of pictures, sent again.
    Replay,
    /// Nothing until the camera's next keyframe: the picture stops there.
    Camera,
}

/// One session's keyframe requests and when the next may be answered.
#[derive(Debug, Default)]
pub(crate) struct Replays {
    wanted: bool,
    last: Option<Instant>,
    /// Requests received.
    pub(crate) requested: u64,
    /// Requests answered by the camera's next keyframe ([`Answer::Camera`]).
    pub(crate) declined: u64,
    /// Frames written by replays.
    pub(crate) frames: u64,
    /// Replays sent.
    pub(crate) sent: u64,
}

impl Replays {
    /// A keyframe request from the viewer.
    pub(crate) fn request(&mut self) {
        self.wanted = true;
        self.requested = self.requested.saturating_add(1);
    }

    /// How to answer a keyframe request that comes `now`, the camera's next
    /// keyframe being expected in `next_keyframe` (`None`: not known yet).
    pub(crate) fn answer(&self, now: Instant, next_keyframe: Option<Duration>) -> Answer {
        if next_keyframe.is_some_and(|left| left <= CAMERA_KEYFRAME_SOON) {
            return Answer::Camera;
        }
        if self.last.is_some_and(|last| now.saturating_duration_since(last) < BEHIND_AFTER_REPLAY) {
            return Answer::Camera;
        }
        Answer::Replay
    }

    /// A keyframe request left to the camera's next keyframe.
    pub(crate) fn decline(&mut self) {
        self.requested = self.requested.saturating_add(1);
        self.declined = self.declined.saturating_add(1);
    }

    /// Video was cut on its way to this session (its queue overflowed: a
    /// session connecting later than its budget, a task held up): a replay
    /// brings it back at once, where the subscription would wait for the
    /// camera's next keyframe. Not counted as the viewer's request.
    pub(crate) fn resume(&mut self) {
        self.wanted = true;
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
    fn a_keyframe_due_soon_is_left_to_the_camera_and_a_far_one_is_replayed() {
        let now = Instant::now();
        let replays = Replays::default();
        assert_eq!(replays.answer(now, Some(Duration::from_millis(400))), Answer::Camera);
        assert_eq!(replays.answer(now, Some(CAMERA_KEYFRAME_SOON)), Answer::Camera, "within the bound");
        assert_eq!(replays.answer(now, Some(Duration::from_secs(5))), Answer::Replay, "a 6 s group");
        assert_eq!(replays.answer(now, None), Answer::Replay, "an interval not known yet: as R24");
    }

    #[test]
    fn a_viewer_that_asks_again_soon_after_a_replay_is_left_to_the_camera() {
        let start = Instant::now();
        let mut replays = Replays::default();
        let far = Some(Duration::from_secs(5));
        replays.request();
        assert!(replays.take(start));
        assert_eq!(replays.answer(start + Duration::from_millis(1500), far), Answer::Camera, "behind");
        assert_eq!(replays.answer(start + BEHIND_AFTER_REPLAY, far), Answer::Replay, "a later loss");
        replays.decline();
        assert_eq!((replays.requested, replays.declined, replays.sent), (2, 1, 1));
    }

    #[test]
    fn the_key_clock_counts_down_to_the_next_keyframe() {
        let mut keys = KeyClock::default();
        keys.saw(0, true);
        keys.saw(3600, false);
        assert_eq!(keys.until_next(), None, "one keyframe: no interval yet");
        // A keyframe every 90_000 ticks (1 s at 90 kHz).
        keys.saw(90_000, true);
        assert_eq!(keys.until_next(), Some(90_000));
        keys.saw(90_000 + 36_000, false);
        assert_eq!(keys.until_next(), Some(54_000));
        keys.saw(90_000 + 100_000, false);
        assert_eq!(keys.until_next(), Some(0), "a little late: due now");
        keys.saw(90_000 + 3600, false);
        assert_eq!(keys.until_next(), Some(0), "an earlier time does not move the latest back");
        keys.saw(210_000, true);
        assert_eq!(keys.until_next(), Some(120_000), "the last interval is the next one's");
        keys.saw(100_000, true);
        assert_eq!(keys.until_next(), Some(120_000), "a keyframe back in time changes nothing");
    }

    /// W3 review: the step between two keyframes is not always the next
    /// group's - a source that restarted mid-group sends its first keyframe
    /// early, an encoder adds one at a change of scene, a camera's groups
    /// vary. A keyframe late by more than a quarter of that step is not
    /// coming on that clock: the interval is unknown again (`None`), where
    /// it read "due now" for the rest of the group, and every request of it
    /// was left to a keyframe up to a whole group away.
    #[test]
    fn a_keyframe_that_does_not_come_on_its_clock_makes_the_interval_unknown() {
        let mut keys = KeyClock::default();
        // Keyframes 1 s apart (90 kHz), then a group of 6 s.
        keys.saw(0, true);
        keys.saw(90_000, true);
        keys.saw(90_000 + 90_000 + 22_500, false);
        assert_eq!(keys.until_next(), Some(0), "a quarter of the interval late: still due");
        keys.saw(90_000 + 90_000 + 22_501, false);
        assert_eq!(keys.until_next(), None, "later: not coming on that clock");
        keys.saw(90_000 + 300_000, false);
        assert_eq!(keys.until_next(), None);
        // The group's end: the clock is the 6 s step now.
        keys.saw(90_000 + 540_000, true);
        assert_eq!(keys.until_next(), Some(540_000));
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
