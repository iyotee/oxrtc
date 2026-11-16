use proptest::prelude::*;

use super::*;

fn spec(media: &str, encoding: &str, clock: u32, fmtp: Option<&str>) -> DepacketizerSpec {
    DepacketizerSpec {
        media: media.into(),
        encoding: encoding.into(),
        clock_rate: NonZeroU32::new(clock).unwrap(),
        channels: None,
        fmtp: fmtp.map(str::to_owned),
    }
}

fn rtp(seq: u16, ts: u32, marker: bool, pt: u8, payload: &[u8]) -> Vec<u8> {
    let mut p = vec![0x80, (u8::from(marker) << 7) | pt];
    p.extend_from_slice(&seq.to_be_bytes());
    p.extend_from_slice(&ts.to_be_bytes());
    p.extend_from_slice(&[1, 2, 3, 4]);
    p.extend_from_slice(payload);
    p
}

#[test]
fn pcma_frames_come_out_one_per_packet() {
    let mut d = TrackDepacketizer::new(spec("audio", "pcma", 8000, None), Codec::Pcma, 1 << 20).unwrap();
    let mut out = Vec::new();
    d.push(&rtp(1, 1000, false, 8, &[0xd5; 160]), &mut out).unwrap();
    d.push(&rtp(2, 1160, false, 8, &[0xd5; 160]), &mut out).unwrap();
    assert_eq!(out.len(), 2);
    assert_eq!((out[0].pts, out[1].pts), (0, 160));
    assert_eq!(out[1].data.len(), 160);
}

#[test]
fn oversized_frame_in_progress_is_dropped() {
    let mut d = TrackDepacketizer::new(spec("video", "h264", 90_000, None), Codec::H264, 4000).unwrap();
    let mut out = Vec::new();
    // FU-A start then middles, never an end, at one timestamp.
    let mut result = Ok(());
    for seq in 0..10u16 {
        let fu = if seq == 0 { 0x85 } else { 0x05 };
        let mut payload = vec![0x7c, fu];
        payload.extend(std::iter::repeat_n(0x11, 1000));
        result = result.and(d.push(&rtp(seq, 0, false, 96, &payload), &mut out));
    }
    assert!(result.is_err());
    assert!(out.is_empty());
    assert!(d.pending <= 4000);
}

/// What a camera that never ends an access unit costs: the pieces stop being
/// kept at the cap (`pending` never passes it, the memory is the cap's, not
/// the camera's), the loss is reported once, and the next access unit comes
/// out whole, with nothing of the one that was cut.
#[test]
fn an_access_unit_cut_at_the_cap_leaves_the_next_one_whole() {
    const CAP: usize = 4000;
    let mut d = TrackDepacketizer::new(spec("video", "h264", 90_000, None), Codec::H264, CAP).unwrap();
    let mut out = Vec::new();
    let mut refusals = 0;
    let mut peak = 0;
    // 200 fragments of 1000 bytes at one timestamp: 200 kB offered to a 4 kB cap.
    for seq in 0..200u16 {
        let mut payload = vec![0x7c, if seq == 0 { 0x85 } else { 0x05 }];
        payload.extend(std::iter::repeat_n(0x11, 1000));
        refusals += usize::from(d.push(&rtp(seq, 0, false, 96, &payload), &mut out).is_err());
        peak = peak.max(d.pending);
    }
    assert!(out.is_empty(), "nothing of the cut access unit comes out");
    assert!(refusals >= 1, "the cut is reported");
    assert!(peak <= CAP, "{peak} bytes kept for a cap of {CAP}");
    // The next picture, at another timestamp.
    d.push(&rtp(200, 3000, true, 96, &[0x65, 0x88, 0x84, 0x00, 0x33]), &mut out).unwrap();
    assert_eq!(out.len(), 1);
    assert!(out[0].keyframe);
    assert_eq!(&out[0].data[..], &[0, 0, 0, 5, 0x65, 0x88, 0x84, 0x00, 0x33][..]);
}

/// An access unit that fits the cap is not lost because the first packet of
/// the next one arrives: a camera that sets no marker bit ends a unit only
/// when the next timestamp shows, and that packet is counted for the new unit,
/// not added to the one it ends (R11 review).
#[test]
fn a_unit_just_under_the_cap_survives_the_packet_that_ends_it() {
    const CAP: usize = 3000;
    let mut d = TrackDepacketizer::new(spec("video", "h264", 90_000, None), Codec::H264, CAP).unwrap();
    let mut out = Vec::new();
    let fragment = |first: bool, last: bool| {
        let mut payload = vec![0x7c, (u8::from(first) << 7) | (u8::from(last) << 6) | 0x05];
        payload.extend(std::iter::repeat_n(0x11, 998));
        payload
    };
    // Three packets of 1,000 bytes: the cap exactly, no marker.
    d.push(&rtp(0, 0, false, 96, &fragment(true, false)), &mut out).unwrap();
    d.push(&rtp(1, 0, false, 96, &fragment(false, false)), &mut out).unwrap();
    d.push(&rtp(2, 0, false, 96, &fragment(false, true)), &mut out).unwrap();
    assert!(out.is_empty(), "no marker, no frame yet");
    // The next unit's first packet closes the first. 3,000 + 1,000 over a 3,000 cap if it were added to the first.
    let mut next = vec![0x7c, 0x85];
    next.extend(std::iter::repeat_n(0x22, 998));
    let result = d.push(&rtp(3, 3000, false, 96, &next), &mut out);
    assert!(result.is_ok(), "{result:?}");
    assert_eq!(out.len(), 1, "the unit that fit comes out");
    assert_eq!(d.pending, 1000, "the packet that ended the unit opens the next");
}

/// Found by the `depacketizers_take_plausible_packets` property: retina
/// sums the losses of a run of packets in a `u16` (`pkts += meta.loss`,
/// then `meta.loss + additional_loss`), so a few large sequence gaps at
/// one timestamp overflowed it: a panic in a debug build and under a
/// fuzzer, a wrapped count in release.
#[test]
fn large_sequence_gaps_never_overflow_the_loss_count() {
    let runs: [(&str, Codec, &[u8], &[u8]); 2] =
        [("h264", Codec::H264, &[0x65], &[0x7c, 0x05]), ("h265", Codec::H265, &[0x26, 0x01], &[0x62, 0x01, 0x13])];
    for (encoding, codec, single, fragment) in runs {
        let mut d = TrackDepacketizer::new(spec("video", encoding, 90_000, None), codec, 1 << 20).unwrap();
        let mut out = Vec::new();
        // The shrunk case, then a longer run of gaps just under half the
        // sequence space, all at one timestamp.
        for (seq, ts, payload) in [(0u16, 0u32, single), (22424, 9000, fragment), (34320, 9000, single)] {
            let _ = d.push(&rtp(seq, ts, false, 96, payload), &mut out);
        }
        let mut seq = 984u16;
        for _ in 0..8 {
            let _ = d.push(&rtp(seq, 9000, false, 96, fragment), &mut out);
            seq = seq.wrapping_add(0x7fff);
        }
        let _ = d.push(&rtp(seq, 0, false, 96, single), &mut out);
    }
}

/// The cap holds for what comes out, not only for what went in: an
/// access unit of one-byte NAL units is five times larger in AVCC (a
/// four-byte length each) than the payloads that carried it.
#[test]
fn a_frame_over_the_cap_never_comes_out() {
    let mut d = TrackDepacketizer::new(spec("video", "h264", 90_000, None), Codec::H264, 4096).unwrap();
    let mut out = Vec::new();
    let mut refused = false;
    for seq in 0..3001u16 {
        let ts = if seq < 3000 { 0 } else { 3000 };
        refused |= d.push(&rtp(seq, ts, false, 96, &[0x41]), &mut out).is_err();
    }
    assert!(out.iter().all(|f| f.data.len() <= 4096), "{:?}", out.iter().map(|f| f.data.len()).collect::<Vec<_>>());
    assert!(refused);
}

/// x264 at `-level:v 4.1` (go2rtc's H.264 template) encoding 2304x1296:
/// 11664 macroblocks over level 4.1's 8192. h264-reader 0.9.0 refused the
/// SPS, from the SDP and in band, and no frame ever came out: the bench's
/// B03 had no picture. The patched copy (third_party/README.md) reads it.
#[test]
fn an_sps_over_its_declared_level_is_read() {
    const SPS: [u8; 24] = [
        0x67, 0x64, 0x00, 0x29, 0xac, 0xb4, 0x01, 0x20, 0x05, 0x1d, 0x80, 0x88, 0x00, 0x00, 0x03, 0x00, 0x38, 0x00,
        0x00, 0x06, 0x44, 0x78, 0xc1, 0x95,
    ];
    const PPS: [u8; 4] = [0x68, 0xef, 0x0f, 0xcb];
    let fmtp =
        "packetization-mode=1;sprop-parameter-sets=Z2QAKay0ASAFHYCIAAADADgAAAZEeMGV,aO8Pyw==;profile-level-id=640029";
    let mut d = TrackDepacketizer::new(spec("video", "h264", 90_000, Some(fmtp)), Codec::H264, 1 << 20).unwrap();
    assert!(matches!(d.params(), CodecParams::H264 { ref sps, .. } if sps[..] == SPS[..]), "{:?}", d.params());
    let mut out = Vec::new();
    d.push(&rtp(0, 0, false, 96, &SPS), &mut out).unwrap();
    d.push(&rtp(1, 0, false, 96, &PPS), &mut out).unwrap();
    d.push(&rtp(2, 0, true, 96, &[0x65, 0x88, 0x84, 0x00, 0x33]), &mut out).unwrap();
    assert_eq!(out.len(), 1);
    assert!(out[0].keyframe);
}

#[test]
fn malformed_packets_are_errors() {
    let mut d = TrackDepacketizer::new(spec("video", "h264", 90_000, None), Codec::H264, 1 << 20).unwrap();
    let mut out = Vec::new();
    assert!(d.push(&[1, 2, 3], &mut out).is_err());
    assert!(d.push(&[0x40; 20], &mut out).is_err());
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(500))]

    /// Arbitrary payloads behind valid RTP headers never panic any of the
    /// depacketizers a publication can use.
    #[test]
    fn random_payloads_never_panic(
        packets in proptest::collection::vec(
            (any::<u16>(), any::<u32>(), any::<bool>(), proptest::collection::vec(any::<u8>(), 0..1500)),
            1..40,
        ),
        which in 0usize..5,
    ) {
        let (media, encoding, clock, codec) = [
            ("video", "h264", 90_000, Codec::H264),
            ("video", "h265", 90_000, Codec::H265),
            ("audio", "opus", 48_000, Codec::Opus),
            ("audio", "pcmu", 8000, Codec::Pcmu),
            ("audio", "mpeg4-generic", 16_000, Codec::Aac),
        ][which];
        let fmtp = (codec == Codec::Aac)
            .then_some("streamtype=5;profile-level-id=1;mode=AAC-hbr;sizelength=13;indexlength=3;indexdeltalength=3;config=1408");
        let mut d = TrackDepacketizer::new(spec(media, encoding, clock, fmtp), codec, 1 << 16).unwrap();
        let mut out = Vec::new();
        for (seq, ts, marker, payload) in packets {
            let _ = d.push(&rtp(seq, ts, marker, 96, &payload), &mut out);
        }
    }
}
