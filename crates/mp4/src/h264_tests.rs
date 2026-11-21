//! SPS of libx264 (FFmpeg 8.0.1), read as FFmpeg reads them.

use super::*;

fn hex(s: &str) -> Vec<u8> {
    (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap()).collect()
}

/// High profile, 640x360: cropped from 368 lines.
pub(crate) const HIGH_640X360: &str = crate::fuzzing::H264_SPS;
/// Main profile, 1920x1080: cropped from 1088.
const MAIN_1080P: &str = "674d4028eca03c0113f2e022000003000200000300641e30632c";
/// Constrained baseline, 320x240.
const BASELINE_240P: &str = "6742c00dd90141fb011000000300100000030320f142a480";
/// High 4:2:2, interlaced 720x576: field pictures, chroma 4:2:2.
const HIGH422_576I: &str = "677a001ebcd940b424d8088000000300800000190f8a14cb";

#[test]
fn the_picture_size_and_profile_are_those_ffmpeg_reads() {
    let cases = [
        (HIGH_640X360, 100, 0x00, 30, 1, 640, 360, "avc1.64001e"),
        (MAIN_1080P, 77, 0x40, 40, 1, 1920, 1080, "avc1.4d4028"),
        (BASELINE_240P, 66, 0xc0, 13, 1, 320, 240, "avc1.42c00d"),
        (HIGH422_576I, 122, 0x00, 30, 2, 720, 576, "avc1.7a001e"),
    ];
    for (sps, profile, constraints, level, chroma, width, height, codec) in cases {
        let parsed = parse_sps(&hex(sps)).unwrap();
        assert_eq!(
            (parsed.profile, parsed.constraints, parsed.level, parsed.chroma_format, parsed.width, parsed.height),
            (profile, constraints, level, chroma, width, height),
            "{sps}"
        );
        assert_eq!(parsed.bit_depth_luma, 8);
        assert_eq!(codec_string(&parsed), codec);
    }
}

#[test]
fn avcc_holds_both_sets_and_the_high_profile_extension() {
    let sps = hex(HIGH_640X360);
    let pps = hex(crate::fuzzing::H264_PPS);
    let parsed = parse_sps(&sps).unwrap();
    let boxed = avcc(&sps, &pps, &parsed).unwrap();
    assert_eq!(&boxed[4..8], b"avcC");
    assert_eq!(usize::from(u32::from_be_bytes(boxed[..4].try_into().unwrap()) as u16), boxed.len());
    let body = &boxed[8..];
    assert_eq!(&body[..6], &[1, 100, 0, 30, 0xff, 0xe1]);
    assert_eq!(&body[6..8], &(sps.len() as u16).to_be_bytes());
    assert_eq!(&body[8..8 + sps.len()], &sps[..]);
    let at = 8 + sps.len();
    assert_eq!(body[at], 1);
    assert_eq!(&body[at + 3..at + 3 + pps.len()], &pps[..]);
    // High profile: chroma 4:2:0, 8 bits, no SPS extension.
    assert_eq!(&body[at + 3 + pps.len()..], &[0xfd, 0xf8, 0xf8, 0]);
    // Main profile has no extension.
    let main = hex(MAIN_1080P);
    let boxed = avcc(&main, &pps, &parse_sps(&main).unwrap()).unwrap();
    assert_eq!(boxed.len(), 8 + 6 + 2 + main.len() + 3 + pps.len());
}

#[test]
fn a_truncated_or_foreign_set_is_refused() {
    let sps = hex(HIGH_640X360);
    for cut in 0..8 {
        assert!(parse_sps(&sps[..cut]).is_err(), "cut at {cut}");
    }
    assert!(parse_sps(&hex("68ebe3cb22c0")).is_err(), "a PPS is not an SPS");
    assert!(parse_sps(&[]).is_err());
}

#[test]
fn a_cropping_larger_than_the_picture_is_refused() {
    // Baseline, 16x16, frame_cropping with a right offset of 100.
    // profile 66, constraints 0, level 10, then: sps_id ue(0)=1, log2_max_frame_num ue(0)=1,
    // poc_type ue(2)=011, max_num_ref ue(0)=1, gaps 0, width ue(0)=1, height ue(0)=1,
    // frame_mbs_only 1, direct_8x8 1, cropping 1, left ue(0)=1, right ue(100), top 1, bottom 1.
    let mut bits = String::from("110111011111");
    bits.push('1'); // left = 0
    bits.push_str(&format!("{:0>13b}", 101)); // right = 100: 6 zeros, then 1100101
    bits.push_str("11"); // top, bottom = 0
    bits.push('1'); // stop bit
    while bits.len() % 8 != 0 {
        bits.push('0');
    }
    let mut nal = vec![0x67, 66, 0, 10];
    nal.extend((0..bits.len()).step_by(8).map(|i| u8::from_str_radix(&bits[i..i + 8], 2).unwrap()));
    assert_eq!(parse_sps(&nal), Err(Mp4Error::invalid("H.264 SPS", "a cropping larger than the picture")));
}
