//! A source's own rhythm, and how long it may stay silent.
//!
//! A fixed stall timeout serves no camera well: 15 s is three hundred and
//! seventy-five pictures of a camera that sends 25 a second, and a camera back
//! from a 10 s outage was back before oxrtc knew it had left (the bench's R01,
//! 2026-11-17: 7.1 s from its return to a picture, go2rtc 5.4 s). A source
//! shows its rhythm with every frame, so its silence is judged against it: the
//! longest gap it has shown between two frames, [`SILENCE_FACTOR`] times over.
//!
//! Three bounds keep a slow or irregular camera from being cut:
//!
//! - the longest gap ever seen, never an average: a camera that once paused
//!   2 s (an encoder switching to its night mode) may pause 8 s from then on;
//! - a floor ([`crate::Limits::silence_floor`]): a camera whose rate drops
//!   from 25 pictures a second to one at night shows its first one-second gap
//!   only once it has dropped, so the silence a fast camera is allowed covers
//!   that already;
//! - nothing is judged before [`LEARN_GAPS`] gaps spanning [`LEARN_SPAN`]:
//!   until then, and never beyond it, the source's stall timeout holds.
//!
//! The gap between the last frame of one connection and the first of the next
//! is not one of the source's gaps ([`Cadence::restart`]).

use std::time::Duration;

use tokio::time::Instant;

/// How many times its longest gap a source may stay silent.
pub const SILENCE_FACTOR: u32 = 4;

/// Gaps seen before the cadence is trusted.
pub const LEARN_GAPS: u32 = 8;

/// Time those gaps must span before the cadence is trusted: a one-picture-a-
/// second webcam shows ten gaps, a 25 fps camera two hundred and fifty.
pub const LEARN_SPAN: Duration = Duration::from_secs(10);

/// What a source has shown of its rhythm, across its connections.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Cadence {
    /// The last frame of the current connection.
    last: Option<Instant>,
    /// Gaps seen, and the time they span (saturating).
    gaps: u32,
    span: Duration,
    /// The longest gap seen.
    longest: Duration,
}

impl Cadence {
    /// A frame arrived at `now`.
    pub(crate) fn frame(&mut self, now: Instant) {
        if let Some(last) = self.last.replace(now) {
            let gap = now.saturating_duration_since(last);
            self.gaps = self.gaps.saturating_add(1);
            self.span = self.span.saturating_add(gap);
            self.longest = self.longest.max(gap);
        }
    }

    /// A new connection: what it delivers first follows no frame of its own.
    pub(crate) fn restart(&mut self) {
        self.last = None;
    }

    /// The longest gap seen, once the cadence is trusted.
    pub(crate) fn longest_gap(&self) -> Option<Duration> {
        (self.gaps >= LEARN_GAPS && self.span >= LEARN_SPAN).then_some(self.longest)
    }

    /// How long the source may stay silent: [`SILENCE_FACTOR`] times its
    /// longest gap, at least `floor`, never more than `ceiling` (its stall
    /// timeout), which also holds until the cadence is trusted.
    pub(crate) fn silence_after(&self, floor: Duration, ceiling: Duration) -> Duration {
        match self.longest_gap() {
            Some(longest) => longest.saturating_mul(SILENCE_FACTOR).max(floor).min(ceiling),
            None => ceiling,
        }
    }
}

/// Gaps between pictures seen before their pace is told ([`PictureRate`]):
/// three, so that one odd gap among the first (a pause, the first gap of a
/// stream with B pictures) is outvoted from the first reading on.
pub const RATE_GAPS: usize = 3;

/// The newest gaps the pace is read from ([`PictureRate`]): half a second
/// of a 30-a-second webcam, fifteen seconds of a one-a-second one.
const RATE_WINDOW: usize = 15;

/// The pace of a source's pictures (its first video track), across its
/// connections: the median of the newest [`RATE_WINDOW`] gaps between two
/// pictures. A median, not a mean: a webcam that pauses 3 s on the same
/// connection and goes on is still a 30-a-second webcam (a running mean read
/// it as 2.5 a second for a dozen pictures, W4 review), while one that slows
/// down at night is followed once half the window is slow. What the RTSP
/// server announces to a reader that would otherwise wait for twenty pictures
/// to guess it (FFmpeg, D36).
///
/// The gaps are those of the pictures' own dates (their timestamps on the
/// track's timeline), not of their arrival: a camera read through a relay
/// that replays its group of pictures, or one that sends in bursts, delivers
/// thirty pictures in a millisecond that are 70 ms apart, and that is what a
/// reader guessing the rate reads too (W4 review: FFmpeg's own guess, which
/// the announcement replaces, reads the timestamps). A source that has no
/// clock of its own (a webcam over HTTP) dates its pictures as they arrive.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct PictureRate {
    /// The latest date seen on this connection, and how many pictures dated
    /// at or before it came since: B pictures, which come in decoding order
    /// after the picture they precede.
    latest: Option<(Duration, u32)>,
    /// The newest gaps, in microseconds, oldest overwritten first.
    gaps: [u64; RATE_WINDOW],
    /// How many of `gaps` hold one (up to [`RATE_WINDOW`]).
    seen: usize,
    /// Where the next gap goes.
    next: usize,
}

impl PictureRate {
    /// A picture dated `at` on its track's timeline. One dated after every
    /// other closes a gap: the time since the latest date, shared between the
    /// pictures that came since (itself, and the B pictures before it).
    pub(crate) fn picture(&mut self, at: Duration) {
        let Some((latest, earlier)) = self.latest else {
            self.latest = Some((at, 0));
            return;
        };
        if at <= latest {
            self.latest = Some((latest, earlier.saturating_add(1)));
            return;
        }
        let each = at.saturating_sub(latest) / earlier.saturating_add(1);
        if let Some(slot) = self.gaps.get_mut(self.next) {
            *slot = u64::try_from(each.as_micros()).unwrap_or(u64::MAX);
        }
        self.next = (self.next + 1) % RATE_WINDOW;
        self.seen = (self.seen + 1).min(RATE_WINDOW);
        self.latest = Some((at, 0));
    }

    /// A new connection: its first picture follows none of its own.
    pub(crate) fn restart(&mut self) {
        self.latest = None;
    }

    /// The median time between two pictures (the upper one of an even
    /// count), once [`RATE_GAPS`] were seen.
    pub(crate) fn interval(&self) -> Option<Duration> {
        if self.seen < RATE_GAPS {
            return None;
        }
        let mut gaps = self.gaps;
        let seen = gaps.get_mut(..self.seen)?;
        seen.sort_unstable();
        let median = *seen.get(self.seen / 2)?;
        (median > 0).then(|| Duration::from_micros(median))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FLOOR: Duration = Duration::from_secs(3);
    const CEILING: Duration = Duration::from_secs(15);

    /// `count` gaps of `gap` after the frame at `now`, which moves on.
    fn feed(cadence: &mut Cadence, now: &mut Instant, gap: Duration, count: u32) {
        cadence.frame(*now);
        for _ in 0..count {
            *now += gap;
            cadence.frame(*now);
        }
    }

    fn fed(gap: Duration, count: u32) -> Cadence {
        let mut cadence = Cadence::default();
        feed(&mut cadence, &mut Instant::now(), gap, count);
        cadence
    }

    #[test]
    fn a_fast_camera_may_be_silent_for_the_floor() {
        let cadence = fed(Duration::from_millis(40), 300);
        assert_eq!(cadence.silence_after(FLOOR, CEILING), FLOOR);
    }

    #[test]
    fn a_picture_a_second_may_be_silent_four_seconds() {
        let cadence = fed(Duration::from_secs(1), 12);
        assert_eq!(cadence.silence_after(FLOOR, CEILING), Duration::from_secs(4));
    }

    #[test]
    fn the_ceiling_holds_until_the_cadence_is_trusted() {
        // Enough gaps, too short a span; then a long span of too few gaps.
        assert_eq!(fed(Duration::from_millis(40), 200).silence_after(FLOOR, CEILING), CEILING);
        assert_eq!(fed(Duration::from_secs(2), LEARN_GAPS - 1).silence_after(FLOOR, CEILING), CEILING);
        assert_eq!(Cadence::default().silence_after(FLOOR, CEILING), CEILING);
    }

    #[test]
    fn one_long_gap_raises_the_silence_for_good_and_never_past_the_ceiling() {
        let (mut cadence, mut now) = (Cadence::default(), Instant::now());
        feed(&mut cadence, &mut now, Duration::from_millis(40), 300);
        feed(&mut cadence, &mut now, Duration::from_secs(2), 1);
        assert_eq!(cadence.silence_after(FLOOR, CEILING), Duration::from_secs(8));
        feed(&mut cadence, &mut now, Duration::from_millis(40), 1000);
        assert_eq!(cadence.silence_after(FLOOR, CEILING), Duration::from_secs(8), "the longest gap is not forgotten");
        feed(&mut cadence, &mut now, Duration::from_secs(5), 1);
        assert_eq!(cadence.silence_after(FLOOR, CEILING), CEILING);
    }

    #[test]
    fn the_time_between_two_connections_is_not_a_gap() {
        let (mut cadence, mut now) = (Cadence::default(), Instant::now());
        feed(&mut cadence, &mut now, Duration::from_millis(40), 300);
        cadence.restart();
        now += Duration::from_secs(60);
        feed(&mut cadence, &mut now, Duration::from_millis(40), 3);
        assert_eq!(cadence.longest_gap(), Some(Duration::from_millis(40)));
    }

    #[test]
    fn a_ceiling_below_the_floor_wins() {
        let cadence = fed(Duration::from_millis(40), 300);
        assert_eq!(cadence.silence_after(FLOOR, Duration::from_secs(2)), Duration::from_secs(2));
    }

    #[test]
    fn the_pace_of_pictures_is_told_after_three_gaps_and_follows_a_change() {
        let (mut rate, mut now) = (PictureRate::default(), Duration::from_secs(100));
        rate.picture(now);
        for _ in 0..2 {
            now += Duration::from_secs(1);
            rate.picture(now);
        }
        assert_eq!(rate.interval(), None, "two gaps are not a pace: one odd gap would be half of them");
        now += Duration::from_secs(1);
        rate.picture(now);
        assert_eq!(rate.interval(), Some(Duration::from_secs(1)));
        for _ in 0..60 {
            now += Duration::from_millis(40);
            rate.picture(now);
        }
        let fast = rate.interval().unwrap();
        assert!(fast < Duration::from_millis(45), "{fast:?}");
        rate.restart();
        now += Duration::from_secs(30);
        rate.picture(now);
        assert_eq!(rate.interval(), Some(fast), "the time between two connections is not a gap");
    }

    #[test]
    fn a_pause_of_a_fast_source_does_not_make_it_look_slow() {
        // A 30-a-second webcam over the internet pauses 3 s and goes on, on the
        // same connection: a running mean read 404 ms (2.5 pictures a second)
        // for the next dozen pictures, and a transcode started then was told
        // that rate for as long as it ran (W4 review).
        let (mut rate, mut now) = (PictureRate::default(), Duration::ZERO);
        let fast = Duration::from_millis(33);
        rate.picture(now);
        for _ in 0..60 {
            now += fast;
            rate.picture(now);
        }
        now += Duration::from_secs(3);
        rate.picture(now);
        for _ in 0..3 {
            let read = rate.interval().unwrap();
            assert!(read < ANNOUNCE_FLOOR_FOR_TESTS, "a pause read as the pace: {read:?}");
            now += fast;
            rate.picture(now);
        }
        // A slowdown that lasts is followed (a webcam at night): within a few pictures.
        for _ in 0..8 {
            now += Duration::from_secs(1);
            rate.picture(now);
        }
        assert_eq!(rate.interval(), Some(Duration::from_secs(1)));
    }

    #[test]
    fn the_first_gaps_of_a_source_outvote_one_odd_gap() {
        // A pause as the second gap a source ever shows, then as the first.
        for gaps in [[33, 3000, 33], [3000, 33, 33]] {
            let (mut rate, mut now) = (PictureRate::default(), Duration::ZERO);
            rate.picture(now);
            for gap in gaps {
                now += Duration::from_millis(gap);
                rate.picture(now);
            }
            assert_eq!(rate.interval(), Some(Duration::from_millis(33)), "{gaps:?}");
        }
    }

    #[test]
    fn b_pictures_come_before_their_date_and_the_pace_is_still_one_picture() {
        // I P B B P B B..., in decoding order, 40 ms a picture: dated 0, 120,
        // 40, 80, 240, 160, 200... The time between two pictures that arrive
        // one after the other is 120, 80 or 40 ms, never the pace.
        let mut rate = PictureRate::default();
        let at = |n: u64| Duration::from_millis(40 * n);
        rate.picture(at(0));
        for group in 0..6 {
            let p = 3 * (group + 1);
            for n in [p, p - 2, p - 1] {
                rate.picture(at(n));
            }
        }
        assert_eq!(rate.interval(), Some(Duration::from_millis(40)));
        // Two pictures with one date (a camera that repeats a timestamp) are
        // no gap of zero.
        let mut rate = PictureRate::default();
        for n in [0, 1, 1, 3, 4, 5, 6] {
            rate.picture(at(n));
        }
        assert_eq!(rate.interval(), Some(Duration::from_millis(40)));
    }

    /// The RTSP server announces a pace of 100 ms and slower (`sdp.rs`).
    const ANNOUNCE_FLOOR_FOR_TESTS: Duration = Duration::from_millis(100);
}
