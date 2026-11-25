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
        self.known().unwrap_or(Duration::from_millis(40))
    }

    /// The step, once there are two frames.
    pub(crate) fn known(&self) -> Option<Duration> {
        self.recent.iter().copied().min()
    }
}

/// A gap is closed only when it is this many of its track's steps at least
/// (and over the bound): a camera of one picture every two seconds has no
/// stall between its pictures.
const GAP_STEPS: u32 = 3;

/// One track as [`Gaps`] sees it: its steps in the source's times, and
/// where its last frame was written.
#[derive(Debug, Default, Clone)]
struct Track {
    steps: Steps,
    placed: Option<Duration>,
}

/// The gaps closed in a feed's times: one shift for every track, so that
/// the picture and the sound stay together. A gap is the whole source's -
/// no frame of any track for longer than the bound -, never one track's: a
/// picture that stalls while the sound goes on (or the sound alone dropping
/// out) is kept in time, as go2rtc and MediaMTX keep it, where closing it
/// would have piled the other track's frames onto one instant (F6P review).
#[derive(Debug, Clone)]
pub(crate) struct Gaps {
    over: Option<Duration>,
    shift: Duration,
    tracks: Vec<Track>,
    /// The latest source time of any track.
    latest: Option<Duration>,
    /// The latest gap closed: where it ended in the source's times, and the
    /// shift before it.
    hole: Option<(Duration, Duration)>,
}

impl Gaps {
    /// Gaps longer than `over` closed; none when `None`.
    pub(crate) fn new(over: Option<Duration>) -> Self {
        Self { over, shift: Duration::ZERO, tracks: Vec::new(), latest: None, hole: None }
    }

    /// How much earlier the frames are written than their source times.
    pub(crate) fn shift(&self) -> Duration {
        self.shift
    }

    /// A new track list: the steps start again, the shift stays.
    pub(crate) fn new_tracks(&mut self) {
        self.tracks.clear();
        self.latest = None;
        self.hole = None;
    }

    /// Where a frame of track `track` at source time `at` is written. The
    /// first frame after a gap of the whole source longer than the bound (and
    /// than [`GAP_STEPS`] of its track's steps), on any track, closes it to
    /// one step after that track's last frame; a frame never goes back past
    /// its own track's last one.
    pub(crate) fn place(&mut self, track: usize, at: Duration) -> Duration {
        let Some(over) = self.over else { return at };
        if self.tracks.len() <= track {
            self.tracks.resize_with(track + 1, Track::default);
        }
        // A frame from well inside the gap last closed, come after it: the
        // source was not silent, only that track late on the wire (FFmpeg
        // holds the picture back while the sound it interleaves with is
        // missing). The gap is opened again for what comes next - the frames
        // written in it stay where they are - so that the late track is
        // written at its own times, not piled onto its last frame.
        if let Some((end, before)) = self.hole
            && at + over < end
        {
            self.shift = before;
            self.hole = None;
        }
        let silent = self.latest.is_some_and(|latest| at > latest + over);
        let Some(this) = self.tracks.get_mut(track) else { return at };
        let mut placed = at.saturating_sub(self.shift);
        if let (Some(last), Some(written)) = (this.steps.last(), this.placed) {
            // A track of one frame yet has no rate to tell a stall by.
            let step = this.steps.known();
            if let Some(step) = step.filter(|step| silent && at.saturating_sub(last) > *step * GAP_STEPS) {
                let to = written + step;
                self.hole = Some((at, self.shift));
                self.shift += placed.saturating_sub(to);
                placed = to;
            } else if placed < written {
                placed = written;
            }
        }
        // The steps are the source's own: a closed gap is no frame rate.
        this.steps.add(at);
        this.placed = Some(placed);
        self.latest = Some(self.latest.map_or(at, |latest| latest.max(at)));
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
        gaps.place(1, ms(40));
        gaps.place(1, ms(2000)); // the whole source silent from 500: the sound's gap closes 1920 ms
        assert_eq!(gaps.shift(), ms(1920));
        assert_eq!(gaps.place(0, ms(2040)), ms(500), "2040 - 1920 = 120, before its last picture at 500: held at it");
    }

    /// One picture every two seconds is that camera's rate, not a stall:
    /// closed, its pictures were written 40 ms apart, a segment of a second
    /// took 25 of them - 50 s - to complete, and a player starved (F6P review).
    #[test]
    fn a_slow_camera_s_pictures_keep_their_times() {
        let mut gaps = Gaps::new(Some(Duration::from_secs(1)));
        for n in 0..10 {
            assert_eq!(gaps.place(0, ms(n * 2000)), ms(n * 2000), "picture {n}");
        }
        assert_eq!(gaps.shift(), Duration::ZERO);
        // A real stall of that camera, many of its steps, is still closed.
        assert_eq!(gaps.place(0, ms(18_000 + 30_000)), ms(20_000));
    }

    /// One track's gap while the other goes on is no stall of the source:
    /// the sound dropping out for 2 s used to shift every track, and the
    /// pictures of those 2 s were then all written at the last one's time
    /// (one instant: a frozen picture and 50 frames lost); a picture stalled
    /// while the sound goes on shifted the sound back the same way.
    #[test]
    fn one_track_s_gap_while_another_goes_on_is_kept() {
        for (gapped, going_on) in [(1, 0), (0, 1)] {
            let mut gaps = Gaps::new(Some(Duration::from_secs(1)));
            for n in 0..100u64 {
                let at = ms(n * 40);
                assert_eq!(gaps.place(going_on, at), at, "track {going_on} goes on");
                if !(25..75).contains(&n) {
                    assert_eq!(gaps.place(gapped, at + ms(5)), at + ms(5), "track {gapped}, frame {n}");
                }
            }
            assert_eq!(gaps.shift(), Duration::ZERO);
        }
    }

    /// The sound back from a drop-out before the picture that FFmpeg held
    /// back behind it (real path: apps/oxrtc/tests/hls.rs): the sound closes
    /// what looks like a stall, then the picture comes with its own times
    /// from inside it. Those pictures were all written at the last one's time
    /// (50 of them on one instant); the gap is opened again instead.
    #[test]
    fn a_track_late_from_inside_a_closed_gap_opens_it_again() {
        let mut gaps = Gaps::new(Some(Duration::from_secs(1)));
        for n in 0..6u64 {
            gaps.place(0, ms(n * 40)); // pictures to 200
            gaps.place(1, ms(1 + n * 64)); // sound to 321
        }
        assert_eq!(gaps.place(1, ms(2369)), ms(385), "the sound alone back: read as a stall, closed");
        assert_eq!(gaps.place(1, ms(2433)), ms(449));
        // The picture held back on the wire, from 240 on.
        for n in 6..70u64 {
            assert_eq!(gaps.place(0, ms(n * 40)), ms(n * 40), "picture {n} at its own time");
        }
        assert_eq!(gaps.shift(), Duration::ZERO);
        assert_eq!(gaps.place(1, ms(2497)), ms(2497), "the sound after it at its own time too");
        // A real stall afterwards is still closed.
        assert_eq!(gaps.place(0, ms(69 * 40 + 5000)), ms(69 * 40 + 40));
        // The sound a little behind the picture that closed it keeps it closed.
        assert_eq!(gaps.place(1, ms(69 * 40 + 4990)), ms(2790));
        assert_eq!(gaps.shift(), ms(4960));
    }
}
