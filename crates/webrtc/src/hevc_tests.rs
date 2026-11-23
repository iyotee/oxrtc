use oxrtc_core::h265_receiver::HevcCamera;

use super::*;

/// What Chrome 157 offers on the owner's PC: Main and Main 10, Main tier, level 6.0.
const CHROME: [HevcReceiver; 2] = [
    HevcReceiver { space: 0, profile: Some(1), tier: 0, level: Some(180) },
    HevcReceiver { space: 0, profile: Some(2), tier: 0, level: Some(180) },
];

fn ptl(profile: u8, tier: u8, level: u8) -> HevcPtl {
    HevcPtl { profile, tier, level }
}

fn receiver(profile: u8, tier: u8, level: Option<u8>) -> HevcReceiver {
    HevcReceiver { space: 0, profile: Some(profile), tier, level }
}

/// A camera of `profile`, flagged as libx265 flags it (Main also Main 10
/// compatible; Main 10 alone).
fn camera(profile: u8, tier: u8, level: u8) -> HevcCamera {
    let compatibility = if profile == 1 { 0x6000_0000 } else { 0x8000_0000 >> profile };
    HevcCamera { space: 0, tier: tier == 1, profile, compatibility, constraints: [0; 6], level }
}

#[test]
fn str0m_is_told_the_camera_s_numbers_or_else_a_payload_type_of_the_offer() {
    // Taken: the camera's own, so the answer names its level.
    assert_eq!(answered(Some(&camera(1, 0, 150)), &CHROME), Some(ptl(1, 0, 150)));
    assert_eq!(answered(Some(&camera(2, 0, 63)), &CHROME), Some(ptl(2, 0, 63)));
    // Sent though no payload type takes it (nothing else to send): the
    // offer's first type, which str0m matches; a Main 10 camera told as
    // itself found no Main type and the answer had no video.
    assert_eq!(answered(Some(&camera(2, 0, 63)), &CHROME[..1]), Some(ptl(1, 0, 180)));
    assert_eq!(answered(Some(&camera(1, 0, 186)), &CHROME), Some(ptl(1, 0, 180)));
    // Taken, at a level str0m cannot say: the type that takes it.
    assert_eq!(answered(Some(&camera(2, 0, 151)), &CHROME), Some(ptl(2, 0, 180)));
    // No SPS yet: the offer's first type, Main 6.0 for Chrome (enable_h265's numbers).
    assert_eq!(answered(None, &CHROME), Some(ptl(1, 0, 180)));
    // Nothing standard to say: enable_h265.
    assert_eq!(answered(None, &[]), None);
    assert_eq!(answered(None, &[receiver(1, 0, Some(151))]), None);
    assert_eq!(answered(Some(&camera(1, 0, 186)), &[receiver(1, 0, Some(151))]), None);
}

#[test]
fn a_main_camera_on_a_main10_only_offer_is_answered_on_its_type() {
    // libx265's Main is flagged Main 10 compatible: a Main 10 decoder takes it,
    // and str0m is told the payload type's profile, which it matches.
    assert_eq!(answered(Some(&camera(1, 0, 150)), &CHROME[1..]), Some(ptl(2, 0, 150)));
}

#[test]
fn an_offer_with_no_level_is_answered_on_the_camera_s() {
    // A payload type with no level-id (no fmtp, or profile-id alone): no
    // ceiling, so a level 6.2 camera is taken, at its own level.
    let no_level = [receiver(1, 0, None)];
    assert_eq!(answered(Some(&camera(1, 0, 186)), &no_level), Some(ptl(1, 0, 186)));
    // No SPS yet: RFC 7798's level, which str0m reads such a type as.
    assert_eq!(answered(None, &no_level), Some(ptl(1, 0, 93)));
}

#[test]
fn only_the_standard_numbers_go_to_str0m() {
    assert!(ptl(1, 0, 150).is_standard());
    assert!(ptl(2, 1, 186).is_standard());
    assert!(!ptl(1, 0, 151).is_standard(), "no level 5.03");
    assert!(!ptl(0, 0, 93).is_standard());
    assert!(!ptl(1, 2, 93).is_standard());
}

#[test]
fn an_offer_s_fmtp_is_read_as_str0m_reads_it_its_level_a_ceiling_only_when_written() {
    use str0m::change::SdpOffer;
    let sdp = |fmtp: &str| {
        format!(
            "v=0\r\no=- 1 2 IN IP4 127.0.0.1\r\ns=-\r\nt=0 0\r\na=group:BUNDLE 0\r\n\
             m=video 9 UDP/TLS/RTP/SAVPF 49\r\nc=IN IP4 0.0.0.0\r\na=ice-ufrag:abcd\r\na=ice-pwd:abcdefghijklmnopqrstuvwx\r\n\
             a=fingerprint:sha-256 DA:1D:6F:73:9E:94:51:25:3A:10:F8:DE:0D:4B:39:C8:6C:0A:16:2B:E8:EB:65:13:2B:CB:AB:71:06:DF:C1:01\r\n\
             a=setup:actpass\r\na=mid:0\r\na=recvonly\r\na=rtcp-mux\r\na=rtpmap:49 H265/90000\r\n{fmtp}"
        )
    };
    let read = |fmtp: &str| crate::offer::OfferedCodecs::from_sdp(&sdp(fmtp)).unwrap().h265;
    let _ = SdpOffer::from_sdp_string(&sdp("")).unwrap();
    let chrome = "a=fmtp:49 level-id=180;profile-id=1;tier-flag=0;tx-mode=SRST\r\n";
    assert_eq!(read(chrome), vec![receiver(1, 0, Some(180))]);
    // No fmtp: Main, Main tier, and no ceiling (RFC 7798 says 3.1; D41).
    assert_eq!(read(""), vec![receiver(1, 0, None)]);
    // A lone profile-id: that profile, no ceiling.
    assert_eq!(read("a=fmtp:49 profile-id=2\r\n"), vec![receiver(2, 0, None)]);
    // A partial fmtp: str0m keeps none of the three; its written level is a
    // ceiling, its profile and tier str0m's (Main, Main tier).
    assert_eq!(read("a=fmtp:49 level-id=150;profile-id=2\r\n"), vec![receiver(1, 0, Some(150))]);
    assert_eq!(read("a=fmtp:49 tier-flag=0;profile-id=1\r\n"), vec![receiver(1, 0, None)]);
    assert_eq!(read("a=fmtp:49 LEVEL-ID=120\r\n"), vec![receiver(1, 0, Some(120))]);
    // A level that is no number is none.
    assert_eq!(read("a=fmtp:49 level-id=x;tier-flag=0\r\n"), vec![receiver(1, 0, None)]);
}
