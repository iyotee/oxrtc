use bytes::Bytes;
use oxrtc_core::{Codec, CodecParams, MediaKind, TrackInfo};

use super::*;
use crate::HevcEntry;

use crate::fuzzing::hex;

pub(crate) fn h264() -> TrackInfo {
    TrackInfo::video(Codec::H264).with_params(CodecParams::H264 {
        sps: hex(crate::h264::tests::HIGH_640X360),
        pps: hex(crate::fuzzing::H264_PPS),
    })
}

pub(crate) fn h265() -> TrackInfo {
    use crate::h265::tests::{MAIN_1080P_PPS, MAIN_1080P_SPS, MAIN_1080P_VPS};
    TrackInfo::video(Codec::H265).with_params(CodecParams::H265 {
        vps: hex(MAIN_1080P_VPS),
        sps: hex(MAIN_1080P_SPS),
        pps: hex(MAIN_1080P_PPS),
    })
}

pub(crate) fn aac() -> TrackInfo {
    TrackInfo::audio(Codec::Aac, 48_000, 2).with_params(CodecParams::Aac { config: Bytes::from_static(&[0x11, 0x90]) })
}

#[test]
fn each_codec_has_its_entry_and_string() {
    let cases = [
        (h264(), b"avc1", "avc1.64001e", 90_000),
        (h265(), b"hvc1", "hvc1.1.6.L120.90", 90_000),
        (aac(), b"mp4a", "mp4a.40.2", 48_000),
        (TrackInfo::audio(Codec::Opus, 48_000, 2), b"Opus", "opus", 48_000),
        (TrackInfo::audio(Codec::Pcma, 8000, 1), b"fLaC", "flac", 8000),
        (TrackInfo::audio(Codec::Pcmu, 8000, 1), b"fLaC", "flac", 8000),
        (TrackInfo::audio(Codec::L16, 44_100, 2), b"fLaC", "flac", 44_100),
    ];
    for (info, entry, codec, timescale) in cases {
        let track = Track::new(&info, 1, HevcEntry::Hvc1).unwrap();
        assert_eq!(&track.entry[4..8], entry, "{codec}");
        assert_eq!(track.codec, codec);
        assert_eq!(codec_string(&info).unwrap(), codec);
        assert_eq!(track.timescale, timescale);
        assert_eq!(track.kind, info.kind);
    }
    let video = Track::new(&h264(), 1, HevcEntry::Hvc1).unwrap();
    assert_eq!((video.width, video.height), (640, 360));
    let hev1 = Track::new(&h265(), 1, HevcEntry::Hev1).unwrap();
    assert_eq!((&hev1.entry[4..8], hev1.codec.as_str()), (&b"hev1"[..], "hev1.1.6.L120.90"));
}

#[test]
fn a_track_without_what_it_needs_is_refused() {
    assert_eq!(codec_string(&TrackInfo::video(Codec::Jpeg)), Err(Mp4Error::Unsupported("Motion JPEG")));
    assert!(!supported(&TrackInfo::video(Codec::Jpeg)));
    assert!(supported(&h264()));
    assert!(matches!(codec_string(&TrackInfo::video(Codec::H264)), Err(Mp4Error::MissingConfig(_))));
    assert!(matches!(codec_string(&TrackInfo::video(Codec::H265)), Err(Mp4Error::MissingConfig(_))));
    assert!(matches!(codec_string(&TrackInfo::audio(Codec::Aac, 48_000, 2)), Err(Mp4Error::MissingConfig(_))));
    assert!(codec_string(&TrackInfo::audio(Codec::Opus, 48_000, 6)).is_err());
    assert!(codec_string(&TrackInfo::audio(Codec::Opus, 8000, 1)).is_err());
    assert!(codec_string(&TrackInfo::audio(Codec::Pcma, 0, 1)).is_err());
    assert!(codec_string(&TrackInfo::audio(Codec::Pcma, 8000, 0)).is_err());
    let mut wrong = h264();
    wrong.kind = MediaKind::Audio;
    assert!(codec_string(&wrong).is_err());
    let garbage = TrackInfo::video(Codec::H264)
        .with_params(CodecParams::H264 { sps: Bytes::from_static(&[0x67, 0x64]), pps: Bytes::new() });
    assert!(codec_string(&garbage).is_err());
}

#[test]
fn a_frame_its_track_cannot_carry_is_refused() {
    let mut video = Track::new(&h264(), 1, HevcEntry::Hvc1).unwrap();
    assert!(video.pack(&Bytes::from_static(&[0, 0, 0, 5, 0x65])).is_err(), "a length past the end");
    assert!(video.pack(&Bytes::new()).is_err());
    let (sample, content) = video.pack(&Bytes::from_static(&[0, 0, 0, 1, 0x65])).unwrap();
    assert_eq!((&sample[..], content), (&[0, 0, 0, 1, 0x65][..], None));
    let mut opus = Track::new(&TrackInfo::audio(Codec::Opus, 48_000, 2), 1, HevcEntry::Hvc1).unwrap();
    assert!(opus.pack(&Bytes::new()).is_err());
    assert_eq!(opus.pack(&Bytes::from_static(&[0xf8, 1])).unwrap().1, Some(960));
    let mut aac = Track::new(&aac(), 1, HevcEntry::Hvc1).unwrap();
    assert!(aac.pack(&Bytes::new()).is_err());
    assert_eq!(aac.pack(&Bytes::from_static(&[0x21])).unwrap().1, Some(1024));
    let mut pcm = Track::new(&TrackInfo::audio(Codec::L16, 16_000, 1), 1, HevcEntry::Hvc1).unwrap();
    assert!(pcm.pack(&Bytes::from_static(&[1, 2, 3])).is_err());
    let (frame, content) = pcm.pack(&Bytes::from_static(&[1, 2, 3, 4])).unwrap();
    assert_eq!(content, Some(2));
    assert_eq!(&frame[..2], &[0xff, 0xf9]);
    let big = Bytes::from(vec![0; crate::MAX_SAMPLE_BYTES + 1]);
    assert_eq!(pcm.pack(&big), Err(Mp4Error::TooLarge(crate::MAX_SAMPLE_BYTES + 1)));
}
