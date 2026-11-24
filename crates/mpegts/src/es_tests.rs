use super::*;

fn avcc(units: &[&[u8]]) -> Vec<u8> {
    let mut out = Vec::new();
    for unit in units {
        out.extend_from_slice(&(unit.len() as u32).to_be_bytes());
        out.extend_from_slice(unit);
    }
    out
}

fn h264() -> Es {
    Es::H264 { sets: vec![Bytes::from_static(&[0x67, 1, 2]), Bytes::from_static(&[0x68, 3])] }
}

#[test]
fn a_keyframe_without_its_sets_gets_the_tracks_after_the_delimiter() {
    let out = h264().write(&avcc(&[&[0x65, 9, 9]]), true).unwrap();
    assert_eq!(
        out,
        [&[0, 0, 0, 1, 0x09, 0xF0][..], &[0, 0, 0, 1, 0x67, 1, 2], &[0, 0, 0, 1, 0x68, 3], &[0, 0, 0, 1, 0x65, 9, 9]]
            .concat()
    );
    // A frame that is not a keyframe gets none.
    let out = h264().write(&avcc(&[&[0x41, 7]]), false).unwrap();
    assert_eq!(out, [0, 0, 0, 1, 0x09, 0xF0, 0, 0, 0, 1, 0x41, 7]);
}

#[test]
fn a_frame_with_its_own_delimiter_and_sets_is_kept_as_it_is() {
    let data = avcc(&[&[0x09, 0x10], &[0x67, 5], &[0x68, 6], &[0x65, 1]]);
    let out = h264().write(&data, true).unwrap();
    assert_eq!(out, [0, 0, 0, 1, 0x09, 0x10, 0, 0, 0, 1, 0x67, 5, 0, 0, 0, 1, 0x68, 6, 0, 0, 0, 1, 0x65, 1]);
    // Its own delimiter, no sets: the sets after the delimiter.
    let out = h264().write(&avcc(&[&[0x09, 0x10], &[0x65, 1]]), true).unwrap();
    assert_eq!(&out[..6], &[0, 0, 0, 1, 0x09, 0x10]);
    assert_eq!(&out[6..11], &[0, 0, 0, 1, 0x67]);
}

#[test]
fn h265_gets_its_own_delimiter_and_three_sets() {
    let es = Es::H265 {
        sets: vec![Bytes::from_static(&[0x40, 1]), Bytes::from_static(&[0x42, 1]), Bytes::from_static(&[0x44, 1])],
    };
    let out = es.write(&avcc(&[&[0x26, 1, 0xAA]]), true).unwrap();
    assert_eq!(&out[..7], &[0, 0, 0, 1, 0x46, 0x01, 0x50]);
    assert_eq!(out.len(), 7 + 3 * 6 + 7);
    assert!(es.write(&[0, 0, 0, 9, 1], false).is_err(), "a prefix past the end");
    assert!(es.write(&[], false).is_err());
}

#[test]
fn aac_gets_adts_headers_ffmpeg_reads() {
    let track = TrackInfo::audio(Codec::Aac, 48_000, 2)
        .with_params(CodecParams::Aac { config: Bytes::from_static(&[0x11, 0x90]) });
    let es = Es::of(&track).unwrap();
    let out = es.write(&[0x21; 100], true).unwrap();
    // FFmpeg's ADTS header of a 107-byte AAC-LC 48 kHz stereo frame.
    assert_eq!(&out[..7], &[0xFF, 0xF1, 0x4C, 0x80, 0x0D, 0x7F, 0xFC]);
    assert_eq!(&out[7..], &[0x21; 100]);
    assert!(es.write(&[], true).is_err());
    assert!(es.write(&vec![0; 9000], true).is_err());
    // HE-AAC: the core's AAC-LC at 24 kHz.
    let he = TrackInfo::audio(Codec::Aac, 48_000, 2)
        .with_params(CodecParams::Aac { config: Bytes::from_static(&[0x2b, 0x11, 0x88, 0]) });
    assert!(matches!(Es::of(&he).unwrap(), Es::Aac { profile: 1, rate_index: 6, channels: 2 }));
    // An explicit rate, no layout, no config: refused.
    for config in [&[0x17, 0x80, 0x5d, 0xc0, 0x10][..], &[0x11, 0x80]] {
        let t = TrackInfo::audio(Codec::Aac, 48_000, 2)
            .with_params(CodecParams::Aac { config: Bytes::copy_from_slice(config) });
        assert!(Es::of(&t).is_err(), "{config:?}");
    }
    assert!(Es::of(&TrackInfo::audio(Codec::Aac, 48_000, 2)).is_err());
}

#[test]
fn opus_packets_get_their_control_header() {
    let es = Es::of(&TrackInfo::audio(Codec::Opus, 48_000, 2)).unwrap();
    assert_eq!(es.write(&[1, 2, 3], true).unwrap(), [0x7F, 0xE0, 3, 1, 2, 3]);
    let long = es.write(&[7; 600], true).unwrap();
    assert_eq!(&long[..5], &[0x7F, 0xE0, 0xFF, 0xFF, 90]);
    assert_eq!(long.len(), 5 + 600);
    assert_eq!(es.stream_type(2).1, [0x05, 4, b'O', b'p', b'u', b's', 0x7F, 2, 0x80, 2]);
    assert!(Es::of(&TrackInfo::audio(Codec::Opus, 48_000, 6)).is_err());
    assert!(Es::of(&TrackInfo::audio(Codec::Opus, 16_000, 1)).is_err());
}

#[test]
fn what_mpeg_ts_does_not_carry_is_refused() {
    for track in
        [TrackInfo::audio(Codec::Pcma, 8000, 1), TrackInfo::audio(Codec::L16, 16_000, 1), TrackInfo::video(Codec::Jpeg)]
    {
        assert!(Es::of(&track).is_err(), "{track:?}");
    }
}
