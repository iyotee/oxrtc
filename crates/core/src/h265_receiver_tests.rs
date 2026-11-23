use bytes::Bytes;
use proptest::prelude::*;

use super::*;

fn hex_bytes(s: &str) -> Bytes {
    Bytes::from((0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap()).collect::<Vec<u8>>())
}

/// libx265's (FFmpeg 8.0.1) SPS of a 1920x1080 Main stream at level 4,
/// flagged Main and Main 10 compatible (`60 00 00 00`).
const MAIN_L4: &str = "420101016000000300900000030000030078a003c08010e596566924caf0168080000003008000000c84";
/// libx265's Main 10, 640x360, level 2.1, flagged Main 10 alone (`20 00 00 00`).
const MAIN10_L21: &str = "42010102200000030090000003000003003fa005020169365959a4932bc05a020000030002000003003210";

fn camera(profile: u8, compatibility: u32, tier: bool, level: u8) -> HevcCamera {
    HevcCamera { space: 0, tier, profile, compatibility, constraints: [0; 6], level }
}

fn receiver(profile: u8, tier: u8, level: Option<u8>) -> HevcReceiver {
    HevcReceiver { space: 0, profile: Some(profile), tier, level }
}

#[test]
fn go2rtc_s_player_string_is_main_at_level_5_1() {
    // go2rtc's player lists `hvc1.1.6.L153.B0` (www/video-rtc.js).
    assert_eq!(HevcReceiver::from_codec_string("hvc1.1.6.L153.B0"), Some(receiver(1, 0, Some(153))));
    assert_eq!(HevcReceiver::from_codec_string("hev1.2.4.H120.90"), Some(receiver(2, 1, Some(120))));
    assert_eq!(
        HevcReceiver::from_codec_string("hvc1.A4.10.L186.B0.0.0.0.0.0"),
        Some(HevcReceiver { space: 1, profile: Some(4), tier: 0, level: Some(186) })
    );
    // A string that stops early names less: no level is no ceiling.
    assert_eq!(HevcReceiver::from_codec_string("hvc1.1.6"), Some(receiver(1, 0, None)));
    assert_eq!(HevcReceiver::from_codec_string("hvc1.2"), Some(receiver(2, 0, None)));
    assert_eq!(HevcReceiver::from_codec_string("HVC1"), Some(HevcReceiver::ANY), "the family alone names nothing");
}

#[test]
fn what_is_not_an_h265_codec_string_is_none() {
    for bad in [
        "",
        "avc1.640029",
        "hvc",
        "hvc1.",
        "hvc1.x",
        "hvc1.32",
        "hvc1.D1",
        "hvc1.1.6.M153",
        "hvc1.1.6.L",
        "hvc1.1.6.L256",
        "hvc1.1.6.L1530",
        "hvc1.1.123456789.L153",
        "hvc1.1.g.L153",
        "hvc1.1.6.L153.B00",
        "hvc1.1.6.L153.0.0.0.0.0.0.0",
        "hvc1.1.6.L153.",
        "hvc1..6",
    ] {
        assert_eq!(HevcReceiver::from_codec_string(bad), None, "{bad:?}");
    }
    assert_eq!(HevcReceiver::from_codec_string(&format!("hvc1.1.6.L153{}", ".0".repeat(40))), None);
}

#[test]
fn a_receiver_takes_its_profile_and_tier_up_to_its_level() {
    let main51 = receiver(1, 0, Some(153));
    assert!(main51.takes(&camera(1, 0x6000_0000, false, 150)), "the owner's XM cameras: Main, level 5.0");
    assert!(main51.takes(&camera(1, 0x6000_0000, false, 153)));
    assert!(!main51.takes(&camera(1, 0x6000_0000, false, 186)), "level 6.2 is above 5.1");
    assert!(!main51.takes(&camera(2, 0x2000_0000, false, 93)), "Main 10 is not Main");
    assert!(!main51.takes(&camera(1, 0x6000_0000, true, 93)), "the High tier is above the Main tier");
    // A High-tier receiver takes the Main tier too.
    assert!(receiver(1, 1, Some(153)).takes(&camera(1, 0x6000_0000, false, 150)));
}

#[test]
fn a_main10_receiver_takes_a_main_picture_flagged_main10_compatible() {
    let main10 = receiver(2, 0, Some(180));
    assert!(main10.takes(&camera(2, 0x2000_0000, false, 150)));
    assert!(main10.takes(&camera(1, 0x6000_0000, false, 150)), "libx265's Main is Main 10 compatible");
    assert!(!main10.takes(&camera(1, 0x4000_0000, false, 150)), "a Main picture not flagged so");
}

#[test]
fn a_level_not_named_is_no_ceiling() {
    let main = receiver(1, 0, None);
    assert!(main.takes(&camera(1, 0x6000_0000, false, 186)));
    assert!(!main.takes(&camera(2, 0x2000_0000, false, 93)), "the profile is still checked");
    assert!(HevcReceiver::ANY.takes(&camera(2, 0x2000_0000, true, 186)));
    // Another profile space is never the standard's.
    let other = HevcReceiver { space: 1, ..HevcReceiver::ANY };
    assert!(!other.takes(&camera(1, 0x6000_0000, false, 93)));
}

fn h265(sps: &str) -> TrackInfo {
    TrackInfo::video(Codec::H265).with_params(CodecParams::H265 {
        vps: Bytes::from_static(&[0x40, 0x01]),
        sps: hex_bytes(sps),
        pps: Bytes::from_static(&[0x44, 0x01]),
    })
}

#[test]
fn the_first_track_no_receiver_takes_is_refused() {
    let tracks = [TrackInfo::audio(Codec::Opus, 48_000, 2), h265(MAIN_L4)];
    let go2rtc = [HevcReceiver::from_codec_string("hvc1.1.6.L153.B0").unwrap()];
    assert_eq!(refused(&go2rtc, &tracks), None, "a Main camera at level 4 plays on go2rtc's player");
    let ten = [h265(MAIN10_L21)];
    assert_eq!(refused(&go2rtc, &ten).map(|c| (c.profile, c.level)), Some((2, 63)), "a Main 10 camera does not");
    let main10 = [HevcReceiver::from_codec_string("hvc1.2.4.L153.B0").unwrap()];
    assert_eq!(refused(&main10, &ten), None, "a list with a Main 10 entry takes it");
    // An H.264 track, an H.265 one with no SPS yet: never refused.
    assert_eq!(refused(&go2rtc, &[TrackInfo::video(Codec::H264), TrackInfo::video(Codec::H265)]), None);
    // No receiver at all refuses every H.265 picture.
    assert!(refused(&[], &tracks).is_some());
}

#[test]
fn a_track_is_read_from_its_sps() {
    let read = camera_of(&h265(MAIN_L4)).unwrap();
    assert_eq!((read.profile, read.tier, read.level, read.compatibility), (1, false, 120, 0x6000_0000));
    assert_eq!(camera_of(&TrackInfo::video(Codec::H265)), None);
    assert_eq!(camera_of(&TrackInfo::video(Codec::H264)), None);
    assert_eq!(camera_of(&h265("42010101600000")), None);
}

/// A codec string as a browser's player writes one, from its parts.
fn written(space: u8, profile: u8, flags: u32, tier: bool, level: u8, constraints: &[u8]) -> String {
    let space = ["", "A", "B", "C"][usize::from(space)];
    let mut name = format!("hvc1.{space}{profile}.{flags:X}.{}{level}", if tier { 'H' } else { 'L' });
    for byte in constraints {
        name.push_str(&format!(".{byte:X}"));
    }
    name
}

proptest! {
    /// The stable twin of the `hevc_codec_string` fuzz target: whatever a
    /// page sends as a codec string is read or refused, never a panic, and
    /// what is read names numbers within their fields.
    #[test]
    fn any_codec_string_is_read_or_refused(text in "[a-zA-Z0-9.]{0,70}", bytes in proptest::collection::vec(any::<u8>(), 0..70)) {
        for input in [text.clone(), String::from_utf8_lossy(&bytes).into_owned(), format!("hvc1.{text}")] {
            if let Some(read) = HevcReceiver::from_codec_string(&input) {
                prop_assert!(read.space <= 3 && read.tier <= 1);
                prop_assert!(read.profile.is_none_or(|p| p <= 31));
                prop_assert!(input.len() <= MAX_CODEC_STRING);
            }
        }
    }

    /// Every well-formed string is read back as its parts, whatever its flags
    /// and constraint bytes.
    #[test]
    fn a_written_string_is_read_as_written(space in 0u8..4, profile in 0u8..32, flags in any::<u32>(), tier in any::<bool>(),
                                           level in any::<u8>(), constraints in proptest::collection::vec(any::<u8>(), 0..7)) {
        let name = written(space, profile, flags, tier, level, &constraints);
        let read = HevcReceiver::from_codec_string(&name);
        prop_assert_eq!(read, Some(HevcReceiver { space, profile: Some(profile), tier: u8::from(tier), level: Some(level) }), "{}", name);
    }
}
