//! Where each sample goes on its track's decode timeline, and how long it
//! lasts.
//!
//! A browser's MSE takes a sample whose decode time is more than twice the
//! previous sample's duration after it for a discontinuity, and then drops
//! every frame until the next keyframe (Media Source Extensions, coded frame
//! processing, step 6); a gap in what is buffered stops playback. So samples
//! are **contiguous**: each starts where the previous one ended. What makes
//! them follow the source is the duration: a sample lasts until where the
//! next one is expected (its decode time plus the track's typical step), and
//! when the next one comes while the previous is still unwritten (a fragment
//! of several frames), the previous one's duration is set to reach it
//! exactly. A sample is therefore placed at most one step's misprediction
//! from its source time, and that error never accumulates.
//!
//! The typical step is the sound frame's own length for audio (known from its
//! content), and for video the shortest of the last eight steps between two
//! decode times (1/25 s before there are two), held to 1 tick to 1 s: a lost
//! frame makes one long step, which must not make the next frames expected
//! late (each would then be squeezed to a tick, never shown); a frame rate
//! that drops is followed eight frames later.

/// The placement state of one track.
#[derive(Debug, Clone)]
pub(crate) struct Timing {
    timescale: u32,
    /// Where the next sample starts: the end of the last one placed.
    next: Option<i64>,
    /// The last source decode time seen.
    last_dts: Option<i64>,
    /// The last steps between two source decode times; the next goes at
    /// `steps_at`, and the first `steps_len` are filled.
    steps: [i64; STEPS],
    steps_len: usize,
    steps_at: usize,
    /// The shortest of `steps`, or 1/25 s before there is one.
    step: i64,
}

/// How many steps the video estimate looks at.
const STEPS: usize = 8;

/// The longest duration a sample is given, in ticks: `u32`'s, the field's.
const MAX_DURATION: i64 = u32::MAX as i64;

impl Timing {
    pub(crate) fn new(timescale: u32) -> Self {
        let timescale = timescale.max(1);
        let step = i64::from(timescale / 25).max(1);
        Self { timescale, next: None, last_dts: None, steps: [0; STEPS], steps_len: 0, steps_at: 0, step }
    }

    /// Places a sample of source decode time `dts`; `content` is its length
    /// when the content says it (audio). Returns its decode time and duration.
    pub(crate) fn place(&mut self, dts: i64, content: Option<u32>) -> (i64, u32) {
        if let Some(last) = self.last_dts
            && dts > last
        {
            let step = dts.saturating_sub(last).clamp(1, i64::from(self.timescale));
            if let Some(slot) = self.steps.get_mut(self.steps_at) {
                *slot = step;
            }
            self.steps_at = (self.steps_at + 1) % STEPS;
            self.steps_len = (self.steps_len + 1).min(STEPS);
            self.step = self.steps.iter().take(self.steps_len).copied().min().unwrap_or(step);
        }
        self.last_dts = Some(dts);
        let placed = self.next.unwrap_or(dts);
        let step = content.map_or(self.step, i64::from);
        let duration = dts.saturating_add(step).saturating_sub(placed).clamp(1, MAX_DURATION);
        self.next = Some(placed.saturating_add(duration));
        (placed, duration as u32)
    }

    /// The previous sample, placed at `placed` and still unwritten, now ends
    /// where a sample of source decode time `dts` goes: its new duration.
    pub(crate) fn reach(&mut self, placed: i64, dts: i64) -> u32 {
        let duration = dts.saturating_sub(placed).clamp(1, MAX_DURATION);
        self.next = Some(placed.saturating_add(duration));
        duration as u32
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_steady_source_is_placed_on_its_own_times() {
        let mut t = Timing::new(90_000);
        assert_eq!(t.place(9000, None), (9000, 3600), "1/25 s before a step is known");
        assert_eq!(t.place(12_600, None), (12_600, 3600));
        assert_eq!(t.place(16_200, None), (16_200, 3600));
    }

    #[test]
    fn a_late_frame_is_placed_contiguous_and_its_lag_absorbed_at_once() {
        let mut t = Timing::new(90_000);
        t.place(0, None);
        t.place(3600, None); // ends at 7200
        // Two frames lost: the next comes at 14400.
        let (placed, duration) = t.place(14_400, None);
        assert_eq!(placed, 7200, "no gap");
        // It lasts to where the following one is expected: the long step is not the estimate.
        assert_eq!(duration, 14_400 + 3600 - 7200);
        assert_eq!(t.place(18_000, None), (18_000, 3600), "back on its own time");
        // The step is held to 1 s.
        let mut t = Timing::new(90_000);
        t.place(0, None);
        t.place(900_000, None);
        assert_eq!(t.step, 90_000);
    }

    #[test]
    fn a_lower_frame_rate_is_followed_after_eight_frames() {
        let mut t = Timing::new(90_000);
        let mut dts = 0;
        for _ in 0..10 {
            t.place(dts, None);
            dts += 3600;
        }
        for _ in 0..STEPS {
            assert_eq!(t.step, 3600);
            t.place(dts, None);
            dts += 9000;
        }
        t.place(dts, None);
        assert_eq!(t.step, 9000);
    }

    #[test]
    fn an_early_frame_never_moves_back_and_lasts_at_least_a_tick() {
        let mut t = Timing::new(8000);
        let (_, d) = t.place(0, Some(160));
        assert_eq!(d, 160);
        // A source time that went back: placed after the last, one tick long.
        assert_eq!(t.place(-1000, Some(160)), (160, 1));
        assert_eq!(t.place(320, Some(160)), (161, 319));
    }

    #[test]
    fn a_held_sample_is_stretched_to_reach_the_next() {
        let mut t = Timing::new(90_000);
        let (placed, _) = t.place(0, None);
        assert_eq!(t.reach(placed, 5000), 5000);
        assert_eq!(t.place(5000, None), (5000, 5000));
        // Reaching back never gives zero.
        assert_eq!(t.reach(5000, 4000), 1);
    }

    #[test]
    fn extreme_times_saturate() {
        let mut t = Timing::new(0);
        t.place(i64::MAX - 1, None);
        let (placed, d) = t.place(i64::MAX, None);
        assert_eq!((placed, d), (i64::MAX, 1));
        let mut t = Timing::new(90_000);
        t.place(0, None);
        let (_, d) = t.place(i64::MAX / 2, None);
        assert_eq!(d, u32::MAX);
    }
}
