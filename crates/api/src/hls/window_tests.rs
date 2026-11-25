use std::time::{Duration, SystemTime};

use bytes::Bytes;

use super::*;

fn ms(n: u64) -> Duration {
    Duration::from_millis(n)
}

/// 25 pictures a second, a keyframe every `gop` pictures, for `seconds`.
fn pictures(window: &mut Window, gop: u64, seconds: u64) {
    for n in 0..seconds * 25 {
        let at = ms(n * 40);
        let key = n % gop == 0;
        window.push(vec![Bytes::from(vec![n as u8; 10])], true, key, at, None, SystemTime::UNIX_EPOCH + at);
    }
}

#[test]
fn segments_start_at_keyframes_once_the_target_is_reached() {
    let mut window = Window::new(Settings::default());
    window.new_init(true, None);
    // A keyframe every 0.6 s: segments of 1.2 s (the first keyframe at or past 1 s).
    pictures(&mut window, 15, 5);
    let segments: Vec<&Segment> = window.segments().collect();
    assert!(segments.len() >= 3);
    for (i, s) in segments.iter().enumerate() {
        assert_eq!(s.msn, i as u64, "numbers rise by one");
        assert_eq!(s.duration, ms(1200));
        assert_eq!(s.start, ms(1200 * i as u64));
        assert!(s.independent && s.parts[0].independent);
        assert_eq!(s.wall, SystemTime::UNIX_EPOCH + s.start, "the wall clock of its first frame");
        let parts: Duration = s.parts.iter().map(|p| p.duration).sum();
        assert_eq!(parts, s.duration, "its parts add up to it");
        assert!(s.parts.iter().all(|p| p.duration <= PART_TARGET), "{:?}", s.parts);
    }
    // Parts: 80 ms each, cut again at the keyframe at 600 ms (an independent part).
    let first: Vec<(Duration, bool)> = segments[0].parts.iter().map(|p| (p.duration, p.independent)).collect();
    assert_eq!(first[..4], [(ms(80), true), (ms(80), false), (ms(80), false), (ms(80), false)]);
    assert_eq!(first[7], (ms(40), false));
    assert_eq!(first[8], (ms(80), true));
    assert_eq!(window.longest(), ms(1200));
}

#[test]
fn a_long_group_of_pictures_makes_long_segments() {
    let mut window = Window::new(Settings::default());
    window.new_init(true, None);
    // The owner's cameras: a keyframe every 6.4 s (160 pictures).
    pictures(&mut window, 160, 20);
    let durations: Vec<Duration> = window.segments().map(|s| s.duration).collect();
    assert_eq!(durations, [ms(6400), ms(6400), ms(6400)]);
    assert_eq!(window.longest(), ms(6400));
    let open = window.open().unwrap();
    assert_eq!((open.msn, open.start), (3, ms(19_200)));
    assert!(!open.parts.is_empty(), "the open segment's parts are listed before it is complete");
    assert_eq!(window.next_part(), Some((3, open.parts.len())));
    assert_eq!(window.last_part(), Some((3, open.parts.len() - 1)));
}

#[test]
fn sound_alone_is_cut_on_time() {
    let mut window = Window::new(Settings::default());
    window.new_init(false, None);
    // AAC at 48 kHz: 21.33 ms frames, each one a frame a decoder starts at.
    for n in 0..300u64 {
        let at = Duration::from_micros(n * 21_333);
        window.push(vec![Bytes::from_static(&[1; 4])], true, true, at, None, SystemTime::UNIX_EPOCH);
    }
    let segments: Vec<&Segment> = window.segments().collect();
    assert!(segments.len() >= 5);
    for s in &segments {
        assert!(s.duration >= SEGMENT_TARGET && s.duration < SEGMENT_TARGET + ms(22), "{:?}", s.duration);
        let (last, rest) = s.parts.split_last().unwrap();
        assert!(rest.iter().all(|p| p.independent && p.duration >= ms(75) && p.duration <= ms(86)), "{:?}", s.parts);
        assert!(last.duration <= ms(86));
    }
}

#[test]
fn the_window_is_bounded_by_count_and_bytes_never_below_two() {
    let mut window = Window::new(Settings::default());
    window.new_init(true, None);
    pictures(&mut window, 25, 12);
    window.evict(usize::MAX);
    assert_eq!(window.segments().count(), MAX_SEGMENTS);
    assert_eq!(window.first(), Some(4), "the oldest gone, the numbers kept");
    window.evict(0);
    assert_eq!(window.segments().count(), MIN_SEGMENTS);
    let held: usize = window.segments().chain(window.open()).flat_map(|s| &s.parts).map(|p| p.bytes.len()).sum();
    assert!(window.size() >= held);
    assert!(window.segment(3).is_none() && window.segment(9).is_some());
}

#[test]
fn what_is_complete_and_what_is_not() {
    let mut window = Window::new(Settings::default());
    window.new_init(true, None);
    assert!(!window.has(0, None) && !window.has(0, Some(0)));
    pictures(&mut window, 25, 2);
    // Segment 0 (0 to 1 s) complete, segment 1 open with its first parts.
    assert!(window.has(0, None) && window.has(0, Some(5)));
    assert!(window.has(1, Some(0)) && !window.has(1, None) && !window.has(2, Some(0)));
    assert_eq!(window.part(1, 0).unwrap().len(), 20, "two pictures of 10 bytes");
    let segment: usize = window.segment(0).unwrap().iter().map(Bytes::len).sum();
    assert_eq!(segment, 250);
    assert!(window.part(1, 99).is_none());
}

#[test]
fn a_new_track_list_closes_the_segment_and_marks_a_discontinuity() {
    let mut window = Window::new(Settings::default());
    window.new_init(true, None);
    pictures(&mut window, 25, 2);
    window.new_init(true, None);
    assert_eq!(window.init(), 2);
    let at = ms(2000);
    window.push(vec![Bytes::from_static(b"key")], true, true, at, None, SystemTime::UNIX_EPOCH);
    let open = window.open().unwrap();
    assert!(open.discontinuity && open.init == 2 && open.msn == 2);
    assert!(window.segments().all(|s| !s.discontinuity && s.init == 1));
}

#[test]
fn a_segment_with_no_keyframe_is_closed_at_its_bound() {
    let mut window = Window::new(Settings::default());
    window.new_init(true, None);
    for n in 0..1000u64 {
        let at = ms(n * 40);
        window.push(vec![Bytes::from_static(&[0; 8])], true, n == 0, at, None, SystemTime::UNIX_EPOCH);
    }
    let first = window.segments().next().unwrap();
    assert_eq!(first.duration, MAX_SEGMENT_DURATION);
    assert!(first.independent);
    assert!(!window.open().unwrap().independent, "the next one does not start at a keyframe");
}

#[test]
fn only_the_lead_track_cuts() {
    let mut window = Window::new(Settings::default());
    window.new_init(true, None);
    // Sound before the first picture has nowhere to go.
    window.push(vec![Bytes::from_static(b"early")], false, true, ms(0), None, SystemTime::UNIX_EPOCH);
    assert_eq!(window.size(), 0);
    for n in 0..100u64 {
        let at = ms(n * 40);
        window.push(vec![Bytes::from_static(b"pic")], true, n % 25 == 0, at, None, SystemTime::UNIX_EPOCH);
        // Sound 30 ms ahead of the picture, every frame a "keyframe".
        window.push(vec![Bytes::from_static(b"snd")], false, true, at + ms(30), None, SystemTime::UNIX_EPOCH);
    }
    let durations: Vec<Duration> = window.segments().map(|s| s.duration).collect();
    assert_eq!(durations, [ms(1000), ms(1000), ms(1000)], "cut at the pictures' keyframes, the sound inside");
    assert_eq!(window.segment(0).unwrap().iter().map(Bytes::len).sum::<usize>(), 25 * 6);
}

#[test]
fn the_init_segments_of_the_window_are_kept_and_its_discontinuities_counted() {
    let mut window = Window::new(Settings::default());
    window.new_init(true, Some(Bytes::from_static(b"init one")));
    pictures(&mut window, 25, 3);
    window.new_init(true, Some(Bytes::from_static(b"init two")));
    // A track list that never made a segment, then another.
    window.new_init(true, Some(Bytes::from_static(b"init three")));
    assert_eq!(window.init_segment(1).as_deref(), Some(&b"init one"[..]), "segments of the first are listed");
    assert!(window.init_segment(2).is_none(), "no segment is read with the second");
    assert_eq!(window.init_segment(3).as_deref(), Some(&b"init three"[..]), "the current one");
    for n in 0..25 * 4 {
        let at = ms(3000 + n * 40);
        window.push(vec![Bytes::from(vec![1; 10])], true, n % 25 == 0, at, None, SystemTime::UNIX_EPOCH + at);
        window.evict(usize::MAX);
    }
    assert_eq!(window.discontinuities(), 0, "the first segment after it is still listed");
    let before = window.size();
    window.evict(0);
    assert_eq!(window.discontinuities(), 1, "it left the window");
    assert!(window.segments().all(|s| s.init == 3));
    assert!(window.init_segment(1).is_none(), "nothing is read with the first any more");
    assert!(window.size() < before);
    let parts: usize = window.segments().chain(window.open()).flat_map(|s| &s.parts).map(|p| p.bytes.len()).sum();
    // The init segment held, the complete parts and the picture of the part being written.
    assert_eq!(window.size(), b"init three".len() + parts + 10);
}
#[test]
fn each_track_of_a_segment_reads_alone() {
    let mut window = Window::new(Settings::default());
    window.new_init(true, None);
    // A picture (lead) and a sound frame after it, 2.5 s, a keyframe a second.
    for n in 0..63u64 {
        let at = ms(n * 40);
        let wall = SystemTime::UNIX_EPOCH + at;
        window.push(vec![Bytes::from(vec![b'v'; 3])], true, n % 25 == 0, at, None, wall);
        window.push(vec![Bytes::from(vec![b'a'; 2])], false, false, at + ms(5), None, wall);
    }
    let first = window.segments().next().unwrap().msn;
    let flat = |chunks: Vec<Bytes>| chunks.iter().flat_map(|c| c.to_vec()).collect::<Vec<u8>>();
    let whole = flat(window.segment(first).unwrap());
    let picture = flat(window.segment_of_track(first, true).unwrap());
    let sound = flat(window.segment_of_track(first, false).unwrap());
    assert_eq!((picture.len(), sound.len()), (25 * 3, 25 * 2), "a second of each");
    assert!(picture.iter().all(|b| *b == b'v') && sound.iter().all(|b| *b == b'a'));
    assert_eq!(picture.len() + sound.len(), whole.len(), "together, the segment");
    assert!(window.segment_of_track(first + 9, true).is_none(), "none past the window");
}
/// 25 pictures a second from `from` for `count` pictures, a keyframe every
/// 25 counted from `first`.
fn pictures_at(window: &mut Window, from: u64, count: u64, first: u64) {
    for i in 0..count {
        let n = first + i;
        let at = ms(from + i * 40);
        window.push(vec![Bytes::from(vec![1; 10])], true, n.is_multiple_of(25), at, None, SystemTime::UNIX_EPOCH + at);
    }
}

fn every_part(window: &Window) -> Vec<Duration> {
    window.segments().chain(window.open()).flat_map(|s| s.parts.iter().map(|p| p.duration)).collect()
}

#[test]
fn a_stalled_source_keeps_its_part_target_and_no_part_outlasts_it() {
    for sealed in [true, false] {
        let mut window = Window::new(Settings::default());
        window.new_init(true, None);
        pictures_at(&mut window, 0, 51, 0); // to 2000 ms
        assert_eq!(window.part_target(), PART_TARGET);
        // The source stalls 5 s, its clock going on; the session closes the
        // part being written at its target on the wall clock (or, its timer
        // late, the frame after the gap comes first).
        if sealed {
            assert!(window.seal());
            assert!(!window.seal(), "nothing left to close");
        }
        pictures_at(&mut window, 7040, 100, 51);
        assert_eq!(window.part_target(), PART_TARGET, "sealed {sealed}: PART-TARGET stays put");
        let parts = every_part(&window);
        assert!(parts.iter().all(|d| *d <= PART_TARGET), "sealed {sealed}: {parts:?}");
        // The gap is in no part, and a segment lasts its parts.
        for s in window.segments() {
            let sum: Duration = s.parts.iter().map(|p| p.duration).sum();
            assert_eq!(s.duration, sum, "sealed {sealed}");
        }
        assert!(window.longest() < Duration::from_millis(1500), "sealed {sealed}: {:?}", window.longest());
        let text = super::super::playlist::View {
            id: "s",
            kind: super::super::playlist::Kind::Fmp4,
            window: &window,
            low_latency: true,
        }
        .media();
        assert!(text.contains("PART-HOLD-BACK=0.200,") && text.contains("PART-TARGET=0.100"), "{text}");
    }
}

#[test]
fn a_part_after_a_close_at_its_target_starts_where_that_one_ended() {
    let mut window = Window::new(Settings::default());
    window.new_init(true, None);
    pictures_at(&mut window, 0, 30, 0); // to 1160 ms; parts ...1120..1160 open
    assert!(window.seal());
    // Sound a little behind the picture arrives after: it must not reach back.
    window.push(vec![Bytes::from_static(b"snd")], false, true, ms(1150), None, SystemTime::UNIX_EPOCH);
    let last = window.open().unwrap().parts.last().cloned().unwrap();
    pictures_at(&mut window, 1200, 2, 30);
    let parts = &window.open().unwrap().parts;
    let ends: Vec<Duration> = parts.iter().map(|p| p.duration).collect();
    assert!(ends.iter().all(|d| *d <= PART_TARGET), "{ends:?} after {last:?}");
}

#[test]
fn a_slow_camera_has_parts_of_one_picture() {
    let mut window = Window::new(Settings::default());
    window.new_init(true, None);
    // 5 pictures a second, a keyframe every 5.
    for n in 0..30u64 {
        let at = ms(n * 200);
        window.push(vec![Bytes::from(vec![1; 10])], true, n % 5 == 0, at, None, SystemTime::UNIX_EPOCH + at);
    }
    assert_eq!(window.part_target(), ms(200), "the target is one picture");
    let parts = every_part(&window);
    assert!(parts.iter().all(|d| *d == ms(200)), "{parts:?}");
}

#[test]
fn the_container_s_own_ends_close_the_parts() {
    // fMP4 says where each sample ends: a lost picture filled by the
    // container (the next sample placed at the previous end) is no gap.
    let mut window = Window::new(Settings::default());
    window.new_init(true, None);
    let mut placed = 0;
    for n in 0..60u64 {
        if n == 30 {
            continue; // lost: the container stretched the one before over it
        }
        let at = ms(placed);
        let end = ms(if n == 29 { placed + 80 } else { placed + 40 });
        window.push(vec![Bytes::from(vec![1; 10])], true, n % 25 == 0, at, Some(end), SystemTime::UNIX_EPOCH + at);
        placed = end.as_millis() as u64;
    }
    assert!(window.seal(), "the part being written closed where its samples end");
    let total: Duration =
        window.segments().chain(window.open()).map(|s| s.parts.iter().map(|p| p.duration).sum::<Duration>()).sum();
    assert_eq!(total, ms(placed), "every part as long as its samples, contiguous");
    assert!(every_part(&window).iter().all(|d| *d <= PART_TARGET));
}
