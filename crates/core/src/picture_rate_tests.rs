//! Tests of the pace and the grid of a source's pictures.

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
    // Its dates are near their slots, not on them: believed from a full
    // window of gaps.
    assert_eq!(esp32.grid(), None, "twelve gaps");
    let esp32 = stepped(&[ESP32, ESP32].concat());
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
    assert!(near(dated(&[40, 41, 40, 40, 41, 40, 40, 41, 40, 40, 41, 40, 40, 41, 40]).grid(), 40.3));
}

#[test]
fn a_grid_finer_than_the_pace_needs_more_than_a_few_gaps() {
    // Three gaps of a camera on a 40 ms grid: 80 and 120 ms share a step,
    // and so would three gaps of anything by chance.
    assert!(!near(dated(&[120, 120, 80]).grid(), 40.0));
    assert_eq!(dated(&[120, 120, 80]).interval(), Some(Duration::from_millis(120)));
    assert_eq!(dated(&[120, 120, 80, 120, 160, 120, 80, 120]).grid(), Some(Duration::from_millis(40)));
    // Three even gaps are a grid at once: it is the pace.
    assert_eq!(dated(&[70, 70, 70]).grid(), Some(Duration::from_millis(70)));
}

/// A thirtieth of a second, as [`PictureRate::grid`] tells a whole rate.
fn thirtieth() -> Option<Duration> {
    Some(Duration::from_secs_f64(1.0 / 30.0))
}

#[test]
fn a_webcam_whose_pictures_come_evenly_is_read_at_its_whole_rate() {
    // The owner's 30-a-second webcam, dated as it arrives, through his
    // relay on the evening of 2026-11-19 (ticks): 24 to 40.5 ms between
    // two pictures, 0.08 of a slot off on average. FFmpeg's guess read
    // `30/1` from these dates, and its transcode then carried 7.4 Mbit/s
    // where one told no rate carried 2.9. The step measured is 33.31 ms
    // (30.02 a second); the dates fit 30 as well, and 30 is said.
    let even = [2943, 2934, 3366, 2619, 3384, 2961, 2556, 3096, 2925, 3177, 2817, 3222, 3645, 2160, 3159];
    assert_eq!(stepped(&even).grid(), thirtieth());
    // The ring of gaps is read oldest first wherever it wrapped.
    let mut twice = even.to_vec();
    twice.extend_from_slice(&even[..7]);
    assert_eq!(stepped(&twice).grid(), thirtieth());
}

#[test]
fn a_webcam_that_catches_up_now_and_then_is_read_at_its_rate_not_its_median_gap() {
    // The same webcam read directly on the morning of 2026-11-20 (ticks):
    // most pictures 33 to 36 ms apart, one in five 26 to 28 ms after the
    // last. The median gap is 34.5 ms, fifteen gaps span 14.5 of it, and
    // slots counted from the span and the median made a grid of 35.7 ms that
    // the dates do not sit on: no rate one start in ten, where FFmpeg's guess
    // read 30 from the same dates.
    for dates in [
        [2970, 3121, 3232, 2364, 3157, 2912, 2934, 3229, 3149, 3110, 2502, 3016, 3107, 3024, 3206],
        [3232, 2364, 3157, 2912, 2934, 3229, 3149, 3110, 2502, 3016, 3107, 3024, 3206, 3143, 2686],
        [3149, 3110, 2502, 3016, 3107, 3024, 3206, 3143, 2686, 2892, 3125, 2810, 3254, 3084, 2553],
    ] {
        assert_eq!(stepped(&dates).grid(), thirtieth(), "{dates:?}");
    }
}

#[test]
fn dates_near_their_slots_are_a_grid_from_a_full_window_only() {
    // The same webcam's dates, a few at a time (ticks): what a transcode
    // that starts with the camera - nothing holding it - is answered from.
    // Three and eight gaps fit some step nearly every time, and read 29, 31,
    // 32 and 28 pictures a second for these; a constant-rate encode told 31
    // repeats a picture every second for as long as it runs.
    for few in [
        &[3239, 3022, 2970][..],
        &[3121, 3232, 2364],
        &[2364, 3157, 2912],
        &[3467, 3271, 3017],
        &[3143, 2686, 2892, 3125, 2810, 3254, 3084, 2553],
        &[3016, 3389, 2700, 3162, 2489, 3467, 3271, 3017],
    ] {
        let webcam = stepped(few);
        assert!(webcam.interval().is_some());
        assert_eq!(webcam.grid(), None, "{few:?}");
    }
    // Every prefix of an even stretch says nothing until the window is full.
    let even = [2943, 2934, 3366, 2619, 3384, 2961, 2556, 3096, 2925, 3177, 2817, 3222, 3645, 2160, 3159];
    for gaps in 3..even.len() {
        assert_eq!(stepped(&even[..gaps]).grid(), None, "{gaps} gaps");
    }
    assert_eq!(stepped(&even).grid(), thirtieth());
    // A camera's own clock is on its slots: three gaps are its grid.
    assert_eq!(stepped(&[3000; 3]).grid(), thirtieth());
}

#[test]
fn a_camera_whose_own_clock_is_no_whole_rate_is_told_as_it_is() {
    // 100 pictures in 7 seconds (the owner's H.265 cameras) and 30000 in
    // 1001: the dates fit their own step exactly, and 14 or 30 a second only
    // loosely. Said as a whole rate, a constant-rate encode would drop or
    // repeat a picture every few seconds.
    for ticks in [6300_i64, 3003] {
        let own = Duration::from_secs_f64(ticks as f64 / f64::from(CLOCK));
        assert_eq!(stepped(&[ticks; 15]).grid(), Some(own), "{ticks}");
    }
    // A webcam of 12.5 a second dated as it arrives is no 12 and no 13.
    let webcam = stepped(&[7150, 7260, 7190, 7230, 7120, 7290, 7200, 7170, 7240, 7180, 7210, 7160, 7250, 7200, 7150]);
    assert!(near(webcam.grid(), 80.0), "{:?}", webcam.grid());
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
    let _ = hostile.grid();
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
