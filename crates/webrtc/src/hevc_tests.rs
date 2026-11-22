use bytes::Bytes;
use oxrtc_core::{Codec, CodecParams, TrackInfo};

use super::*;

fn hex(s: &str) -> Bytes {
    Bytes::from((0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap()).collect::<Vec<u8>>())
}

/// libx265's SPS of a 1920x1080 Main stream at level 4.
const MAIN_L4: &str = "420101016000000300900000030000030078a003c08010e596566924caf0168080000003008000000c84";

fn h265(sps: &str) -> TrackInfo {
    TrackInfo::video(Codec::H265).with_params(CodecParams::H265 {
        vps: Bytes::from_static(&[0x40, 0x01]),
        sps: hex(sps),
        pps: Bytes::from_static(&[0x44, 0x01]),
    })
}

const CHROME: [HevcPtl; 2] = [HevcPtl { profile: 1, tier: 0, level: 180 }, HevcPtl { profile: 2, tier: 0, level: 180 }];

#[test]
fn a_track_is_read_from_its_sps() {
    assert_eq!(HevcPtl::of_track(&h265(MAIN_L4)), Some(HevcPtl { profile: 1, tier: 0, level: 120 }));
    // An SPS not known yet, a track of another codec, a cut SPS.
    assert_eq!(HevcPtl::of_track(&TrackInfo::video(Codec::H265)), None);
    assert_eq!(HevcPtl::of_track(&TrackInfo::video(Codec::H264)), None);
    assert_eq!(HevcPtl::of_track(&h265("42010101600000")), None);
}

#[test]
fn a_receiver_takes_its_profile_and_tier_up_to_its_level() {
    let main6 = CHROME[0];
    assert!(main6.takes(HevcPtl { profile: 1, tier: 0, level: 150 }), "the owner's XM cameras: Main, level 5.0");
    assert!(main6.takes(HevcPtl { profile: 1, tier: 0, level: 180 }));
    assert!(!main6.takes(HevcPtl { profile: 1, tier: 0, level: 183 }), "above the receiver's level");
    assert!(!main6.takes(HevcPtl { profile: 2, tier: 0, level: 93 }), "Main 10 is not Main");
    assert!(!main6.takes(HevcPtl { profile: 1, tier: 1, level: 93 }), "the High tier is not the Main tier");
}

#[test]
fn the_first_track_no_payload_type_takes_is_refused() {
    let tracks = [TrackInfo::audio(Codec::Opus, 48_000, 2), h265(MAIN_L4)];
    assert_eq!(refused(&CHROME, &tracks), None);
    let low = [HevcPtl { profile: 1, tier: 0, level: 93 }];
    assert_eq!(refused(&low, &tracks), Some(HevcPtl { profile: 1, tier: 0, level: 120 }));
    // An H.264 track, or an H.265 one with no SPS yet, is never refused.
    assert_eq!(refused(&low, &[TrackInfo::video(Codec::H264), TrackInfo::video(Codec::H265)]), None);
    // An offer with no H.265 at all is not asked: the core never picked H.265 for it.
    assert_eq!(refused(&[], &tracks), Some(HevcPtl { profile: 1, tier: 0, level: 120 }));
}

#[test]
fn only_the_standard_numbers_go_to_str0m() {
    assert!(HevcPtl { profile: 1, tier: 0, level: 150 }.is_standard());
    assert!(HevcPtl { profile: 2, tier: 1, level: 186 }.is_standard());
    assert!(!HevcPtl { profile: 1, tier: 0, level: 151 }.is_standard(), "no level 5.03");
    assert!(!HevcPtl { profile: 0, tier: 0, level: 93 }.is_standard());
    assert!(!HevcPtl { profile: 1, tier: 2, level: 93 }.is_standard());
}

#[test]
fn an_offer_s_fmtp_is_read_as_str0m_reads_it() {
    use str0m::change::SdpOffer;
    let sdp = |fmtp: &str| {
        format!(
            "v=0\r\no=- 1 2 IN IP4 127.0.0.1\r\ns=-\r\nt=0 0\r\na=group:BUNDLE 0\r\n\
             m=video 9 UDP/TLS/RTP/SAVPF 49\r\nc=IN IP4 0.0.0.0\r\na=ice-ufrag:abcd\r\na=ice-pwd:abcdefghijklmnopqrstuvwx\r\n\
             a=fingerprint:sha-256 DA:1D:6F:73:9E:94:51:25:3A:10:F8:DE:0D:4B:39:C8:6C:0A:16:2B:E8:EB:65:13:2B:CB:AB:71:06:DF:C1:01\r\n\
             a=setup:actpass\r\na=mid:0\r\na=recvonly\r\na=rtcp-mux\r\na=rtpmap:49 H265/90000\r\n{fmtp}"
        )
    };
    let read = |fmtp: &str| {
        let offer = SdpOffer::from_sdp_string(&sdp(fmtp)).unwrap();
        let line = offer.media_lines.first().unwrap();
        line.rtp_params().iter().map(HevcPtl::offered).collect::<Vec<_>>()
    };
    let chrome = "a=fmtp:49 level-id=180;profile-id=1;tier-flag=0;tx-mode=SRST\r\n";
    assert_eq!(read(chrome), vec![HevcPtl { profile: 1, tier: 0, level: 180 }]);
    // No fmtp: RFC 7798's Main, Main tier, level 3.1.
    assert_eq!(read(""), vec![RFC_DEFAULT]);
    // A lone profile-id keeps the RFC's tier and level.
    assert_eq!(read("a=fmtp:49 profile-id=2\r\n"), vec![HevcPtl { profile: 2, ..RFC_DEFAULT }]);
}
