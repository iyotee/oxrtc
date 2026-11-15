use proptest::prelude::*;

use super::*;
use crate::media::Codec;

const SEC: i64 = 90_000;

fn video() -> TrackInfo {
    TrackInfo::video(Codec::H264)
}

fn sound(rate: u32) -> TrackInfo {
    TrackInfo::audio(Codec::Pcma, rate, 1)
}

fn ms(epoch: Instant, ms: u64) -> Instant {
    epoch + Duration::from_millis(ms)
}

fn timelines(tracks: &[TrackInfo]) -> Timelines {
    let mut t = Timelines::default();
    t.set_tracks(tracks);
    t
}

#[test]
fn a_run_is_anchored_at_its_first_frames_arrival_and_keeps_its_steps() {
    let epoch = Instant::now();
    let mut t = timelines(&[video()]);
    assert_eq!(t.map(0, 123_456, ms(epoch, 2000), epoch), 2 * SEC);
    // A camera that delivers in bursts: its own regular timing is kept.
    assert_eq!(t.map(0, 123_456 + 3600, ms(epoch, 2600), epoch), 2 * SEC + 3600);
    assert_eq!(t.map(0, 123_456 + 7200, ms(epoch, 2601), epoch), 2 * SEC + 7200);
    // A decoder's reordering too.
    assert_eq!(t.map(0, 123_456 + 3600, ms(epoch, 2602), epoch), 2 * SEC + 3600);
}

#[test]
fn a_restart_continues_at_the_wall_clock_not_at_the_sources_zero() {
    let epoch = Instant::now();
    let mut t = timelines(&[video(), sound(8000)]);
    for n in 0..10 {
        t.map(0, n * 3600, ms(epoch, 1000 + 40 * n as u64), epoch);
        t.map(1, n * 320, ms(epoch, 1000 + 40 * n as u64), epoch);
    }
    t.restart();
    // Reopened 3 s after the epoch, pts from 0 again: both tracks land at 3 s.
    assert_eq!(t.map(0, 0, ms(epoch, 3000), epoch), 3 * SEC);
    assert_eq!(t.map(1, 0, ms(epoch, 3000), epoch), 3 * 8000);
    assert_eq!(t.map(1, 320, ms(epoch, 3040), epoch), 3 * 8000 + 320);
}

#[test]
fn the_timeline_never_goes_back_even_after_a_run_ahead_of_the_wall_clock() {
    let epoch = Instant::now();
    let mut t = timelines(&[video()]);
    // A transcoder working through a cached GOP: 1 s of media every 100 ms.
    let mut last = 0;
    for n in 0..7 {
        last = t.map(0, n * SEC, ms(epoch, 100 * n as u64), epoch);
    }
    assert_eq!(last, 6 * SEC, "running ahead is not a jump");
    t.restart();
    // Back at 1 s of wall clock, behind 6 s: placed just after it.
    assert_eq!(t.map(0, 0, ms(epoch, 1000), epoch), 6 * SEC + SEC / 25);
}

#[test]
fn a_source_whose_clock_runs_fast_is_held_within_its_lead_of_the_wall_clock() {
    let epoch = Instant::now();
    let mut t = timelines(&[sound(8000)]);
    // Every 40 ms, a frame 1 s on: each step under the per-frame bound,
    // 99 s of time in 4 s all the same.
    let mut times = Vec::new();
    for n in 0..100 {
        times.push(t.map(0, n * 8000, ms(epoch, 1000 + 40 * n as u64), epoch));
    }
    let last_arrival = 8 * 4960;
    assert_eq!(*times.last().unwrap(), last_arrival + MAX_LEAD_SECS * 8000, "held at the lead");
    assert!(times.windows(2).all(|w| w[1] > w[0]), "and never back");
}

#[test]
fn transcodes_restarted_on_the_cached_gop_do_not_add_up_their_leads() {
    let epoch = Instant::now();
    let mut t = timelines(&[sound(8000)]);
    let (mut at, mut lead) = (1000u64, 0i64);
    for _ in 0..10 {
        // A transcode works through a 6 s cached GOP in 0.6 s, runs 2 s in
        // real time, exits, and is restarted 0.5 s later under the reader.
        t.restart();
        let mut pts = 0;
        for n in 0..65 {
            pts += if n < 15 { 3200 } else { 320 };
            let out = t.map(0, pts, ms(epoch, at), epoch);
            lead = out - 8 * at as i64;
            at += 40;
        }
        at += 500;
    }
    assert!(lead <= MAX_LEAD_SECS * 8000, "the sound ran {} s ahead of its arrival", lead / 8000);
}

#[test]
fn a_jump_back_inside_a_run_is_a_restart() {
    let epoch = Instant::now();
    let mut t = timelines(&[video()]);
    t.map(0, 1_000_000, ms(epoch, 1000), epoch);
    assert_eq!(t.map(0, 1_000_000 + 3600, ms(epoch, 1040), epoch), SEC + 3600);
    // More than a second back, 5 s later: anchored again at the wall clock.
    assert_eq!(t.map(0, 5, ms(epoch, 6000), epoch), 6 * SEC);
    assert_eq!(t.map(0, 3605, ms(epoch, 6040), epoch), 6 * SEC + 3600);
}

#[test]
fn a_jump_forward_inside_a_run_is_anchored_again() {
    let epoch = Instant::now();
    let mut t = timelines(&[sound(8000)]);
    t.map(0, 0, ms(epoch, 1000), epoch);
    let before = t.map(0, 320, ms(epoch, 1040), epoch);
    // 9.4 hours ahead in one 40 ms step: the sound openIVR found.
    let hours = (9.4 * 3600.0 * 8000.0) as i64;
    let after = t.map(0, hours, ms(epoch, 1080), epoch);
    assert_eq!(after - before, 320, "placed at the wall clock, 40 ms on");
    assert_eq!(t.map(0, hours + 320, ms(epoch, 1120), epoch), after + 320);
}

#[test]
fn a_new_track_list_continues_each_kind_in_its_new_clock_rate() {
    let epoch = Instant::now();
    let mut t = timelines(&[video(), sound(8000)]);
    t.map(0, 0, ms(epoch, 1000), epoch);
    t.map(1, 0, ms(epoch, 1000), epoch);
    t.map(1, 8000 * 5, ms(epoch, 1000), epoch);
    // The same list again inside the run (parameter sets found in band).
    t.set_tracks(&[video(), sound(8000)]);
    assert_eq!(t.map(0, 3600, ms(epoch, 1040), epoch), SEC + 3600, "nothing moved");
    // Reordered, the sound now at 16 kHz: it continues after its 6 s.
    t.set_tracks(&[sound(16_000), video()]);
    assert_eq!(t.map(0, 0, ms(epoch, 1100), epoch), 6 * 16_000 + 16_000 / 25);
    assert_eq!(t.map(1, 7200, ms(epoch, 1080), epoch), SEC + 7200, "the picture kept its anchor");
}

#[test]
fn the_time_is_never_negative() {
    let epoch = Instant::now();
    let mut t = timelines(&[video()]);
    assert_eq!(t.map(0, 3600, epoch, epoch), 0);
    assert_eq!(t.map(0, 0, epoch, epoch), 0, "reordered before the anchor");
    assert_eq!(t.map(7, 42, epoch, epoch), 42, "an unknown track passes through");
}

#[test]
fn ticks_and_rescaling() {
    assert_eq!(ticks(Duration::from_millis(1500), 90_000), 135_000);
    assert_eq!(ticks(Duration::MAX, u32::MAX), i64::MAX);
    assert_eq!(rescale(1, 8000, 16_000), 2);
    assert_eq!(rescale(3, 16_000, 8000), 2, "rounded up");
    assert_eq!(rescale(i64::MAX, 1, u32::MAX), i64::MAX);
}

proptest! {
    /// Whatever the source does - restarts, jumps, bursts - a track's time
    /// after a restart is after every time it was given before.
    #[test]
    fn a_restart_never_goes_back(
        steps in proptest::collection::vec((-200_000i64..200_000, 0u64..2000, any::<bool>()), 1..200),
    ) {
        let epoch = Instant::now();
        let mut t = timelines(&[video()]);
        let (mut pts, mut at, mut high) = (0i64, 0u64, None::<i64>);
        for (step, wait, restart) in steps {
            if restart {
                t.restart();
            }
            pts = pts.saturating_add(step);
            at += wait;
            let out = t.map(0, pts, ms(epoch, at), epoch);
            prop_assert!(out >= 0);
            // Ahead of its arrival by the lead at most (plus the restarts'
            // steps and one tick a frame while the arrivals stand still).
            let ceiling = 90 * at as i64 + MAX_LEAD_SECS * SEC + 200 * (SEC / 25 + 1);
            prop_assert!(out <= ceiling, "{out} ahead of {ceiling}");
            if restart && let Some(high) = high {
                prop_assert!(out > high, "{out} after {high}");
            }
            high = Some(high.map_or(out, |h| h.max(out)));
        }
    }
}
