//! Packetizer tests: wire format, and round trips through retina's
//! depacketizers (the ones the publication side uses).

use std::num::NonZeroU32;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use bytes::Bytes;
use oxrtc_core::frame::{NalUnits, h264_nal_type};
use proptest::prelude::*;

use super::*;
use crate::depacketize::{Depacketized, DepacketizerSpec, TrackDepacketizer};

const H264_FMTP: &str = "packetization-mode=1;sprop-parameter-sets=Z2QAHqwsaoLA9puCgIKgAAADACAAAAMD0IAA,aO4xshsA";
const H265_SPS: &str = "QgEBAWAAAAMAsAAAAwAAAwBaoAWCAeFja5JFL83BQYFBAAADAAEAAAMADKE=";
const H265_PPS: &str = "RAHA8saNA7NA";
const H265_VPS: &str = "QAEMAf//AWAAAAMAsAAAAwAAAwBarAwAAAMABAAAAwAyqA==";

fn b64(s: &str) -> Bytes {
    Bytes::from(STANDARD.decode(s).unwrap())
}

fn h264_params() -> CodecParams {
    let (sps, pps) = H264_FMTP.rsplit_once('=').unwrap().1.split_once(',').unwrap();
    CodecParams::H264 { sps: b64(sps), pps: b64(pps) }
}

fn h265_params() -> CodecParams {
    CodecParams::H265 { vps: b64(H265_VPS), sps: b64(H265_SPS), pps: b64(H265_PPS) }
}

fn avcc(nals: &[Vec<u8>]) -> Bytes {
    let mut out = Vec::new();
    for nal in nals {
        out.extend_from_slice(&(nal.len() as u32).to_be_bytes());
        out.extend_from_slice(nal);
    }
    Bytes::from(out)
}

/// A NAL of `len` bytes after its header, avoiding start-code-like runs.
fn nal(header: &[u8], len: usize) -> Vec<u8> {
    let mut n = header.to_vec();
    n.extend((0..len).map(|i| 0x10 + (i % 200) as u8));
    n
}

fn split(mut wire: &[u8]) -> Vec<(u8, Vec<u8>)> {
    let mut out = Vec::new();
    while let [b'$', channel, hi, lo, rest @ ..] = wire {
        let len = usize::from(u16::from_be_bytes([*hi, *lo]));
        out.push((*channel, rest[..len].to_vec()));
        wire = &rest[len..];
    }
    assert!(wire.is_empty(), "trailing bytes");
    out
}

fn roundtrip(packets: &[(u8, Vec<u8>)], spec: DepacketizerSpec, codec: Codec) -> Vec<Depacketized> {
    let mut d = TrackDepacketizer::new(spec, codec, 1 << 22).unwrap();
    let mut out = Vec::new();
    for (_, packet) in packets {
        d.push(packet, &mut out).unwrap();
    }
    out
}

fn video_spec(encoding: &str, fmtp: Option<String>) -> DepacketizerSpec {
    DepacketizerSpec {
        media: "video".into(),
        encoding: encoding.into(),
        clock_rate: NonZeroU32::new(90_000).unwrap(),
        channels: None,
        fmtp,
    }
}

fn vcl(data: &[u8], keep: impl Fn(&[u8]) -> bool) -> Vec<Vec<u8>> {
    NalUnits::new(data).filter(|n| keep(n)).map(<[u8]>::to_vec).collect()
}

#[test]
fn h264_keyframe_gets_parameter_sets_and_fragments() {
    let track = TrackInfo::video(Codec::H264);
    let mut p = Packetizer::new(&track, 96, 0);
    let idr = nal(&[0x65], 5000);
    let frame = Frame::new(0, 3600, true, avcc(std::slice::from_ref(&idr)));
    let mut out = BytesMut::new();
    let count = p.packetize(&frame, &h264_params(), &mut out);
    let packets = split(&out);
    assert_eq!(count, packets.len());
    assert!(packets.iter().all(|(ch, pkt)| *ch == 0 && pkt.len() <= RTP_HEADER + MAX_PAYLOAD));
    // SPS, PPS, then FU-A pieces; marker on the last packet only.
    assert_eq!(h264_nal_type(&packets[0].1[12..]), Some(7));
    assert_eq!(h264_nal_type(&packets[1].1[12..]), Some(8));
    assert_eq!(packets[2].1[12] & 0x1f, FU_A);
    let markers: Vec<bool> = packets.iter().map(|(_, pkt)| pkt[1] & 0x80 != 0).collect();
    assert_eq!(markers.iter().filter(|m| **m).count(), 1);
    assert_eq!(markers.last(), Some(&true));
    // Consecutive sequence numbers.
    let seqs: Vec<u16> = packets.iter().map(|(_, pkt)| u16::from_be_bytes([pkt[2], pkt[3]])).collect();
    assert!(seqs.windows(2).all(|w| w[1] == w[0].wrapping_add(1)));
    assert_eq!(seqs[0], p.initial_seq());
    let frames = roundtrip(&packets, video_spec("h264", Some(H264_FMTP.into())), Codec::H264);
    assert_eq!(frames.len(), 1);
    assert!(frames[0].keyframe);
    assert_eq!(vcl(&frames[0].data, |n| h264_nal_type(n) == Some(5)), vec![idr]);
}

#[test]
fn h264_parameter_sets_carried_in_band_are_not_repeated() {
    let mut p = Packetizer::new(&TrackInfo::video(Codec::H264), 96, 2);
    let CodecParams::H264 { sps, pps } = h264_params() else { unreachable!() };
    let frame = Frame::new(0, 0, true, avcc(&[sps.to_vec(), pps.to_vec(), nal(&[0x65], 100)]));
    let mut out = BytesMut::new();
    p.packetize(&frame, &h264_params(), &mut out);
    let packets = split(&out);
    assert_eq!(packets.len(), 3);
    assert!(packets.iter().all(|(ch, _)| *ch == 2));
}

#[test]
fn h265_keyframe_roundtrips_through_fu() {
    let mut p = Packetizer::new(&TrackInfo::video(Codec::H265), 96, 0);
    let idr = nal(&[0x26, 0x01], 4000); // IDR_W_RADL
    let frame = Frame::new(0, 0, true, avcc(std::slice::from_ref(&idr)));
    let mut out = BytesMut::new();
    p.packetize(&frame, &h265_params(), &mut out);
    let packets = split(&out);
    assert_eq!(packets.len(), 3 + 4000_usize.div_ceil(MAX_PAYLOAD - 3));
    assert_eq!((packets[3].1[12] >> 1) & 0x3f, H265_FU);
    let fmtp = format!("sprop-vps={H265_VPS};sprop-sps={H265_SPS};sprop-pps={H265_PPS}");
    let frames = roundtrip(&packets, video_spec("h265", Some(fmtp)), Codec::H265);
    assert_eq!(frames.len(), 1);
    assert!(frames[0].keyframe);
    assert_eq!(vcl(&frames[0].data, |n| oxrtc_core::frame::h265_nal_type(n) == Some(19)), vec![idr]);
}

#[test]
fn aac_gets_one_au_header() {
    let mut p = Packetizer::new(&TrackInfo::audio(Codec::Aac, 16_000, 1), 97, 2);
    let mut out = BytesMut::new();
    p.packetize(&Frame::new(1, 1024, true, Bytes::from(vec![0xaa; 300])), &CodecParams::None, &mut out);
    let packets = split(&out);
    assert_eq!(packets.len(), 1);
    let payload = &packets[0].1[12..];
    assert_eq!(&payload[..4], &[0x00, 0x10, (300u16 >> 5) as u8, ((300u16 & 0x1f) << 3) as u8]);
    assert_eq!(payload.len(), 304);
    // Too large for a 13-bit size: dropped, not truncated.
    let mut out = BytesMut::new();
    assert_eq!(p.packetize(&Frame::new(1, 0, true, Bytes::from(vec![0; 9000])), &CodecParams::None, &mut out), 0);
}

#[test]
fn g711_splits_on_sample_bounds_with_advancing_timestamps() {
    let track = TrackInfo::audio(Codec::Pcma, 8000, 1);
    assert_eq!(payload_type(1, &track), 8);
    let mut p = Packetizer::new(&track, 8, 2);
    let mut out = BytesMut::new();
    p.packetize(&Frame::new(1, 0, true, Bytes::from(vec![0xd5; 3000])), &CodecParams::None, &mut out);
    let packets = split(&out);
    let sizes: Vec<usize> = packets.iter().map(|(_, p)| p.len() - 12).collect();
    assert_eq!(sizes, vec![1200, 1200, 600]);
    let ts: Vec<u32> = packets.iter().map(|(_, p)| u32::from_be_bytes([p[4], p[5], p[6], p[7]])).collect();
    assert_eq!(ts[1].wrapping_sub(ts[0]), 1200);
    assert_eq!(ts[2].wrapping_sub(ts[0]), 2400);
}

#[test]
fn l16_stereo_never_splits_a_sample() {
    let mut p = Packetizer::new(&TrackInfo::audio(Codec::L16, 48_000, 2), 97, 2);
    let mut out = BytesMut::new();
    p.packetize(&Frame::new(1, 0, true, Bytes::from(vec![0; 4000])), &CodecParams::None, &mut out);
    assert!(split(&out).iter().all(|(_, p)| (p.len() - 12) % 4 == 0));
}

#[test]
fn sender_report_follows_the_first_packet() {
    let mut p = Packetizer::new(&TrackInfo::audio(Codec::Opus, 48_000, 2), 97, 4);
    let mut out = BytesMut::new();
    p.sender_report(SystemTime::now(), &mut out);
    assert!(out.is_empty(), "no report before any packet");
    p.packetize(&Frame::new(1, 960, true, Bytes::from_static(&[1, 2, 3])), &CodecParams::None, &mut out);
    let mut sr = BytesMut::new();
    p.sender_report(SystemTime::now(), &mut sr);
    let packets = split(&sr);
    assert_eq!(packets.len(), 1);
    assert_eq!(packets[0].0, 5);
    assert_eq!(&packets[0].1[..4], &[0x80, 200, 0, 6]);
    assert_eq!(u32::from_be_bytes(packets[0].1[4..8].try_into().unwrap()), p.ssrc());
    assert_eq!(u32::from_be_bytes(packets[0].1[20..24].try_into().unwrap()), 1, "packet count");
}

#[test]
fn sender_reports_go_to_the_rtcp_channel_the_client_chose() {
    let mut p = Packetizer::new(&TrackInfo::audio(Codec::Opus, 48_000, 2), 97, 4).with_rtcp_channel(7);
    let mut out = BytesMut::new();
    p.packetize(&Frame::new(1, 960, true, Bytes::from_static(&[1, 2, 3])), &CodecParams::None, &mut out);
    assert_eq!(split(&out)[0].0, 4, "RTP on its channel");
    let mut sr = BytesMut::new();
    p.sender_report(SystemTime::now(), &mut sr);
    assert_eq!(split(&sr)[0].0, 7, "RTCP on the chosen channel, not RTP + 1");
}

#[test]
fn payload_types_are_dynamic_except_8khz_g711() {
    assert_eq!(payload_type(0, &TrackInfo::video(Codec::H264)), 96);
    assert_eq!(payload_type(1, &TrackInfo::audio(Codec::Pcmu, 8000, 1)), 0);
    assert_eq!(payload_type(1, &TrackInfo::audio(Codec::Pcma, 16_000, 1)), 97);
    assert_eq!(payload_type(200, &TrackInfo::audio(Codec::Opus, 48_000, 2)), 127);
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// Any frame of non-IDR slices survives packetization and retina's
    /// depacketization unchanged, with every packet within the MTU budget.
    #[test]
    fn h264_frames_roundtrip(sizes in proptest::collection::vec(1usize..6000, 1..5), pts in 0i64..1_000_000) {
        let nals: Vec<Vec<u8>> = sizes.iter().map(|&n| nal(&[0x41], n)).collect();
        let mut p = Packetizer::new(&TrackInfo::video(Codec::H264), 96, 0);
        let mut out = BytesMut::new();
        p.packetize(&Frame::new(0, pts, false, avcc(&nals)), &h264_params(), &mut out);
        let packets = split(&out);
        prop_assert!(packets.iter().all(|(_, pkt)| pkt.len() <= RTP_HEADER + MAX_PAYLOAD));
        let frames = roundtrip(&packets, video_spec("h264", Some(H264_FMTP.into())), Codec::H264);
        prop_assert_eq!(frames.len(), 1);
        prop_assert_eq!(vcl(&frames[0].data, |n| h264_nal_type(n) == Some(1)), nals);
    }
}
