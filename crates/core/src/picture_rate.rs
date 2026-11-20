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

/// The most slots of its grid between two pictures of a camera, at its
/// median ([`PictureRate::grid`]): one that fills fewer than one slot in eight
/// is read at its pace.
const GRID_SLOTS: u32 = 8;

/// Gaps seen before a grid finer than the pace is believed: with three gaps,
/// some fraction of the pace fits by chance.
const GRID_GAPS: usize = 8;

/// How far the dates may sit from their slots and still be on a grid: a
/// variance of a hundredth of a slot squared, a tenth of a slot as a standard
/// deviation. What FFmpeg's guess allows too: on the owner's 30-a-second
/// webcam, dated as it arrives, both find the rate on an even minute (the
/// dates 0.08 of a slot off) and neither on an uneven one.
const GRID_VARIANCE: f64 = 0.01;

/// The spread under which the dates are on their slots, not near them: a
/// thousandth of a slot, squared. A camera's own clock, counted in the ticks
/// the dates are read in.
const EXACT_VARIANCE: f64 = 0.000_001;

/// How much wider the dates may spread around the slots of a whole number of
/// pictures a second than around those of the step measured, for the whole
/// number to be the rate: twice. A step measured over half a second of a
/// webcam dated as it arrives is good to a hundredth (its 30 a second read
/// 29.4 to 30.3), the dates fit 30 as well, and 30 is what the camera sends
/// and what FFmpeg's guess reads; a camera with a clock of its own that sends
/// 100 pictures in 7 seconds fits its own step exactly and 14 a second not
/// nearly as well, and is told as it is.
const WHOLE_RATE_SPREAD: f64 = 2.0;

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
/// The gaps are kept in the ticks of the track's clock, and in the order they
/// came, so that the grid a camera stamps on can be read ([`Self::grid`]).
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
        self.median().map(|median| self.time(median))
    }

    /// The median gap in ticks (the upper one of an even count), once
    /// [`RATE_GAPS`] were seen; never zero.
    fn median(&self) -> Option<u64> {
        let mut gaps = self.gaps;
        let seen = gaps.get_mut(..self.known()?.len())?;
        seen.sort_unstable();
        seen.get(seen.len() / 2).copied().filter(|median| *median > 0)
    }

    /// The newest gaps, oldest first.
    fn in_order(&self) -> impl Iterator<Item = u64> + Clone + '_ {
        let oldest = if self.seen == RATE_WINDOW { self.next } else { 0 };
        (0..self.seen).filter_map(move |nth| self.gaps.get((oldest + nth) % RATE_WINDOW).copied())
    }

    /// The grid the pictures are dated on, when they are: the coarsest step -
    /// about the median gap, or a half, a third ... an eighth of it - that
    /// every date is a whole number of from the first, within a tenth of a
    /// step ([`GRID_VARIANCE`]), and coarser than a [`GRID_PER_SECOND`]th of
    /// a second. Where the dates fit a whole number of pictures a second as
    /// well, that is the step ([`WHOLE_RATE_SPREAD`]).
    ///
    /// Dates that are on their slots ([`EXACT_VARIANCE`]) are a grid from
    /// three gaps, a grid finer than the pace from [`GRID_GAPS`]. Dates that
    /// are only near them are believed from a full window: three gaps of the
    /// owner's 30-a-second webcam, dated as it arrives, fit some step nearly
    /// every time, and it read 28 to 33 a second two times in three (the
    /// first transcode of a camera nothing holds is answered after three
    /// gaps); fifteen read 30, or nothing.
    ///
    /// A reader that guesses the frame rate from the dates reads this, not
    /// the pace: it is what replaces its guess (FFmpeg, D36). A camera with a
    /// clock of its own stamps on a grid - 40 ms for 25 pictures a second -
    /// and may leave slots empty: the owner's ESP32 fills one slot in three
    /// to five (dated 80 to 240 ms apart, each within a millisecond of its
    /// slot), and FFmpeg reads `25/1`. For a camera that fills every slot the
    /// grid is its pace, and so it is for a webcam dated as it arrives whose
    /// pictures come evenly. `None` when they do not: no rate is read from
    /// such dates by anyone.
    pub(crate) fn grid(&self) -> Option<Duration> {
        let gaps = self.known()?.len();
        let median = self.median()?;
        let clock = f64::from(self.clock);
        let finest = (u64::from(self.clock) / GRID_PER_SECOND).max(1);
        (1..=GRID_SLOTS)
            .filter(|slots| *slots == 1 || gaps >= GRID_GAPS)
            .take_while(|slots| median / u64::from(*slots) > finest)
            .find_map(|slots| step_of(self.in_order(), median as f64 / f64::from(slots), clock))
            .filter(|(step, spread)| *step > finest as f64 && (*spread < EXACT_VARIANCE || gaps == RATE_WINDOW))
            .and_then(|(step, _)| Duration::try_from_secs_f64(step / clock).ok())
    }
}

/// The step, in ticks of a `clock` Hz clock, of the grid the dates `gaps`
/// apart sit on, when it is about `about` ticks.
///
/// Each gap is the whole number of steps nearest to it, and the step measured
/// is the time the gaps span over the steps that makes. (Counting the steps
/// from the span and the median gap misread the owner's 30-a-second webcam
/// one start in ten: most of its pictures come 34 to 36 ms apart and one in
/// five 27 ms after the last, so fifteen gaps held 14.5 median ones.) The
/// dates must then sit within a tenth of a step of their slots: the variance
/// of what each is off by, the first being on its slot ([`GRID_VARIANCE`]).
/// The whole number of pictures a second nearest to the step measured is
/// preferred when the dates fit it too, no more than [`WHOLE_RATE_SPREAD`]
/// times as loosely. With the step, how far the dates spread around its
/// slots.
fn step_of(gaps: impl Iterator<Item = u64> + Clone, about: f64, clock: f64) -> Option<(f64, f64)> {
    let span: f64 = gaps.clone().map(|gap| gap as f64).sum();
    let steps: f64 = gaps.clone().map(|gap| (gap as f64 / about).round().max(1.0)).sum();
    let measured = span / steps;
    let spread = spread_of(gaps.clone(), measured);
    let whole = (clock / measured).round();
    if whole >= 1.0 {
        let step = clock / whole;
        let around = spread_of(gaps, step);
        if around < GRID_VARIANCE && around <= WHOLE_RATE_SPREAD * spread {
            return Some((step, around));
        }
    }
    // A span, a count or a spread that is not a number compares false.
    (spread < GRID_VARIANCE && measured > 0.0).then_some((measured, spread))
}

/// The variance of how far each of the dates `gaps` apart sits from its slot
/// of a grid of `step` ticks, in slots, the first date being on one.
fn spread_of(gaps: impl Iterator<Item = u64>, step: f64) -> f64 {
    let (mut at, mut dates, mut sum, mut squares) = (0.0, 1.0, 0.0, 0.0);
    for gap in gaps {
        at += gap as f64;
        let slot = at / step;
        let off = slot - slot.round();
        sum += off;
        squares += off * off;
        dates += 1.0;
    }
    let mean = sum / dates;
    squares / dates - mean * mean
}

#[cfg(test)]
#[path = "picture_rate_tests.rs"]
mod tests;
