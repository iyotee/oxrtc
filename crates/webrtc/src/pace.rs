//! The sound of a session, sent on the stream's own clock (D30).
//!
//! A camera's sound does not arrive as it was sampled. FFmpeg and most
//! cameras deliver 20 ms frames in pairs every 40 ms; a camera on a poor link
//! stops for a second and then delivers that second at once. A browser's
//! audio buffer follows the worst lateness it sees: it holds 60 ms for the
//! pairs and close to a second for the stalls, and since the picture and the
//! sound are one `MediaStream` (`msid.rs`) it holds the picture as long.
//!
//! [`SoundClock`] gives each sound frame the time it leaves, from the times
//! the stream's own frames carry:
//!
//! - the **earliest** arrival of the last [`WINDOW`], against the frames' own
//!   times, is the stream's clock as oxrtc can know it (a frame cannot arrive
//!   before it was sampled);
//! - a frame is **held** as long as the latest arrival of the last
//!   [`HOLD_WINDOW`] was behind that clock, [`MAX_HOLD`] at most: the pairs
//!   leave 20 ms apart, as they were sampled, and the browser's buffer has
//!   nothing to follow. The hold moves seldom (a step of the hold is itself
//!   a lateness the browser's buffer would follow);
//! - a frame later than the hold leaves at once, dated when it was due, so
//!   the browser sees its lateness as it is;
//! - a frame later than the session's bound ([`SoundClock::place`]'s
//!   [`Placed::Late`]) does not leave: the sound a stall kept back is not
//!   sent as a burst, by the rule the connection already applies
//!   (`Sender::stale`). The bound is for a stall, not for a source's way of
//!   delivering: a frame no later than this stream's frames usually are is
//!   never late, whatever the bound.
//!
//! No I/O and no timer here: the session sleeps until [`Held::next_due`].

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::Duration;

use oxrtc_core::Frame;
use tokio::time::Instant;

/// How far back the clock looks for the earliest and the latest arrival: what
/// a browser's audio buffer looks back over too.
pub(crate) const WINDOW: Duration = Duration::from_secs(2);

/// Arrivals remembered at most, whatever the window holds (frames of 10 ms
/// for two seconds are 200).
const MAX_ARRIVALS: usize = 256;

/// How long the hold remembers the latest arrival it had to cover.
pub(crate) const HOLD_WINDOW: Duration = Duration::from_secs(30);

/// Seconds of arrivals the hold remembers at most (a second each, and room
/// for arrivals that do not come in order).
const MAX_PEAKS: usize = 32;

/// The longest a sound frame is held to leave evenly spaced. The picture is
/// not held with it and the frame is dated when it leaves, so this is also
/// how far the sound may trail the picture without the browser knowing:
/// within the strictest broadcast tolerance for a sound that lags (45 ms,
/// ATSC IS-191; ITU-R BT.1359 puts its detection at 125 ms).
pub(crate) const MAX_HOLD: Duration = Duration::from_millis(40);

/// Sound frames waiting for their time at most; the oldest gives way.
pub(crate) const MAX_HELD: usize = 64;

/// What the clock makes of a frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Placed {
    /// The frame leaves at this instant (now, when it is past).
    Due(Instant),
    /// The frame is this much behind the stream's clock, more than the
    /// session's bound: it does not leave.
    Late(Duration),
}

/// The clock of one sound track, learnt from its frames.
#[derive(Debug)]
pub(crate) struct SoundClock {
    rate: u32,
    /// Arrivals are counted from here.
    origin: Instant,
    /// Per recent frame: its arrival, and its arrival minus its own time
    /// (both in microseconds). The smallest difference is the stream's clock.
    arrivals: VecDeque<(i64, i64)>,
    /// Per second of arrivals (its number, counted from the origin), over
    /// the last [`HOLD_WINDOW`]: how far behind the clock its latest frame
    /// was.
    peaks: VecDeque<(i64, i64)>,
    /// A frame this far behind the stream's clock is not sent; `None`: every
    /// frame is.
    late_max: Option<Duration>,
    /// The hold the last frame was placed with.
    hold: Duration,
    /// The second of the clock's first frame: none of its frames had an
    /// earlier one to be behind, so it says nothing of the source.
    first: Option<i64>,
}

fn micros(duration: Duration) -> i64 {
    i64::try_from(duration.as_micros()).unwrap_or(i64::MAX)
}

impl SoundClock {
    pub(crate) fn new(rate: u32, origin: Instant, late_max: Option<Duration>) -> Self {
        Self {
            rate: rate.max(1),
            origin,
            arrivals: VecDeque::new(),
            peaks: VecDeque::new(),
            late_max,
            hold: Duration::ZERO,
            first: None,
        }
    }

    /// The hold the last frame was placed with: how far the sound trails its
    /// arrival to leave evenly.
    pub(crate) fn hold(&self) -> Duration {
        self.hold
    }

    /// Notes that a frame arrived at `arrival` this far `behind` the clock.
    fn note(&mut self, arrival: i64, behind: i64) {
        let second = arrival / 1_000_000;
        self.first.get_or_insert(second);
        let horizon = second.saturating_sub(i64::try_from(HOLD_WINDOW.as_secs()).unwrap_or(i64::MAX));
        while self.peaks.front().is_some_and(|(at, _)| *at < horizon || *at > second) {
            self.peaks.pop_front();
        }
        match self.peaks.back_mut() {
            Some((at, peak)) if *at == second => *peak = (*peak).max(behind),
            _ => self.peaks.push_back((second, behind)),
        }
        while self.peaks.len() > MAX_PEAKS {
            self.peaks.pop_front();
        }
    }

    /// How far behind the clock this stream's latest frame is in nearly
    /// every second: the lowest fifth of the seconds remembered before the
    /// one of `arrival` (a burst does not excuse itself) and after the
    /// clock's first. A source that
    /// delivers its sound a second at a time is a second behind at every
    /// delivery, and that is not a stall; a camera that stalls in half of its
    /// seconds still stalls (the middle second would excuse it).
    fn usual(&self, arrival: i64) -> i64 {
        let second = arrival / 1_000_000;
        let mut peaks: Vec<i64> = self
            .peaks
            .iter()
            .filter(|(at, _)| *at != second && Some(*at) != self.first)
            .map(|(_, peak)| *peak)
            .collect();
        peaks.sort_unstable();
        peaks.get(peaks.len() / 5).copied().unwrap_or(0)
    }

    /// A frame's own time, in microseconds.
    fn media(&self, pts: i64) -> i64 {
        let wide = i128::from(pts).saturating_mul(1_000_000) / i128::from(self.rate);
        i64::try_from(wide).unwrap_or(if wide < 0 { i64::MIN } else { i64::MAX })
    }

    /// Places the frame of time `pts` (the track's clock) that entered at
    /// `received`.
    pub(crate) fn place(&mut self, pts: i64, received: Instant) -> Placed {
        let arrival = micros(received.saturating_duration_since(self.origin));
        let offset = arrival.saturating_sub(self.media(pts));
        let horizon = arrival.saturating_sub(micros(WINDOW));
        // The newest arrival older than the window is kept: a source that
        // stopped for longer than the window is still judged by the clock it
        // had, for the whole burst that follows (not only its first frame).
        while self.arrivals.len() > 1
            && (self.arrivals.get(1).is_some_and(|(at, _)| *at < horizon) || self.arrivals.len() >= MAX_ARRIVALS)
        {
            self.arrivals.pop_front();
        }
        self.arrivals.push_back((arrival, offset));
        let earliest = self.arrivals.iter().map(|(_, o)| *o).min().unwrap_or(offset);
        let latest = self.arrivals.iter().map(|(_, o)| *o).max().unwrap_or(offset);
        let behind = offset.saturating_sub(earliest).max(0);
        let usual = self.usual(arrival);
        self.note(arrival, behind);
        if self.late_max.is_some_and(|max| behind > micros(max).max(usual)) {
            return Placed::Late(Duration::from_micros(u64::try_from(behind).unwrap_or(0)));
        }
        // The hold every frame of the last HOLD_WINDOW could have left with
        // (a frame is known to have been behind only once a later one came
        // earlier: the window's spread says it), bounded: a frame later than
        // the bound leaves late, not the others with it.
        let spread = latest.saturating_sub(earliest);
        let peak = self.peaks.iter().map(|(_, peak)| *peak).max().unwrap_or(behind);
        let hold = peak.max(spread).min(micros(MAX_HOLD));
        self.hold = Duration::from_micros(u64::try_from(hold).unwrap_or(0));
        let line = earliest.saturating_add(hold);
        let due = Duration::from_micros(u64::try_from(self.media(pts).saturating_add(line)).unwrap_or(0));
        // Never after the frame's arrival plus the hold, whatever its time says.
        Placed::Due(self.origin.checked_add(due).map_or(received, |at| at.min(received + MAX_HOLD)))
    }
}

/// A sound frame waiting for its time.
#[derive(Debug)]
pub(crate) struct Waiting {
    pub(crate) track: usize,
    pub(crate) frame: Arc<Frame>,
    /// When it leaves, and what it is dated.
    pub(crate) due: Instant,
    /// Whether its time was still ahead when it was queued: a frame the
    /// session's timer has to wake for (one already late is not).
    pub(crate) timed: bool,
}

/// The sound frames of a session that wait for their time, in the order they
/// came. Bounded: [`MAX_HELD`].
#[derive(Debug, Default)]
pub(crate) struct Held {
    frames: VecDeque<Waiting>,
}

impl Held {
    /// Queues at `now` a frame due at `due`, never before the one queued
    /// ahead of it (a clock's line can move back; the order cannot). Returns
    /// the frame that gave way when the queue was full.
    pub(crate) fn push(&mut self, track: usize, frame: Arc<Frame>, due: Instant, now: Instant) -> Option<Waiting> {
        let due = self.frames.back().map_or(due, |last| last.due.max(due));
        let dropped = if self.frames.len() >= MAX_HELD { self.frames.pop_front() } else { None };
        self.frames.push_back(Waiting { track, frame, due, timed: due > now });
        dropped
    }

    /// The next frame whose time has come at `now`.
    pub(crate) fn pop_due(&mut self, now: Instant) -> Option<Waiting> {
        if self.frames.front().is_some_and(|w| w.due <= now) { self.frames.pop_front() } else { None }
    }

    /// When the next frame leaves.
    pub(crate) fn next_due(&self) -> Option<Instant> {
        self.frames.front().map(|w| w.due)
    }

    /// Forgets what waits (the tracks changed).
    pub(crate) fn clear(&mut self) {
        self.frames.clear();
    }

    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.frames.len()
    }
}

#[cfg(test)]
#[path = "pace_tests.rs"]
mod tests;
