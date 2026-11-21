//! The writer read back by the independent reader (`reader.rs`).

use bytes::Bytes;
use oxrtc_core::{Codec, Frame, TrackInfo};

use super::*;
use crate::reader::{ReadFragment, read_fragment, read_init};
use crate::track::tests::{aac, h264, h265};

fn nal(kind: u8, len: usize) -> Bytes {
    let mut out = (len as u32 + 1).to_be_bytes().to_vec();
    out.push(kind);
    out.extend((0..len).map(|i| i as u8));
    Bytes::from(out)
}

fn video(pts: i64, key: bool) -> Frame {
    Frame::new(0, pts, key, nal(if key { 0x65 } else { 0x41 }, 50 + pts.rem_euclid(7) as usize))
}

fn sound(pts: i64) -> Frame {
    Frame::new(1, pts, true, Bytes::from(vec![0x21; 20 + pts.rem_euclid(5) as usize]))
}

fn writer(fragmenting: Fragmenting) -> Writer {
    Writer::new(&[h264(), aac()], Options { fragmenting, ..Options::default() }).unwrap()
}

fn read(w: &Writer, fragment: &Fragment) -> ReadFragment {
    let init = read_init(w.init_segment()).unwrap();
    read_fragment(&fragment.bytes, &init.trex).unwrap()
}

#[test]
fn the_init_segment_describes_every_track() {
    let w = Writer::new(&[h265(), TrackInfo::audio(Codec::Pcma, 8000, 1)], Options::default()).unwrap();
    let init = read_init(w.init_segment()).unwrap();
    assert_eq!(&init.major, b"iso5");
    assert_eq!(init.tracks.len(), 2);
    let (v, a) = (&init.tracks[0], &init.tracks[1]);
    assert_eq!((v.id, &v.handler, v.timescale, &v.entry), (1, b"vide", 90_000, b"hvc1"));
    assert_eq!((v.width, v.height), (1920, 1080));
    assert_eq!(&v.config[4..8], b"hvcC");
    assert_eq!((a.id, &a.handler, a.timescale, &a.entry), (2, b"soun", 8000, b"fLaC"));
    assert_eq!((a.channels, a.rate), (1, 8000));
    assert_eq!(&a.config[4..8], b"dfLa");
    assert_eq!(init.trex.iter().map(|t| t.0).collect::<Vec<_>>(), vec![1, 2]);
    assert_eq!(w.codecs(), &["hvc1.1.6.L120.90".to_owned(), "flac".to_owned()]);
    assert_eq!(w.mime_type(), "video/mp4; codecs=\"hvc1.1.6.L120.90,flac\"");
    let audio = Writer::new(&[TrackInfo::audio(Codec::Opus, 48_000, 2)], Options::default()).unwrap();
    assert_eq!(audio.mime_type(), "audio/mp4; codecs=\"opus\"");
}

#[test]
fn every_frame_is_its_own_fragment_and_decode_times_stay_contiguous() {
    let mut w = writer(Fragmenting::EveryFrame);
    let mut ends = [None::<u64>; 2];
    let mut sequence = 0;
    for i in 0..60i64 {
        let frames = [(0, video(90_000 + i * 3600, i % 25 == 0)), (1, sound(48_000 + i * 1024))];
        for (track, frame) in frames {
            let fragment = w.push(track, &frame).unwrap().expect("one fragment per frame");
            sequence += 1;
            assert_eq!(fragment.sequence, sequence);
            assert_eq!(fragment.samples, 1);
            assert_eq!(fragment.independent, track == 0 && frame.keyframe, "frame {i} of track {track}");
            let read = read(&w, &fragment);
            assert_eq!(read.sequence, sequence);
            let run = &read.runs[0];
            assert_eq!(run.track_id as usize, track + 1);
            let sample = &run.samples[0];
            assert_eq!(sample.data, frame.data.to_vec());
            if let Some(end) = ends[track] {
                assert_eq!(sample.decode_time, end, "no gap, no overlap");
            } else {
                assert_eq!(sample.decode_time, frame.pts as u64, "the first sample is on the core's timeline");
            }
            // A steady source is placed exactly on its own times.
            assert_eq!(sample.decode_time, frame.pts as u64);
            ends[track] = Some(sample.decode_time + u64::from(sample.duration));
            let key = track == 1 || frame.keyframe;
            assert_eq!(sample.flags, if key { 0x0200_0000 } else { 0x0101_0000 });
            assert_eq!(sample.composition_offset, 0);
            assert_eq!(fragment.span.track, track);
            assert_eq!(fragment.span.start, sample.decode_time);
        }
    }
    assert_eq!(w.pending_samples(), 0);
    assert_eq!(w.flush().unwrap(), None);
}

#[test]
fn a_group_of_pictures_is_one_fragment_with_exact_durations() {
    let mut w = writer(Fragmenting::GroupOfPictures);
    let mut fragments = Vec::new();
    // Video at an uneven rate (a camera's own steps), sound every 1024.
    let mut pts = 0i64;
    for i in 0..50 {
        if let Some(f) = w.push(0, &video(pts, i % 10 == 0)).unwrap() {
            fragments.push(f);
        }
        assert_eq!(w.push(1, &sound(pts * 48_000 / 90_000)).unwrap(), None, "sound never closes a group");
        pts += if i % 3 == 0 { 3000 } else { 4000 };
    }
    fragments.extend(w.flush().unwrap());
    assert_eq!(fragments.len(), 5);
    for (n, fragment) in fragments.iter().enumerate() {
        assert!(fragment.independent);
        let read = read(&w, fragment);
        assert_eq!(read.runs.len(), 2, "video, then sound");
        let video = &read.runs[0].samples;
        assert_eq!(video.len(), 10);
        assert_eq!(video[0].flags, 0x0200_0000);
        assert!(video[1..].iter().all(|s| s.flags == 0x0101_0000));
        // Each held sample lasts exactly to the next one's source time.
        for pair in video.windows(2) {
            assert_eq!(pair[0].decode_time + u64::from(pair[0].duration), pair[1].decode_time);
        }
        let first = (0..n * 10).map(|i| if i % 3 == 0 { 3000 } else { 4000 }).sum::<u64>();
        assert_eq!(video[0].decode_time, first);
        assert_eq!(fragment.span.start, first);
        let total: u64 = video.iter().map(|s| u64::from(s.duration)).sum();
        assert_eq!(fragment.span.duration, total);
        if n < 4 {
            let next = first + (n * 10..n * 10 + 10).map(|i| if i % 3 == 0 { 3000 } else { 4000 }).sum::<u64>();
            assert_eq!(
                video[9].decode_time + u64::from(video[9].duration),
                next,
                "the cut sample reaches the keyframe"
            );
        }
    }
}

#[test]
fn a_duration_cut_closes_parts_and_a_keyframe_always_starts_one() {
    let mut w = writer(Fragmenting::Duration(Duration::from_millis(200)));
    let mut fragments = Vec::new();
    for i in 0..50i64 {
        fragments.extend(w.push(0, &video(i * 3600, i % 25 == 0)).unwrap());
    }
    fragments.extend(w.flush().unwrap());
    // 25 frames of 40 ms per group: parts of 5 frames, the first of each group independent.
    assert_eq!(fragments.iter().map(|f| f.samples).collect::<Vec<_>>(), vec![5; 10]);
    let independent: Vec<bool> = fragments.iter().map(|f| f.independent).collect();
    assert_eq!(independent, (0..10).map(|n| n % 5 == 0).collect::<Vec<_>>());
    assert!(fragments.iter().all(|f| f.span.duration() == Duration::from_millis(200)));
    // A keyframe before the target still starts a new part.
    let mut w = writer(Fragmenting::Duration(Duration::from_secs(10)));
    assert_eq!(w.push(0, &video(0, true)).unwrap(), None);
    assert_eq!(w.push(0, &video(3600, false)).unwrap(), None);
    let cut = w.push(0, &video(7200, true)).unwrap().unwrap();
    assert_eq!(cut.samples, 2);
}

#[test]
fn an_audio_only_group_is_a_second() {
    let mut w = Writer::new(
        &[TrackInfo::audio(Codec::Pcma, 8000, 1)],
        Options { fragmenting: Fragmenting::GroupOfPictures, ..Options::default() },
    )
    .unwrap();
    let mut fragments = Vec::new();
    for i in 0..120 {
        fragments.extend(w.push(0, &Frame::new(0, i * 160, true, Bytes::from(vec![0xd5; 160]))).unwrap());
    }
    assert_eq!(fragments.len(), 2);
    assert!(fragments.iter().all(|f| f.samples == 50 && f.independent));
    assert_eq!(fragments[0].span.duration(), Duration::from_secs(1));
}

#[test]
fn a_decode_time_before_the_presentation_time_is_a_signed_offset() {
    let mut w = writer(Fragmenting::GroupOfPictures);
    // I P B B, in decode order: presentation 0, 3, 1, 2 frames after the start.
    let order = [(0i64, 0i64, true), (3, 1, false), (1, 2, false), (2, 3, false)];
    for (pts, dts, key) in order {
        let mut frame = video(pts * 3600, key);
        frame.pts = 7200 + pts * 3600;
        w.push_decoded(0, &frame, dts * 3600).unwrap();
    }
    let fragment = w.flush().unwrap().unwrap();
    let samples = &read(&w, &fragment).runs[0].samples;
    let offsets: Vec<i64> = samples.iter().map(|s| s.composition_offset).collect();
    assert_eq!(offsets, vec![7200, 14_400, 3600, 3600]);
    let shown: Vec<i64> = samples.iter().map(|s| s.decode_time as i64 + s.composition_offset).collect();
    assert_eq!(shown, vec![7200, 18_000, 10_800, 14_400]);
    // A trun of version 1 carries them: an offset can be negative.
    let mut frame = video(0, true);
    frame.pts = 0;
    assert!(w.push_decoded(0, &frame, 90_000).is_ok(), "shown before decoded: a negative offset");
    let fragment = w.flush().unwrap().unwrap();
    assert_eq!(read(&w, &fragment).runs[0].samples[0].composition_offset, -90_000);
}

#[test]
fn a_refused_frame_changes_nothing() {
    let mut w = Writer::new(
        &[h264(), TrackInfo::audio(Codec::L16, 8000, 2)],
        Options { fragmenting: Fragmenting::GroupOfPictures, ..Options::default() },
    )
    .unwrap();
    w.push(0, &video(0, true)).unwrap();
    w.push(1, &Frame::new(1, 0, true, Bytes::from(vec![0; 8]))).unwrap();
    let before = w.clone();
    let bad = [
        (0, Frame::new(0, 3600, false, Bytes::from_static(&[0, 0, 0, 9, 1]))),
        (1, Frame::new(1, 8000, true, Bytes::from(vec![0; 6]))),
        (2, video(3600, false)),
    ];
    for (track, frame) in bad {
        assert!(w.push(track, &frame).is_err());
    }
    assert!(matches!(w.push(0, &video(-1, false)), Err(Mp4Error::Time(_))));
    let mut far = video(i64::MAX, false);
    far.pts = i64::MAX;
    assert!(matches!(w.push_decoded(0, &far, 0), Err(Mp4Error::Time(_))));
    assert_eq!(w.pending_samples(), before.pending_samples());
    assert_eq!(w.clone().flush().unwrap(), before.clone().flush().unwrap());
}

#[test]
fn a_fragment_is_closed_before_its_bounds() {
    let mut w = Writer::new(
        &[TrackInfo::audio(Codec::Opus, 48_000, 2)],
        Options { fragmenting: Fragmenting::Duration(Duration::from_secs(3600)), ..Options::default() },
    )
    .unwrap();
    let mut closed = Vec::new();
    for i in 0..(MAX_FRAGMENT_SAMPLES as i64 + 10) {
        closed.extend(w.push(0, &Frame::new(0, i * 120, true, Bytes::from_static(&[16 << 3]))).unwrap());
    }
    assert_eq!(closed.len(), 1);
    assert_eq!(closed[0].samples, MAX_FRAGMENT_SAMPLES);
    assert_eq!(w.pending_samples(), 10);
}

#[test]
fn a_writer_holds_one_to_eight_tracks() {
    assert_eq!(Writer::new(&[], Options::default()).unwrap_err(), Mp4Error::TrackCount { count: 0 });
    let nine = vec![aac(); 9];
    assert_eq!(Writer::new(&nine, Options::default()).unwrap_err(), Mp4Error::TrackCount { count: 9 });
    assert!(Writer::new(&nine[..8], Options::default()).is_ok());
    assert_eq!(writer(Fragmenting::EveryFrame).push(2, &sound(0)).unwrap_err(), Mp4Error::NoTrack(2));
}
