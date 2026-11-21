#![allow(clippy::unwrap_used, clippy::indexing_slicing, clippy::panic)]

use std::sync::Arc;

use bytes::Bytes;
use oxrtc_core::{Codec, CodecParams, Frame, MediaWant, TrackInfo, Want};
use oxrtc_mp4::HevcEntry;
use oxrtc_mp4::fuzzing::{H264_PPS, H264_SPS, H265_PPS, H265_SPS, H265_VPS, hex};
use oxrtc_mp4::reader::{read_fragment, read_init};

use super::{MuxError, Muxer, Out};

fn h264() -> TrackInfo {
    TrackInfo::video(Codec::H264).with_params(CodecParams::H264 { sps: hex(H264_SPS), pps: hex(H264_PPS) })
}

fn h265() -> TrackInfo {
    TrackInfo::video(Codec::H265).with_params(CodecParams::H265 {
        vps: hex(H265_VPS),
        sps: hex(H265_SPS),
        pps: hex(H265_PPS),
    })
}

fn alaw() -> TrackInfo {
    TrackInfo::audio(Codec::Pcma, 8000, 1)
}

fn list(tracks: Vec<TrackInfo>) -> Arc<[TrackInfo]> {
    tracks.into()
}

fn every() -> Muxer {
    let want = Want {
        video: MediaWant::OneOf(vec![Codec::H265, Codec::H264]),
        audio: MediaWant::OneOf(vec![Codec::Pcma, Codec::Pcmu, Codec::L16]),
    };
    Muxer::new(want, HevcEntry::Hvc1)
}

fn picture(pts: i64, keyframe: bool) -> Frame {
    let nal: &[u8] = if keyframe { &[0, 0, 0, 3, 0x65, 0x88, 0x80] } else { &[0, 0, 0, 3, 0x41, 0x9a, 0x02] };
    Frame::new(0, pts, keyframe, Bytes::from_static(nal))
}

fn sound(pts: i64) -> Frame {
    Frame::new(1, pts, true, Bytes::from_static(&[0xd5; 160]))
}

fn binary(out: &Out) -> &Bytes {
    match out {
        Out::Binary(bytes) => bytes,
        Out::Mime(mime) => panic!("a binary frame, not {mime}"),
    }
}

#[test]
fn the_mime_type_then_the_init_segment_then_one_fragment_per_frame_from_a_keyframe() {
    let mut muxer = every();
    let out = muxer.tracks(&list(vec![h264(), alaw()])).unwrap();
    assert_eq!(out[0], Out::Mime("video/mp4; codecs=\"avc1.64001e,flac\"".into()), "G.711 goes as FLAC");
    let init = read_init(binary(&out[1])).unwrap();
    assert_eq!(out.len(), 2);
    let entries: Vec<[u8; 4]> = init.tracks.iter().map(|t| t.entry).collect();
    assert_eq!(entries, vec![*b"avc1", *b"fLaC"]);
    assert!(muxer.started());

    // Before the first keyframe nothing is written, sound included.
    assert_eq!(muxer.frame(0, &picture(90_000, false)), None);
    assert_eq!(muxer.frame(1, &sound(8000)), None);
    assert_eq!(muxer.skipped, 2);
    let mut decode_times = Vec::new();
    for (n, (track, frame)) in [
        (0, picture(93_600, true)),
        (1, sound(8_400)),
        (0, picture(97_200, false)),
        (1, sound(8_560)),
        (0, picture(100_800, false)),
    ]
    .into_iter()
    .enumerate()
    {
        let bytes = muxer.frame(track, &frame).unwrap_or_else(|| panic!("frame {n} written"));
        let fragment = read_fragment(&bytes, &init.trex).unwrap();
        assert_eq!(fragment.sequence, n as u32 + 1);
        assert_eq!(fragment.runs.len(), 1, "one frame, one track");
        assert_eq!(fragment.runs[0].track_id, track as u32 + 1);
        assert_eq!(fragment.runs[0].samples.len(), 1, "one sample per fragment");
        decode_times.push((track, fragment.runs[0].samples[0].decode_time));
    }
    assert_eq!(muxer.fragments, 5);
    // The core's timeline moved back by the keyframe's time, 1.04 s, in each
    // track's clock: 93 600 ticks of 90 kHz, 8 320 of 8 kHz.
    assert_eq!(decode_times, vec![(0, 0), (1, 80), (0, 3_600), (1, 240), (0, 7_200)]);
}

#[test]
fn a_sound_from_before_the_socket_s_zero_is_left_out() {
    let mut muxer = every();
    muxer.tracks(&list(vec![h264(), alaw()])).unwrap();
    assert!(muxer.frame(0, &picture(90_000, true)).is_some());
    // 0.99 s, where the keyframe is at 1 s: it left the camera first.
    assert_eq!(muxer.frame(1, &sound(7_920)), None);
    let bytes = muxer.frame(1, &sound(8_000)).unwrap();
    assert!(!bytes.is_empty(), "at the zero itself it is written");
}

#[test]
fn a_list_without_sound_gets_the_picture_alone() {
    let want = Want { video: MediaWant::OneOf(vec![Codec::H264]), audio: MediaWant::Skip };
    let mut muxer = Muxer::new(want, HevcEntry::Hvc1);
    let out = muxer.tracks(&list(vec![h264(), alaw()])).unwrap();
    assert_eq!(out[0], Out::Mime("video/mp4; codecs=\"avc1.64001e\"".into()));
    assert_eq!(read_init(binary(&out[1])).unwrap().tracks.len(), 1);
    assert!(muxer.frame(0, &picture(0, true)).is_some());
    assert_eq!(muxer.frame(1, &sound(0)), None, "the sound is not written");
}

#[test]
fn sound_alone_starts_at_once() {
    let want = Want { video: MediaWant::Skip, audio: MediaWant::OneOf(vec![Codec::Pcma]) };
    let mut muxer = Muxer::new(want, HevcEntry::Hvc1);
    let out = muxer.tracks(&list(vec![alaw()])).unwrap();
    assert_eq!(out[0], Out::Mime("audio/mp4; codecs=\"flac\"".into()));
    assert!(muxer.frame(0, &Frame::new(0, 0, true, Bytes::from_static(&[0xd5; 160]))).is_some());
}

#[test]
fn the_h265_entry_is_the_one_the_list_named() {
    let want = Want { video: MediaWant::OneOf(vec![Codec::H265]), audio: MediaWant::Skip };
    let mut muxer = Muxer::new(want.clone(), HevcEntry::Hev1);
    let out = muxer.tracks(&list(vec![h265()])).unwrap();
    let Out::Mime(mime) = &out[0] else { panic!("{out:?}") };
    assert!(mime.starts_with("video/mp4; codecs=\"hev1."), "{mime}");
    assert_eq!(read_init(binary(&out[1])).unwrap().tracks[0].entry, *b"hev1");
    let out = Muxer::new(want, HevcEntry::Hvc1).tracks(&list(vec![h265()])).unwrap();
    assert_eq!(read_init(binary(&out[1])).unwrap().tracks[0].entry, *b"hvc1");
}

#[test]
fn a_stream_with_no_track_the_list_takes_is_codecs_not_matched() {
    let want = Want { video: MediaWant::OneOf(vec![Codec::H265]), audio: MediaWant::OneOf(vec![Codec::Aac]) };
    let mut muxer = Muxer::new(want, HevcEntry::Hvc1);
    assert_eq!(muxer.tracks(&list(vec![h264(), alaw()])), Err(MuxError::NoCodec));
    assert!(!muxer.started());
}

#[test]
fn parameter_sets_that_come_later_hold_the_writer_back() {
    let mut muxer = every();
    let bare = TrackInfo::video(Codec::H264);
    assert_eq!(muxer.tracks(&list(vec![bare, alaw()])), Ok(Vec::new()), "no init segment without an SPS");
    assert_eq!(muxer.frame(0, &picture(0, true)), None);
    assert!(!muxer.started());
    // The source publishes its list again once it found them in band.
    let out = muxer.tracks(&list(vec![h264(), alaw()])).unwrap();
    assert!(matches!(out[0], Out::Mime(_)) && out.len() == 2, "{out:?}");
    assert!(muxer.frame(0, &picture(3600, true)).is_some());
}

#[test]
fn the_same_tracks_again_change_nothing_and_new_ones_of_the_same_codecs_get_an_init_segment() {
    let mut muxer = every();
    muxer.tracks(&list(vec![h264(), alaw()])).unwrap();
    assert!(muxer.frame(0, &picture(0, true)).is_some());
    assert_eq!(muxer.tracks(&list(vec![h264(), alaw()])), Ok(Vec::new()));
    assert!(muxer.frame(0, &picture(3600, false)).is_some(), "the writer goes on");
    // A reconnection with other parameter sets of the same profile and level.
    let mut sps = hex(H264_SPS).to_vec();
    sps.push(0x80);
    let other = TrackInfo::video(Codec::H264).with_params(CodecParams::H264 { sps: sps.into(), pps: hex(H264_PPS) });
    let out = muxer.tracks(&list(vec![other, alaw()])).unwrap();
    assert_eq!(out.len(), 1, "an init segment, no second MIME type: {out:?}");
    read_init(binary(&out[0])).unwrap();
    assert_eq!(muxer.frame(0, &picture(7200, false)), None, "the new segment starts at a keyframe");
    assert!(muxer.frame(0, &picture(10_800, true)).is_some());
    // Tracks of another codec: the player's SourceBuffer cannot follow.
    assert_eq!(muxer.tracks(&list(vec![h265()])), Err(MuxError::CodecsChanged));
}

#[test]
fn after_a_loss_nothing_is_written_until_a_keyframe() {
    let mut muxer = every();
    muxer.tracks(&list(vec![h264(), alaw()])).unwrap();
    assert!(muxer.frame(0, &picture(0, true)).is_some());
    muxer.lost();
    assert_eq!(muxer.frame(0, &picture(90_000, false)), None);
    assert_eq!(muxer.frame(1, &sound(8000)), None);
    let bytes = muxer.frame(0, &picture(93_600, true)).unwrap();
    assert!(bytes.len() > 8);
    assert!(muxer.frame(1, &sound(8160)).is_some());
}

#[test]
fn a_picture_the_writer_refuses_makes_the_next_ones_wait_for_a_keyframe() {
    let mut muxer = every();
    muxer.tracks(&list(vec![h264(), alaw()])).unwrap();
    assert!(muxer.frame(0, &picture(0, true)).is_some());
    // Length prefixes that do not add up.
    let broken = Frame::new(0, 3600, false, Bytes::from_static(&[0, 0, 0, 9, 0x41]));
    assert_eq!(muxer.frame(0, &broken), None);
    assert_eq!(muxer.frame(0, &picture(7200, false)), None);
    assert!(muxer.frame(0, &picture(10_800, true)).is_some());
}
