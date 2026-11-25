//! A track's step between frames, and the gaps of a stalled source closed.
//!
//! A source that stalls for seconds with its clock going on (a camera whose
//! frames were lost on the network) comes back with a gap in its times. HLS
//! (D47 as amended by F6P) closes such a gap: every track's later frames are
//! written that much earlier, so that the media goes on where it stopped. A
//! player then shows the newest picture as soon as it comes, at the latency
//! it had - where a gap kept in the media makes hls.js wait for its buffer to
//! refill past the hole, and a gap filled by stretching the first frame after
//! it (MSE's rule, `oxrtc-mp4`'s timing) shows that frame for the whole
//! stall: a second freeze as long as the first. A part is then never as long
//! as the stall. The wall clock a segment is dated with stays the source's.

use std::collections::VecDeque;
use std::time::Duration;

/// The step between two frames of one track: the shortest of the last eight
/// (a lost frame or a stall makes one long step, which is no frame rate),
/// 1/25 s before there are two frames.
#[derive(Debug, Default, Clone)]
pub(crate) struct Steps {
    last: Option<Duration>,
    recent: VecDeque<Duration>,
}

/// How many steps [`Steps`] looks at.
const STEPS: usize = 8;

impl Steps {
    /// A frame at `at`.
    pub(crate) fn add(&mut self, at: Duration) {
        if let Some(last) = self.last
            && at > last
        {
            if self.recent.len() == STEPS {
                self.recent.pop_front();
            }
            self.recent.push_back(at - last);
        }
        self.last = Some(self.last.map_or(at, |last| last.max(at)));
    }

    /// The latest frame's time.
    pub(crate) fn last(&self) -> Option<Duration> {
        self.last
    }

    /// The step.
    pub(crate) fn step(&self) -> Duration {
        self.recent.iter().copied().min().unwrap_or(Duration::from_millis(40))
    }
}

/// The gaps closed in a feed's times: one shift for every track, so that
/// the picture and the sound stay together.
#[derive(Debug, Clone)]
pub(crate) struct Gaps {
    over: Option<Duration>,
    shift: Duration,
    tracks: Vec<Steps>,
}

impl Gaps {
    /// Gaps longer than `over` closed; none when `None`.
    pub(crate) fn new(over: Option<Duration>) -> Self {
        Self { over, shift: Duration::ZERO, tracks: Vec::new() }
    }

    /// How much earlier the frames are written than their source times.
    pub(crate) fn shift(&self) -> Duration {
        self.shift
    }

    /// A new track list: the steps start again, the shift stays.
    pub(crate) fn new_tracks(&mut self) {
        self.tracks.clear();
    }

    /// Where a frame of track `track` at source time `at` is written. The
    /// first frame after a gap longer than the bound, on any track, closes
    /// it to one step after that track's last frame; a frame of another track
    /// never goes back past its own track's last one.
    pub(crate) fn place(&mut self, track: usize, at: Duration) -> Duration {
        let Some(over) = self.over else { return at };
        if self.tracks.len() <= track {
            self.tracks.resize_with(track + 1, Steps::default);
        }
        let Some(steps) = self.tracks.get_mut(track) else { return at };
        let mut placed = at.saturating_sub(self.shift);
        if let Some(last) = steps.last() {
            if placed > last + over {
                let to = last + steps.step();
                self.shift += placed - to;
                placed = to;
            } else if placed < last {
                placed = last;
            }
        }
        steps.add(placed);
        placed
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    #[test]
    fn a_stall_is_closed_to_one_step_for_every_track() {
        let mut gaps = Gaps::new(Some(Duration::from_secs(1)));
        for n in 0..10 {
            assert_eq!(gaps.place(0, ms(n * 40)), ms(n * 40), "a steady picture is written at its times");
            assert_eq!(gaps.place(1, ms(n * 40 + 5)), ms(n * 40 + 5));
        }
        // Five seconds of nothing, the source's clock going on.
        assert_eq!(gaps.place(0, ms(360 + 5000)), ms(400), "one step after the last picture");
        assert_eq!(gaps.shift(), ms(4960));
        assert_eq!(gaps.place(1, ms(365 + 5000)), ms(405), "the sound with the same shift: still together");
        assert_eq!(gaps.place(0, ms(400 + 5000)), ms(440), "and the next pictures follow");
    }

    #[test]
    fn a_short_gap_is_left_and_none_is_closed_without_a_bound() {
        let mut gaps = Gaps::new(Some(Duration::from_secs(1)));
        gaps.place(0, ms(0));
        gaps.place(0, ms(40));
        assert_eq!(gaps.place(0, ms(800)), ms(800), "under the bound: left as it is");
        let mut none = Gaps::new(None);
        none.place(0, ms(0));
        assert_eq!(none.place(0, ms(9000)), ms(9000));
        assert_eq!(none.shift(), Duration::ZERO);
    }

    #[test]
    fn a_track_never_goes_back_past_its_last_frame() {
        let mut gaps = Gaps::new(Some(Duration::from_secs(1)));
        for at in [0, 40] {
            gaps.place(0, ms(at));
            gaps.place(1, ms(at));
        }
        // The sound shows the stall first, and a little longer: the picture
        // after it is then written at its last frame's time, not before.
        assert_eq!(gaps.place(1, ms(40 + 3000)), ms(80));
        assert_eq!(gaps.shift(), ms(2960));
        assert_eq!(gaps.place(0, ms(2990)), ms(40), "2990 - 2960 = 30, before its last picture at 40");
        let mut gaps = Gaps::new(Some(Duration::from_secs(1)));
        gaps.place(0, ms(0));
        gaps.place(0, ms(500));
        gaps.place(1, ms(0));
        gaps.place(1, ms(2000)); // the sound's gap closes 1960 ms
        assert_eq!(gaps.place(0, ms(2040)), ms(500), "2040 - 1960 = 80, before its last picture at 500: held at it");
    }
}
