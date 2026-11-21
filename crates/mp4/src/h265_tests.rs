//! SPS of libx265 (FFmpeg 8.0.1), read as FFmpeg reads them.

use super::*;

fn hex(s: &str) -> Vec<u8> {
    (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap()).collect()
}

pub(crate) const MAIN_1080P_VPS: &str = crate::fuzzing::H265_VPS;
/// Main, 1920x1080, level 4: a conformance window crops 1088 to 1080.
pub(crate) const MAIN_1080P_SPS: &str = crate::fuzzing::H265_SPS;
pub(crate) const MAIN_1080P_PPS: &str = crate::fuzzing::H265_PPS;
/// Main 10, 640x360, level 2.1.
const MAIN10_360P_SPS: &str = "42010102200000030090000003000003003fa005020169365959a4932bc05a020000030002000003003210";

#[test]
fn the_profile_tier_level_and_size_are_those_ffmpeg_reads() {
    let sps = parse_sps(&hex(MAIN_1080P_SPS)).unwrap();
    assert_eq!((sps.width, sps.height, sps.chroma_format, sps.bit_depth_luma), (1920, 1080, 1, 8));
    assert_eq!(
        sps.ptl,
        ProfileTierLevel {
            space: 0,
            tier: false,
            profile: 1,
            compatibility: 0x6000_0000,
            constraints: [0x90, 0, 0, 0, 0, 0],
            level: 120
        }
    );
    assert_eq!((sps.max_sub_layers, sps.temporal_id_nesting), (1, true));
    assert_eq!(codec_string("hvc1", &sps.ptl), "hvc1.1.6.L120.90");
    assert_eq!(codec_string("hev1", &sps.ptl), "hev1.1.6.L120.90");

    let ten = parse_sps(&hex(MAIN10_360P_SPS)).unwrap();
    assert_eq!((ten.width, ten.height, ten.bit_depth_luma, ten.bit_depth_chroma), (640, 360, 10, 10));
    assert_eq!(codec_string("hvc1", &ten.ptl), "hvc1.2.4.L63.90");
}

#[test]
fn the_codec_string_names_space_and_tier_and_drops_trailing_zero_bytes() {
    let ptl = ProfileTierLevel {
        space: 2,
        tier: true,
        profile: 4,
        compatibility: 0x0800_0000,
        constraints: [0xb0, 0, 0x01, 0, 0, 0],
        level: 153,
    };
    assert_eq!(codec_string("hvc1", &ptl), "hvc1.B4.10.H153.B0.00.01");
    let bare = ProfileTierLevel { constraints: [0; 6], ..ptl };
    assert_eq!(codec_string("hvc1", &bare), "hvc1.B4.10.H153");
}

#[test]
fn hvcc_holds_three_complete_arrays() {
    let (vps, sps, pps) = (hex(MAIN_1080P_VPS), hex(MAIN_1080P_SPS), hex(MAIN_1080P_PPS));
    let parsed = parse_sps(&sps).unwrap();
    let boxed = hvcc(&vps, &sps, &pps, &parsed).unwrap();
    assert_eq!(&boxed[4..8], b"hvcC");
    assert_eq!(u32::from_be_bytes(boxed[..4].try_into().unwrap()) as usize, boxed.len());
    let body = &boxed[8..];
    assert_eq!(body[0], 1);
    assert_eq!(body[1], 1, "space 0, tier 0, profile 1");
    assert_eq!(&body[2..6], &[0x60, 0, 0, 0]);
    assert_eq!(&body[6..12], &[0x90, 0, 0, 0, 0, 0]);
    assert_eq!(body[12], 120);
    assert_eq!(&body[13..21], &[0xf0, 0, 0xfc, 0xfd, 0xf8, 0xf8, 0, 0]);
    assert_eq!(body[21], 0b0000_1111, "one temporal layer, nested, four-byte lengths");
    assert_eq!(body[22], 3);
    let mut at = 23;
    for (kind, nal) in [(32u8, &vps), (33, &sps), (34, &pps)] {
        assert_eq!(body[at], 0x80 | kind);
        assert_eq!(&body[at + 1..at + 3], &[0, 1]);
        assert_eq!(usize::from(u16::from_be_bytes([body[at + 3], body[at + 4]])), nal.len());
        assert_eq!(&body[at + 5..at + 5 + nal.len()], &nal[..]);
        at += 5 + nal.len();
    }
    assert_eq!(at, body.len());
}

#[test]
fn a_truncated_or_foreign_set_is_refused() {
    let sps = hex(MAIN_1080P_SPS);
    for cut in 0..20 {
        assert!(parse_sps(&sps[..cut]).is_err(), "cut at {cut}");
    }
    assert!(parse_sps(&hex(MAIN_1080P_VPS)).is_err(), "a VPS is not an SPS");
}
