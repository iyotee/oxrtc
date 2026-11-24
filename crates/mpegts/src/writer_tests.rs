use bytes::Bytes;
use oxrtc_core::{Codec, CodecParams, Frame, TrackInfo};

use super::*;
use crate::reader::read;

fn h264() -> TrackInfo {
    TrackInfo::video(Codec::H264)
        .with_params(CodecParams::H264 { sps: Bytes::from_static(&[0x67, 1]), pps: Bytes::from_static(&[0x68, 2]) })
}

fn aac() -> TrackInfo {
    TrackInfo::audio(Codec::Aac, 48_000, 2).with_params(CodecParams::Aac { config: Bytes::from_static(&[0x11, 0x90]) })
}

fn picture(key: bool, fill: u8) -> Bytes {
    let nal = if key { [0x65, fill, fill] } else { [0x41, fill, fill] };
    Bytes::from([&3u32.to_be_bytes()[..], &nal].concat())
}

#[test]
fn the_tables_name_each_track_and_the_pcr_on_the_picture() {
    let mut w = Writer::new(&[aac(), h264(), TrackInfo::audio(Codec::Opus, 48_000, 2)]).unwrap();
    let tables = w.tables();
    assert_eq!(tables.len(), 2 * crate::PACKET_SIZE);
    let r = read(&tables).unwrap();
    assert_eq!(r.pmt_pid, Some(PMT_PID));
    assert_eq!(r.pcr_pid, Some(FIRST_PID + 1), "the video track's");
    let types: Vec<(u8, u16)> = r.streams.iter().map(|s| (s.stream_type, s.pid)).collect();
    assert_eq!(types, [(0x0F, 0x100), (0x1B, 0x101), (0x06, 0x102)]);
    assert_eq!(&r.streams[2].descriptors[..6], &[0x05, 4, b'O', b'p', b'u', b's']);
    assert_eq!(w.pids(), [0x100, 0x101, 0x102]);
}

#[test]
fn frames_come_back_with_their_times_marks_and_bytes() {
    let mut w = Writer::new(&[h264(), aac()]).unwrap();
    let mut out = w.tables().to_vec();
    out.extend_from_slice(&w.push(0, &Frame::new(0, 900_000, true, picture(true, 1))).unwrap());
    out.extend_from_slice(&w.push(1, &Frame::new(1, 480_000, true, Bytes::from_static(&[0x21; 300]))).unwrap());
    out.extend_from_slice(&w.push(0, &Frame::new(0, 903_600, false, picture(false, 2))).unwrap());
    out.extend_from_slice(&w.tables());
    // A B-frame's decode time, written before its presentation.
    out.extend_from_slice(&w.push_decoded(0, &Frame::new(0, 914_400, false, picture(false, 3)), 907_200).unwrap());
    let r = read(&out).unwrap();
    assert_eq!(r.tables, 4);
    let video: Vec<_> = r.pes.iter().filter(|p| p.pid == 0x100).collect();
    let audio: Vec<_> = r.pes.iter().filter(|p| p.pid == 0x101).collect();
    assert_eq!((video.len(), audio.len()), (3, 1));
    assert_eq!(video[0].pts, Some(900_000 + TIME_OFFSET));
    assert_eq!(video[0].dts, None, "the same decode time is not written");
    assert_eq!(video[0].pcr, Some(900_000), "the decode time without the offset");
    assert!(video[0].random_access && !video[1].random_access);
    assert_eq!((video[0].stream_id, video[0].declared), (0xE0, 0));
    assert_eq!(
        video[0].data,
        [&[0, 0, 0, 1, 0x09, 0xF0][..], &[0, 0, 0, 1, 0x67, 1], &[0, 0, 0, 1, 0x68, 2], &[0, 0, 0, 1, 0x65, 1, 1]]
            .concat()
    );
    assert_eq!((video[2].pts, video[2].dts), (Some(914_400 + TIME_OFFSET), Some(907_200 + TIME_OFFSET)));
    // 480 000 ticks of 48 kHz: 10 s, 900 000 on the 90 kHz clock.
    assert_eq!(audio[0].pts, Some(900_000 + TIME_OFFSET));
    assert_eq!((audio[0].stream_id, audio[0].pcr, audio[0].random_access), (0xC0, None, false));
    assert_eq!(&audio[0].data[7..], &[0x21; 300]);
    assert_eq!(usize::from(audio[0].declared), 3 + 5 + 307);
}

#[test]
fn sound_alone_carries_the_pcr_and_random_access() {
    let mut w = Writer::new(&[aac()]).unwrap();
    let mut out = w.tables().to_vec();
    for i in 0..3 {
        out.extend_from_slice(&w.push(0, &Frame::new(0, i * 1024, false, Bytes::from_static(&[1; 20]))).unwrap());
    }
    let r = read(&out).unwrap();
    assert_eq!(r.pcr_pid, Some(0x100));
    assert!(r.pes.iter().all(|p| p.random_access && p.pcr.is_some()));
    assert_eq!(r.pes[2].pcr, Some(2 * 1024 * 90_000 / 48_000));
}

#[test]
fn a_refused_frame_changes_nothing() {
    let mut w = Writer::new(&[h264()]).unwrap();
    let mut out = w.tables().to_vec();
    out.extend_from_slice(&w.push(0, &Frame::new(0, 0, true, picture(true, 1))).unwrap());
    assert!(w.push(0, &Frame::new(0, 3600, false, Bytes::from_static(&[0, 0, 0, 9, 1]))).is_err());
    assert!(w.push(0, &Frame::new(0, -1, false, picture(false, 1))).is_err());
    assert!(w.push_decoded(0, &Frame::new(0, 3600, false, picture(false, 1)), 7200).is_err());
    assert!(matches!(w.push(1, &Frame::new(1, 0, true, picture(true, 1))), Err(TsError::NoTrack(1))));
    out.extend_from_slice(&w.push(0, &Frame::new(0, 3600, false, picture(false, 2))).unwrap());
    assert_eq!(read(&out).unwrap().pes.len(), 2, "the counters ran on unbroken");
}

#[test]
fn what_the_writer_cannot_carry_is_refused() {
    assert!(matches!(Writer::new(&[]), Err(TsError::TrackCount { count: 0 })));
    assert!(matches!(Writer::new(&vec![aac(); 9]), Err(TsError::TrackCount { count: 9 })));
    assert!(Writer::new(&[h264(), TrackInfo::audio(Codec::Pcma, 8000, 1)]).is_err());
    let mut zero = aac();
    zero.clock_rate = 0;
    assert!(Writer::new(&[zero]).is_err());
    assert!(supported(&h264()) && supported(&TrackInfo::video(Codec::H265)) && supported(&aac()));
    assert!(!supported(&TrackInfo::audio(Codec::Pcmu, 8000, 1)) && !supported(&TrackInfo::video(Codec::Jpeg)));
}

#[test]
fn a_large_keyframe_and_times_past_33_bits() {
    let mut w = Writer::new(&[h264()]).unwrap();
    let big = Bytes::from([&200_000u32.to_be_bytes()[..], &[0x65], &vec![7u8; 199_999]].concat());
    let mut out = w.tables().to_vec();
    out.extend_from_slice(&w.push(0, &Frame::new(0, (1 << 33) + 90_000, true, big)).unwrap());
    let r = read(&out).unwrap();
    assert_eq!(r.pes[0].pts, Some(90_000 + TIME_OFFSET), "wrapped at 33 bits");
    assert_eq!(r.pes[0].data.len(), 6 + 4 + 2 + 4 + 2 + 4 + 200_000);
}
