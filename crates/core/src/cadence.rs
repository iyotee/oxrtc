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

/// Gaps between pictures seen before their pace is told ([`PictureRate`]).
pub const RATE_GAPS: u32 = 2;

/// The weight of the newest gap in the mean, as a divisor: an eighth.
const RATE_WEIGHT: u64 = 8;

/// The pace of a source's pictures (its first video track), across its
/// connections: a running mean of the time between two pictures, the newest
/// gaps weighing most, so a webcam that slows down at night is followed
/// within a few pictures. What the RTSP server announces to a reader that
/// would otherwise wait for twenty pictures to guess it (FFmpeg, D36).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct PictureRate {
    last: Option<Instant>,
    gaps: u32,
    mean_us: u64,
}

impl PictureRate {
    /// A picture arrived at `now`.
    pub(crate) fn picture(&mut self, now: Instant) {
        if let Some(last) = self.last.replace(now) {
            let gap = u64::try_from(now.saturating_duration_since(last).as_micros()).unwrap_or(u64::MAX);
            self.mean_us = if self.gaps == 0 {
                gap
            } else if gap >= self.mean_us {
                self.mean_us.saturating_add((gap - self.mean_us) / RATE_WEIGHT)
            } else {
                self.mean_us - (self.mean_us - gap) / RATE_WEIGHT
            };
            self.gaps = self.gaps.saturating_add(1);
        }
    }

    /// A new connection: its first picture follows none of its own.
    pub(crate) fn restart(&mut self) {
        self.last = None;
    }

    /// The mean time between two pictures, once [`RATE_GAPS`] were seen.
    pub(crate) fn interval(&self) -> Option<Duration> {
        (self.gaps >= RATE_GAPS && self.mean_us > 0).then(|| Duration::from_micros(self.mean_us))
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
    fn the_pace_of_pictures_is_told_after_two_gaps_and_follows_a_change() {
        let (mut rate, mut now) = (PictureRate::default(), Instant::now());
        rate.picture(now);
        now += Duration::from_secs(1);
        rate.picture(now);
        assert_eq!(rate.interval(), None, "one gap is not a pace");
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
}
