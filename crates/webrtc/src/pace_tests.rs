//! The sound's clock on its own: pairs, jitter, a stall, the bounds.
#![allow(clippy::unwrap_used, clippy::indexing_slicing)]

use bytes::Bytes;
use proptest::prelude::*;

use super::*;

const RATE: u32 = 48_000;
/// One frame of 20 ms.
const STEP: i64 = 960;

fn ms(n: u64) -> Duration {
    Duration::from_millis(n)
}

fn due(placed: Placed) -> Instant {
    match placed {
        Placed::Due(at) => at,
        Placed::Late(by) => panic!("late by {by:?}"),
    }
}

/// The sound as FFmpeg and most cameras deliver it: `pairs` times two frames
/// of 20 ms at once, every 40 ms. Returns when each frame is due, counted
/// from the first pair's arrival.
fn pairs(clock: &mut SoundClock, origin: Instant, pairs: u64) -> Vec<Duration> {
    let mut out = Vec::new();
    for pair in 0..pairs {
        let at = origin + ms(1000 + 40 * pair);
        for half in 0..2 {
            let pts = STEP * i64::try_from(2 * pair + half).unwrap();
            out.push(due(clock.place(pts, at)).duration_since(origin + ms(1000)));
        }
    }
    out
}

#[test]
fn pairs_leave_one_frame_apart() {
    let origin = Instant::now();
    let mut clock = SoundClock::new(RATE, origin, None);
    let left = pairs(&mut clock, origin, 50);
    // The first frame leaves as it comes; every other one 20 ms after the
    // one before it, from the first pair on.
    for (n, at) in left.iter().enumerate() {
        assert_eq!(*at, ms(20 * u64::try_from(n).unwrap()), "frame {n}");
    }
    assert_eq!(clock.hold(), ms(20), "the pair's second frame is the one held");
}

#[test]
fn a_frame_is_never_due_before_it_arrived_on_time() {
    // A stream that arrives exactly as it was sampled is not held at all.
    let origin = Instant::now();
    let mut clock = SoundClock::new(RATE, origin, None);
    for n in 0..100u64 {
        let at = origin + ms(20 * n);
        assert_eq!(due(clock.place(STEP * i64::try_from(n).unwrap(), at)), at);
    }
    assert_eq!(clock.hold(), Duration::ZERO);
}

#[test]
fn a_late_pair_moves_the_hold_once_and_it_is_forgotten_half_a_minute_later() {
    let origin = Instant::now();
    let mut clock = SoundClock::new(RATE, origin, None);
    let mut last = origin;
    let mut steps = Vec::new();
    // Pair 10 arrives 12 ms late; every other one on time, for 40 s.
    for pair in 0..1000u64 {
        let late = if pair == 10 { 12 } else { 0 };
        let at = origin + ms(1000 + 40 * pair + late);
        for half in 0..2 {
            let at_due = due(clock.place(STEP * i64::try_from(2 * pair + half).unwrap(), at));
            if pair > 0 || half > 0 {
                steps.push(at_due.duration_since(last));
            }
            last = at_due;
        }
    }
    let odd: Vec<(usize, Duration)> = steps.iter().copied().enumerate().filter(|(_, s)| *s != ms(20)).collect();
    // One step of 32 ms when the late pair comes, one of 8 ms when the hold
    // has forgotten it: the 12 ms are not paid at every pair, and the hold
    // does not move in between (each of its steps is a lateness a browser's
    // audio buffer would follow).
    assert_eq!(odd.len(), 2, "{odd:?}");
    assert_eq!((odd[0].1, odd[1].1), (ms(32), ms(8)));
    let apart = ms(20) * u32::try_from(odd[1].0 - odd[0].0).unwrap();
    assert!(apart >= HOLD_WINDOW && apart <= HOLD_WINDOW + ms(2000), "forgotten {apart:?} later");
}

#[test]
fn the_hold_is_bounded_and_a_later_frame_leaves_at_once_dated_when_it_was_due() {
    let origin = Instant::now();
    let mut clock = SoundClock::new(RATE, origin, None);
    for n in 0..50u64 {
        clock.place(STEP * i64::try_from(n).unwrap(), origin + ms(20 * n));
    }
    // Frame 50 comes 300 ms late: the others are not held 300 ms for it.
    let late = due(clock.place(STEP * 50, origin + ms(1000 + 300)));
    assert_eq!(clock.hold(), MAX_HOLD);
    assert_eq!(late, origin + ms(1000) + MAX_HOLD, "dated on the line, in the past");
    // The next one, on time, is held the bound and no more.
    let next = due(clock.place(STEP * 51, origin + ms(1020 + 300)));
    assert_eq!(next, origin + ms(1020) + MAX_HOLD);
}

#[test]
fn the_sound_a_stall_kept_back_is_late_beyond_the_bound_and_what_is_fresh_is_not() {
    let origin = Instant::now();
    let mut clock = SoundClock::new(RATE, origin, Some(ms(200)));
    for n in 0..50u64 {
        assert!(matches!(clock.place(STEP * i64::try_from(n).unwrap(), origin + ms(20 * n)), Placed::Due(_)));
    }
    // The source stops for a second, then delivers the second at once.
    let burst = origin + ms(2000);
    let placed: Vec<Placed> = (50..100u64).map(|n| clock.place(STEP * i64::try_from(n).unwrap(), burst)).collect();
    let late = placed.iter().filter(|p| matches!(p, Placed::Late(_))).count();
    // Frame 50 was due at 1000 ms and comes at 2000: 1 s behind. Frame 99
    // was due at 1980 ms: 20 ms behind. The bound is 200 ms: frames 50 to
    // 89 are late (1000 ms down to 220 ms), frames 90 to 99 leave.
    assert_eq!(late, 40, "{placed:?}");
    assert!(placed[..40].iter().all(|p| matches!(p, Placed::Late(by) if *by > ms(200))));
    assert!(placed[40..].iter().all(|p| matches!(p, Placed::Due(_))));
    assert_eq!(placed[0], Placed::Late(ms(1000)));
}

#[test]
fn without_a_bound_nothing_is_late() {
    let origin = Instant::now();
    let mut clock = SoundClock::new(RATE, origin, None);
    clock.place(0, origin);
    assert!(matches!(clock.place(STEP, origin + ms(5000)), Placed::Due(_)));
}

#[test]
fn a_source_that_slows_for_good_is_late_for_one_window_only() {
    // Every frame from the 50th on comes 500 ms later than before, for good
    // (a transcoder that got slower). The bound drops them while the window
    // still holds the earlier clock, and no longer.
    let origin = Instant::now();
    let mut clock = SoundClock::new(RATE, origin, Some(ms(200)));
    let mut late = Vec::new();
    for n in 0..400u64 {
        let shift = if n >= 50 { 500 } else { 0 };
        if matches!(clock.place(STEP * i64::try_from(n).unwrap(), origin + ms(20 * n + shift)), Placed::Late(_)) {
            late.push(n);
        }
    }
    assert_eq!(late.first(), Some(&50));
    let last = *late.last().unwrap();
    assert!(last <= 50 + 101, "late until frame {last}: more than the window");
}

#[test]
fn a_stall_longer_than_the_window_is_still_judged_by_the_clock_it_had() {
    let origin = Instant::now();
    let mut clock = SoundClock::new(RATE, origin, Some(ms(200)));
    for n in 0..100u64 {
        clock.place(STEP * i64::try_from(n).unwrap(), origin + ms(20 * n));
    }
    // Five seconds of nothing (more than the window), then the five seconds at once.
    let burst = origin + ms(2000 + 5000);
    let late = (100..350u64)
        .filter(|n| matches!(clock.place(STEP * i64::try_from(*n).unwrap(), burst), Placed::Late(_)))
        .count();
    assert_eq!(late, 240, "all but the last 200 ms of the burst");
}

#[test]
fn a_source_that_delivers_a_second_at_a_time_is_not_stalling() {
    // 50 frames at once, every second: each delivery's first frame is a
    // second behind the last one's clock. That is how this source is; with
    // a bound of 200 ms it would lose four fifths of its sound for good.
    let origin = Instant::now();
    let mut clock = SoundClock::new(RATE, origin, Some(ms(200)));
    let mut late_per_delivery = Vec::new();
    for delivery in 0..40u64 {
        let at = origin + ms(1000 * (delivery + 1));
        let late = (0..50u64)
            .filter(|n| matches!(clock.place(STEP * i64::try_from(50 * delivery + n).unwrap(), at), Placed::Late(_)))
            .count();
        late_per_delivery.push(late);
    }
    // The second delivery is judged a stall (the first gave the clock, and
    // nothing else is known of the source yet); once the source has done it
    // again, none is.
    assert_eq!(late_per_delivery[..2], [0, 39], "{late_per_delivery:?}");
    assert!(late_per_delivery[2..].iter().all(|late| *late == 0), "{late_per_delivery:?}");
}

#[test]
fn a_stall_now_and_then_is_a_stall_however_long_the_stream_ran() {
    // A second's stall every eight seconds (the owner's camera_1): the
    // usual second has none, so each burst is late beyond the bound.
    let origin = Instant::now();
    let mut clock = SoundClock::new(RATE, origin, Some(ms(200)));
    let mut late = 0;
    for n in 0..3000u64 {
        // Frames 350..400 of every 400 arrive together, at the 400th's time.
        let arrival = if n % 400 >= 350 { 20 * (n - n % 400 + 400) } else { 20 * n };
        if matches!(clock.place(STEP * i64::try_from(n).unwrap(), origin + ms(arrival)), Placed::Late(_)) {
            late += 1;
        }
    }
    // Seven stalls, 40 of each one's 50 frames later than 200 ms.
    assert_eq!(late, 7 * 40);
}

#[test]
fn a_camera_that_stalls_every_other_second_is_still_cut() {
    // The owner's camera_1 at noon on 2026-11-18: 45 % of its half-seconds
    // held a frame more than 100 ms late. Here the last 600 ms of every 2 s
    // arrive together: half of the seconds have a burst, half none. That is
    // a stalling camera, not a source that delivers a second at a time (every
    // one of whose seconds has its burst): each burst is cut beyond the bound.
    let origin = Instant::now();
    let mut clock = SoundClock::new(RATE, origin, Some(ms(200)));
    let mut late = 0;
    for n in 0..3000u64 {
        let arrival = if n % 100 >= 70 { 20 * (n - n % 100 + 100) } else { 20 * n };
        if matches!(clock.place(STEP * i64::try_from(n).unwrap(), origin + ms(arrival)), Placed::Late(_)) {
            late += 1;
        }
    }
    // Thirty bursts, 20 of each one's 30 frames later than 200 ms.
    assert_eq!(late, 30 * 20);
}

fn frame(pts: i64) -> Arc<Frame> {
    Arc::new(Frame::new(1, pts, false, Bytes::from_static(b"x")))
}

#[test]
fn held_frames_leave_in_order_when_their_time_comes() {
    let now = Instant::now();
    let mut held = Held::default();
    assert!(held.push(1, frame(0), now + ms(20), now).is_none());
    // The clock's line moved back: this one would be due before the first.
    assert!(held.push(1, frame(STEP), now + ms(10), now).is_none());
    assert_eq!(held.next_due(), Some(now + ms(20)));
    assert!(held.pop_due(now + ms(19)).is_none());
    let first = held.pop_due(now + ms(20)).unwrap();
    assert_eq!((first.frame.pts, first.timed), (0, true));
    let second = held.pop_due(now + ms(20)).unwrap();
    assert_eq!((second.frame.pts, second.due), (STEP, now + ms(20)), "never before the one ahead of it");
    assert!(held.pop_due(now + ms(1000)).is_none() && held.next_due().is_none());
    // A frame already past its time is not one the timer was asked for.
    held.push(1, frame(2 * STEP), now, now + ms(5));
    assert!(!held.pop_due(now + ms(5)).unwrap().timed);
}

#[test]
fn the_queue_is_bounded_and_the_oldest_gives_way() {
    let now = Instant::now();
    let mut held = Held::default();
    for n in 0..MAX_HELD {
        assert!(held.push(1, frame(STEP * i64::try_from(n).unwrap()), now + ms(40), now).is_none());
    }
    let dropped = held.push(1, frame(STEP * 1000), now + ms(40), now).unwrap();
    assert_eq!(dropped.frame.pts, 0);
    assert_eq!(held.len(), MAX_HELD);
    held.clear();
    assert_eq!(held.len(), 0);
}

proptest! {
    /// Whatever the frames' times and arrivals: nothing panics, a frame is
    /// never due before the earliest it could have left (its arrival, or the
    /// hold's bound earlier when it came late), never held longer than the
    /// bound past its arrival, and the window stays bounded.
    #[test]
    fn any_arrivals_are_placed_within_the_bounds(
        frames in proptest::collection::vec((any::<i64>(), 0u64..20_000_000, any::<bool>()), 1..400),
        rate in 0u32..200_000,
        bound in proptest::option::of(0u64..2_000),
    ) {
        let origin = Instant::now();
        let mut clock = SoundClock::new(rate, origin, bound.map(ms));
        let mut at = origin;
        for (pts, gap_us, back) in frames {
            // Arrivals mostly go forward; a clock handed an earlier instant
            // (two tracks, a relay) must cope too.
            at = if back { at.checked_sub(Duration::from_micros(gap_us)).unwrap_or(at).max(origin) }
                 else { at + Duration::from_micros(gap_us) };
            match clock.place(pts, at) {
                Placed::Due(due) => {
                    prop_assert!(due <= at + MAX_HOLD, "held more than the bound");
                    prop_assert!(clock.hold() <= MAX_HOLD);
                }
                Placed::Late(by) => prop_assert!(bound.is_some_and(|b| by > ms(b))),
            }
            prop_assert!(clock.arrivals.len() <= MAX_ARRIVALS && clock.peaks.len() <= MAX_PEAKS);
        }
    }

    /// A regular stream delivered in batches of any size is sent one frame
    /// apart once the first batch has passed, as long as the batch fits the
    /// hold.
    #[test]
    fn batches_within_the_hold_leave_evenly(batch in 1u64..=3, batches in 20u64..60, frame_ms in prop::sample::select(vec![10u64, 20])) {
        prop_assume!((batch - 1) * frame_ms <= 40);
        let origin = Instant::now();
        let step = i64::try_from(u64::from(RATE) * frame_ms / 1000).unwrap();
        let mut clock = SoundClock::new(RATE, origin, None);
        let mut last: Option<Instant> = None;
        for b in 0..batches {
            let at = origin + ms(500 + b * batch * frame_ms);
            for n in 0..batch {
                let pts = step * i64::try_from(b * batch + n).unwrap();
                let Placed::Due(due) = clock.place(pts, at) else { return Err(TestCaseError::fail("late")) };
                if let Some(before) = last {
                    prop_assert_eq!(due.duration_since(before), ms(frame_ms));
                }
                last = Some(due);
            }
        }
    }
}
