//! The pace of a source's pictures, read from their dates.
//!
//! A reader that is not told a video track's frame rate guesses it from the
//! pictures' timestamps (FFmpeg reads twenty of them before it opens its
//! input: 20 s on a webcam of one picture a second). The channel keeps what
//! that guess would read - the pace of the pictures, and the grid their dates
//! sit on - so that the RTSP server can say it instead (`a=framerate`, D36).

use std::time::Duration;

/// Gaps between pictures seen before their pace is told ([`PictureRate`]):
/// three, so that one odd gap among the first (a pause, the first gap of a
/// stream with B pictures) is outvoted from the first reading on.
pub const RATE_GAPS: usize = 3;

/// The newest gaps the pace is read from ([`PictureRate`]): half a second
/// of a 30-a-second webcam, fifteen seconds of a one-a-second one.
const RATE_WINDOW: usize = 15;

/// The finest grid of dates that is a camera's own ([`PictureRate::grid`]):
/// a five-hundredth of a second, 2 ms. A finer step is a clock's resolution
/// (a camera that stamps in milliseconds), not a frame rate.
const GRID_PER_SECOND: u64 = 500;

/// The most slots of its grid between the two closest pictures of a camera
/// ([`PictureRate::grid`]): one that fills fewer than one slot in eight is
/// read at its pace.
const GRID_SLOTS: u64 = 8;

/// Gaps seen before a grid finer than the smallest of them is believed: with
/// three gaps, some fraction of the smallest fits by chance.
const GRID_GAPS: usize = 8;

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
///
/// The gaps are kept in the ticks of the track's clock, so that the grid a
/// camera stamps on can be read exactly ([`Self::grid`]).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct PictureRate {
    /// The clock the dates are counted in; 0 before the first picture.
    clock: u32,
    /// The latest date seen on this connection, and how many pictures dated
    /// at or before it came since: B pictures, which come in decoding order
    /// after the picture they precede.
    latest: Option<(i64, u32)>,
    /// The newest gaps, in ticks, oldest overwritten first.
    gaps: [u64; RATE_WINDOW],
    /// How many of `gaps` hold one (up to [`RATE_WINDOW`]).
    seen: usize,
    /// Where the next gap goes.
    next: usize,
}

impl PictureRate {
    /// A picture dated `at` ticks of a `clock` Hz clock on its track's
    /// timeline. One dated after every other closes a gap: the time since the
    /// latest date, shared between the pictures that came since (itself, and
    /// the B pictures before it). A track on another clock starts over: its
    /// steps do not compare with the earlier ones.
    pub(crate) fn picture(&mut self, at: i64, clock: u32) {
        let clock = clock.max(1);
        if self.clock != clock {
            *self = Self { clock, ..Self::default() };
        }
        let Some((latest, earlier)) = self.latest else {
            self.latest = Some((at, 0));
            return;
        };
        if at <= latest {
            self.latest = Some((latest, earlier.saturating_add(1)));
            return;
        }
        let step = u64::try_from(at.saturating_sub(latest)).unwrap_or(0);
        if let Some(slot) = self.gaps.get_mut(self.next) {
            *slot = step / u64::from(earlier.saturating_add(1));
        }
        self.next = (self.next + 1) % RATE_WINDOW;
        self.seen = (self.seen + 1).min(RATE_WINDOW);
        self.latest = Some((at, 0));
    }

    /// A new connection: its first picture follows none of its own.
    pub(crate) fn restart(&mut self) {
        self.latest = None;
    }

    /// The gaps seen, once there are [`RATE_GAPS`] of them.
    fn known(&self) -> Option<&[u64]> {
        self.gaps.get(..self.seen).filter(|gaps| gaps.len() >= RATE_GAPS)
    }

    fn time(&self, ticks: u64) -> Duration {
        crate::timeline::duration(i64::try_from(ticks).unwrap_or(i64::MAX), self.clock)
    }

    /// The median time between two pictures (the upper one of an even
    /// count), once [`RATE_GAPS`] were seen.
    pub(crate) fn interval(&self) -> Option<Duration> {
        let mut gaps = self.gaps;
        let seen = gaps.get_mut(..self.known()?.len())?;
        seen.sort_unstable();
        let median = *seen.get(seen.len() / 2)?;
        (median > 0).then(|| self.time(median))
    }

    /// The grid the pictures are dated on, when they are: the coarsest step
    /// every gap is a whole number of, give or take a tenth of it, and
    /// coarser than a [`GRID_PER_SECOND`]th of a second. A camera with a
    /// clock of its own stamps on one - 40 ms for 25 pictures a second - and
    /// may leave slots empty: the owner's ESP32 fills one slot in three (8
    /// pictures a second, dated 80, 120 or 160 ms apart, each within a
    /// millisecond of its slot). A reader that guesses the frame rate from
    /// the dates reads the grid, not the pace (FFmpeg: `25/1` for that
    /// camera), so the grid is what replaces its guess. For a camera that
    /// fills every slot the grid is its pace. `None` for pictures dated as
    /// they arrive over a network, whose gaps share no step.
    pub(crate) fn grid(&self) -> Option<Duration> {
        let gaps = self.known()?;
        let smallest = gaps.iter().copied().filter(|gap| *gap > 0).min()?;
        let finest = (u64::from(self.clock) / GRID_PER_SECOND).max(1);
        (1..=GRID_SLOTS)
            .filter(|slots| *slots == 1 || gaps.len() >= GRID_GAPS)
            .take_while(|slots| smallest / slots > finest)
            .find_map(|slots| step_of(gaps, smallest, slots))
            .filter(|step| *step > finest)
            .map(|step| self.time(step))
    }
}

/// The step of the grid `gaps` sit on when the smallest of them, `smallest`,
/// is `slots` steps: every gap a whole number of steps, off by less than a
/// tenth of a step (root mean square). The step is then the time the gaps
/// span over the steps they count. Whole numbers only, saturating: a gap is a
/// source's to choose.
fn step_of(gaps: &[u64], smallest: u64, slots: u64) -> Option<u64> {
    let unit = u128::from(smallest);
    let (mut steps, mut ticks, mut off) = (0u128, 0u128, 0u128);
    for gap in gaps {
        // In `1 / smallest`ths of a step.
        let scaled = u128::from(*gap).saturating_mul(u128::from(slots));
        let whole = (scaled.saturating_add(unit / 2).checked_div(unit)?).max(1);
        let miss = scaled.abs_diff(whole.saturating_mul(unit));
        off = off.saturating_add(miss.saturating_mul(miss));
        steps = steps.saturating_add(whole);
        ticks = ticks.saturating_add(u128::from(*gap));
    }
    // rms(miss / unit) < 1/10, squared and cleared of its fractions.
    let count = u128::try_from(gaps.len()).ok()?;
    let within = off.saturating_mul(100) < count.saturating_mul(unit).saturating_mul(unit);
    within.then(|| ticks.checked_div(steps)).flatten().and_then(|step| u64::try_from(step).ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A video track's clock.
    const CLOCK: u32 = 90_000;

    /// A picture dated `ms` on a [`CLOCK`] Hz clock.
    fn picture(rate: &mut PictureRate, ms: u64) {
        rate.picture(i64::try_from(ms * 90).unwrap(), CLOCK);
    }

    /// Pictures dated `gaps` ms after one another, from 0.
    fn dated(gaps: &[u64]) -> PictureRate {
        let (mut rate, mut now) = (PictureRate::default(), 0);
        picture(&mut rate, now);
        for gap in gaps {
            now += gap;
            picture(&mut rate, now);
        }
        rate
    }

    #[test]
    fn the_pace_of_pictures_is_told_after_three_gaps_and_follows_a_change() {
        let (mut rate, mut now) = (PictureRate::default(), 100_000);
        picture(&mut rate, now);
        for _ in 0..2 {
            now += 1000;
            picture(&mut rate, now);
        }
        assert_eq!(rate.interval(), None, "two gaps are not a pace: one odd gap would be half of them");
        assert_eq!(rate.grid(), None);
        now += 1000;
        picture(&mut rate, now);
        assert_eq!(rate.interval(), Some(Duration::from_secs(1)));
        for _ in 0..60 {
            now += 40;
            picture(&mut rate, now);
        }
        let fast = rate.interval().unwrap();
        assert!(fast < Duration::from_millis(45), "{fast:?}");
        rate.restart();
        now += 30_000;
        picture(&mut rate, now);
        assert_eq!(rate.interval(), Some(fast), "the time between two connections is not a gap");
    }

    #[test]
    fn a_pause_of_a_fast_source_does_not_make_it_look_slow() {
        // A 30-a-second webcam over the internet pauses 3 s and goes on, on the
        // same connection: a running mean read 404 ms (2.5 pictures a second)
        // for the next dozen pictures, and a transcode started then was told
        // that rate for as long as it ran (W4 review).
        let (mut rate, mut now) = (PictureRate::default(), 0);
        picture(&mut rate, now);
        for _ in 0..60 {
            now += 33;
            picture(&mut rate, now);
        }
        now += 3000;
        picture(&mut rate, now);
        for _ in 0..3 {
            let read = rate.interval().unwrap();
            assert!(read < ANNOUNCE_FLOOR_FOR_TESTS, "a pause read as the pace: {read:?}");
            now += 33;
            picture(&mut rate, now);
        }
        // A slowdown that lasts is followed (a webcam at night): within a few pictures.
        for _ in 0..8 {
            now += 1000;
            picture(&mut rate, now);
        }
        assert_eq!(rate.interval(), Some(Duration::from_secs(1)));
    }

    #[test]
    fn the_first_gaps_of_a_source_outvote_one_odd_gap() {
        // A pause as the second gap a source ever shows, then as the first.
        for gaps in [[33, 3000, 33], [3000, 33, 33]] {
            assert_eq!(dated(&gaps).interval(), Some(Duration::from_millis(33)), "{gaps:?}");
        }
    }

    #[test]
    fn b_pictures_come_before_their_date_and_the_pace_is_still_one_picture() {
        // I P B B P B B..., in decoding order, 40 ms a picture: dated 0, 120,
        // 40, 80, 240, 160, 200... The time between two pictures that arrive
        // one after the other is 120, 80 or 40 ms, never the pace.
        let mut rate = PictureRate::default();
        picture(&mut rate, 0);
        for group in 0..6 {
            let p = 3 * (group + 1);
            for n in [p, p - 2, p - 1] {
                picture(&mut rate, 40 * n);
            }
        }
        assert_eq!(rate.interval(), Some(Duration::from_millis(40)));
        assert_eq!(rate.grid(), Some(Duration::from_millis(40)));
        // Two pictures with one date (a camera that repeats a timestamp) are
        // no gap of zero.
        let mut rate = PictureRate::default();
        for n in [0, 1, 1, 3, 4, 5, 6] {
            picture(&mut rate, 40 * n);
        }
        assert_eq!(rate.interval(), Some(Duration::from_millis(40)));
    }

    /// What the owner's ESP32 dated its pictures, in ticks of 90 kHz, read
    /// through his relay on 2026-11-19 (ffprobe's `pts_time`): 5 pictures a
    /// second, 160 to 520 ms apart, each date within a millisecond of a
    /// multiple of 40 ms.
    const ESP32: [i64; 12] = [21591, 14400, 21609, 14373, 18018, 28773, 46773, 17991, 18063, 14319, 14391, 18000];

    fn stepped(steps: &[i64]) -> PictureRate {
        let (mut rate, mut at) = (PictureRate::default(), 0);
        rate.picture(at, CLOCK);
        for step in steps {
            at += step;
            rate.picture(at, CLOCK);
        }
        rate
    }

    fn near(read: Option<Duration>, ms: f64) -> bool {
        read.is_some_and(|d| (d.as_secs_f64() * 1000.0 - ms).abs() < 0.1)
    }

    #[test]
    fn a_camera_that_leaves_slots_of_its_grid_empty_is_read_at_its_grid() {
        // FFmpeg's guess reads 25 pictures a second from these dates; the
        // median pace, 200 ms, had the transcode encoded at twice the bits.
        let esp32 = stepped(&ESP32);
        assert!(near(esp32.interval(), 200.2), "{:?}", esp32.interval());
        assert!(near(esp32.grid(), 40.0), "{:?}", esp32.grid());
        // The same camera sending 8 a second, on exact slots.
        let exact = dated(&[120, 120, 80, 160, 80, 120, 120, 120, 120, 240, 120, 80]);
        assert_eq!(exact.interval(), Some(Duration::from_millis(120)));
        assert_eq!(exact.grid(), Some(Duration::from_millis(40)));
        // A camera that fills every slot: the grid is its pace (his H.265
        // cameras, 6300 ticks a picture; a 30-a-second one, 3000 ticks, which
        // no whole number of microseconds holds).
        for ticks in [6300_i64, 3000, 3600] {
            let rate = stepped(&[ticks; 9]);
            assert_eq!(rate.grid(), rate.interval(), "{ticks} ticks a picture");
            assert_eq!(rate.grid(), Some(crate::timeline::duration(ticks, CLOCK)));
        }
        // One that stamps in whole milliseconds, 40 or 41 of them.
        assert!(near(dated(&[40, 41, 40, 40, 41, 40]).grid(), 40.3));
    }

    #[test]
    fn a_grid_finer_than_the_smallest_gap_needs_more_than_a_few_gaps() {
        // Three gaps of a camera on a 40 ms grid: 80 and 120 ms share a step,
        // and so would three gaps of anything by chance.
        assert_eq!(dated(&[120, 120, 80]).grid(), None);
        assert_eq!(dated(&[120, 120, 80]).interval(), Some(Duration::from_millis(120)));
        assert_eq!(dated(&[120, 120, 80, 120, 160, 120, 80, 120]).grid(), Some(Duration::from_millis(40)));
        // Three even gaps are a grid at once: it is the pace.
        assert_eq!(dated(&[70, 70, 70]).grid(), Some(Duration::from_millis(70)));
    }

    #[test]
    fn pictures_dated_as_they_arrive_sit_on_no_grid() {
        // A webcam over the internet: 25 pictures a second on average, each
        // dated when it came, 22 to 55 ms after the one before.
        let mut steps = [0_i64; 20];
        for (n, step) in (0_i64..).zip(&mut steps) {
            *step = 2000 + (n * 1237) % 2900;
        }
        let webcam = stepped(&steps);
        assert!(webcam.interval().is_some());
        assert_eq!(webcam.grid(), None);
        // A step finer than 2 ms is a clock's, not a camera's: no grid there.
        assert_eq!(stepped(&[100, 300, 200, 500, 100, 400, 300, 200, 100, 500]).grid(), None);
        // Gaps a source chose to overflow with are no grid and no panic.
        let mut hostile = PictureRate::default();
        for at in [0, 1, i64::MAX / 2, i64::MAX - 1, i64::MAX] {
            hostile.picture(at, CLOCK);
        }
        assert_eq!(hostile.grid(), None);
    }

    #[test]
    fn a_track_on_another_clock_starts_the_pace_over() {
        let mut rate = dated(&[40, 40, 40, 40]);
        assert_eq!(rate.interval(), Some(Duration::from_millis(40)));
        // The same source's new track list, on a 1 kHz clock: 100 ms a picture.
        for n in 0..3 {
            rate.picture(n * 100, 1000);
        }
        assert_eq!(rate.interval(), None, "two gaps on the new clock");
        rate.picture(300, 1000);
        assert_eq!(rate.interval(), Some(Duration::from_millis(100)));
        assert_eq!(rate.grid(), Some(Duration::from_millis(100)));
    }

    /// The RTSP server announces a pace of 100 ms and slower (`sdp.rs`).
    const ANNOUNCE_FLOOR_FOR_TESTS: Duration = Duration::from_millis(100);
}
