//! The JPEG reader, the RFC 2435 fragments, and the packets the server writes
//! for a JPEG, read back by retina's RTP/JPEG depacketizer.

use std::num::NonZeroU32;

use bytes::{Bytes, BytesMut};
use oxrtc_core::{Codec, CodecParams, Frame, TrackInfo};
use proptest::prelude::*;

use super::*;
use crate::huffman::Table;
use crate::packetize::{MAX_PAYLOAD, Packetizer, payload_type};
use oxrtc_depacketize::{DepacketizerSpec, TrackDepacketizer};

/// Real files, 4:2:0 or 4:2:2 as libjpeg writes them (Pillow: one DHT per
/// table, two quantization tables, a restart interval) and as FFmpeg's
/// encoder does (one quantization table, one DHT of the four).
const PIL_420: &[u8] = include_bytes!("../tests/data/pil420.jpg");
const PIL_422: &[u8] = include_bytes!("../tests/data/pil422.jpg");
const PIL_420_RESTART: &[u8] = include_bytes!("../tests/data/pil420_restart.jpg");
const YUV420: &[u8] = include_bytes!("../tests/data/yuvj420p.jpg");
/// What RTP/JPEG cannot carry: FFmpeg's 4:2:2 (Y at 2x2, chroma at 1x2, a
/// different MCU from RFC 2435's type 0) and full-resolution chroma.
const YUV422: &[u8] = include_bytes!("../tests/data/yuvj422p.jpg");
const YUV444: &[u8] = include_bytes!("../tests/data/yuvj444p.jpg");

pub(crate) fn segment(code: u8, body: &[u8]) -> Vec<u8> {
    let mut out = vec![0xff, code];
    out.extend_from_slice(&(u16::try_from(body.len() + 2).unwrap()).to_be_bytes());
    out.extend_from_slice(body);
    out
}

pub(crate) fn dht(info: u8, table: &[u8]) -> Vec<u8> {
    segment(0xc4, &[&[info][..], table].concat())
}

/// A JPEG made of the parts a test wants to vary. Its tables are the
/// standard ones; its scan is `scan`, with `0xff` stuffed.
pub(crate) struct Build {
    pub(crate) width: u16,
    pub(crate) height: u16,
    pub(crate) sampling: [u8; 3],
    pub(crate) quant_tables: Vec<(u8, u8)>,
    pub(crate) precision: u8,
    pub(crate) sof: u8,
    pub(crate) restart: Option<u16>,
    pub(crate) scan: Vec<u8>,
    /// The ids the scan names its components by, and their `Td << 4 | Ta`.
    pub(crate) scan_ids: [u8; 3],
    pub(crate) selectors: [u8; 3],
    /// The DHT tables, by `Tc << 4 | Th`: the standard ones unless replaced.
    pub(crate) huffman: Vec<(u8, Vec<u8>)>,
}

impl Build {
    pub(crate) fn new(width: u16, height: u16, scan_len: usize) -> Self {
        Self {
            width,
            height,
            sampling: [0x22, 0x11, 0x11],
            quant_tables: vec![(0, 0), (1, 0)],
            precision: 8,
            sof: 0xc0,
            restart: None,
            scan: (0..scan_len).map(|i| (i % 251) as u8).collect(),
            scan_ids: [1, 2, 3],
            selectors: [0x00, 0x11, 0x11],
            huffman: [(0x00, false, false), (0x10, true, false), (0x01, false, true), (0x11, true, true)]
                .map(|(info, ac, chroma)| (info, Table::standard(ac, chroma).dht_body()))
                .to_vec(),
        }
    }

    pub(crate) fn bytes(&self) -> Vec<u8> {
        let mut out = vec![0xff, 0xd8];
        for (id, precision) in &self.quant_tables {
            let mut body = vec![(precision << 4) | id];
            let size = if *precision == 0 { 64 } else { 128 };
            body.extend((0..size).map(|i| 1 + (i as u8).wrapping_add(id * 40)));
            out.extend(segment(0xdb, &body));
        }
        if let Some(interval) = self.restart {
            out.extend(segment(0xdd, &interval.to_be_bytes()));
        }
        let mut sof = vec![self.precision];
        sof.extend_from_slice(&self.height.to_be_bytes());
        sof.extend_from_slice(&self.width.to_be_bytes());
        sof.push(3);
        for (index, sampling) in self.sampling.iter().enumerate() {
            sof.extend_from_slice(&[index as u8 + 1, *sampling, u8::from(index > 0)]);
        }
        out.extend(segment(self.sof, &sof));
        for (info, body) in &self.huffman {
            out.extend(dht(*info, body));
        }
        let [a, b, c] = self.scan_ids;
        let [x, y, z] = self.selectors;
        out.extend(segment(0xda, &[3, a, x, b, y, c, z, 0, 63, 0]));
        out.extend_from_slice(&self.scan);
        out.extend_from_slice(&[0xff, 0xd9]);
        out
    }
}

#[test]
fn real_files_with_the_standard_tables_are_read() {
    // The constants of this module are those both encoders write.
    let jpeg = parse(YUV420).unwrap();
    assert_eq!((jpeg.width, jpeg.height, jpeg.kind, jpeg.restart), (48, 32, 1, None));
    assert_eq!(&YUV420[YUV420.len() - jpeg.scan.len() - 2..][..jpeg.scan.len()], &*jpeg.scan);
    assert_eq!(&YUV420[YUV420.len() - 2..], &[0xff, 0xd9]);
    // FFmpeg writes one quantization table for both: the chroma one is the same.
    assert_eq!(jpeg.tables.get(..64), jpeg.tables.get(64..));
    let jpeg = parse(PIL_420).unwrap();
    assert_eq!((jpeg.width, jpeg.height, jpeg.kind, jpeg.restart), (64, 48, 1, None));
    assert_ne!(jpeg.tables.get(..64), jpeg.tables.get(64..), "two tables");
    let jpeg = parse(PIL_422).unwrap();
    assert_eq!((jpeg.width, jpeg.height, jpeg.kind), (64, 48, 0));
    let jpeg = parse(PIL_420_RESTART).unwrap();
    assert_eq!((jpeg.kind, jpeg.restart), (65, Some(4)));
}

#[test]
fn what_rtp_jpeg_cannot_carry_is_refused_with_its_reason() {
    for (data, why) in [
        (YUV444, "chroma layout"),
        (YUV422, "chroma layout"),
        (b"\xff\xd8".as_slice(), "marker was expected"),
        (b"GIF89a".as_slice(), "start-of-image"),
        (b"".as_slice(), "start-of-image"),
    ] {
        let error = parse(data).unwrap_err().to_string();
        assert!(error.contains(why), "{error} does not say {why:?}");
    }
    let mut big = Build::new(2048, 1000, 100);
    assert!(parse(&big.bytes()).unwrap_err().to_string().contains("2040"));
    big.width = 2040;
    assert_eq!(parse(&big.bytes()).unwrap().width, 2040);
    big.width = 0;
    assert!(matches!(parse(&big.bytes()), Err(JpegError::Malformed(_))));
}

#[test]
fn each_unsupported_variant_is_named() {
    let mut progressive = Build::new(64, 48, 100);
    progressive.sof = 0xc2;
    let mut sixteen = Build::new(64, 48, 100);
    sixteen.quant_tables = vec![(0, 1), (1, 0)];
    let mut twelve = Build::new(64, 48, 100);
    twelve.precision = 12;
    let mut tall_luma = Build::new(64, 48, 100);
    tall_luma.sampling = [0x12, 0x11, 0x11];
    let mut odd_chroma = Build::new(64, 48, 100);
    odd_chroma.sampling = [0x22, 0x21, 0x11];
    let mut third_table = Build::new(64, 48, 100);
    third_table.quant_tables = vec![(0, 0), (1, 0), (3, 0)];
    for (jpeg, why) in [
        (progressive, "baseline"),
        (sixteen, "16-bit"),
        (twelve, "8 bits"),
        (tall_luma, "chroma layout"),
        (odd_chroma, "chroma layout"),
    ] {
        let error = parse(&jpeg.bytes()).unwrap_err();
        assert!(matches!(error, JpegError::Unsupported(_)) && error.to_string().contains(why), "{error}");
    }
    // A table the components do not use is no reason to refuse.
    assert!(parse(&third_table.bytes()).is_ok());
}

#[test]
fn what_follows_the_end_of_image_is_not_the_pictures() {
    // Cameras pad a picture (a line break, zeros) and a part's declared length
    // may cover the padding: none of it goes in the packets.
    let clean = Build::new(64, 48, 100).bytes();
    for padding in [
        &b"
"[..],
        &[0, 0, 0],
        &[0xff, 0xd9, 1, 2],
    ] {
        let padded = [&clean[..], padding].concat();
        assert_eq!(parse(&padded).unwrap().scan, parse(&clean).unwrap().scan, "{padding:?}");
    }
    // No end of image at all: the scan is what there is.
    let cut = &clean[..clean.len() - 2];
    assert_eq!(parse(cut).unwrap().scan.len(), 100);
}

#[test]
fn the_restart_interval_makes_the_type_64_plus() {
    let mut build = Build::new(64, 48, 100);
    assert_eq!(parse(&build.bytes()).unwrap().kind, 1);
    build.restart = Some(7);
    let file = build.bytes();
    let jpeg = parse(&file).unwrap();
    assert_eq!((jpeg.kind, jpeg.restart), (65, Some(7)));
    build.restart = Some(0);
    assert_eq!(parse(&build.bytes()).unwrap().restart, None, "an interval of 0 is no interval");
    build.sampling = [0x21, 0x11, 0x11];
    build.restart = Some(3);
    assert_eq!(parse(&build.bytes()).unwrap().kind, 64);
}

#[test]
fn a_file_cut_anywhere_is_an_error_or_a_short_scan_never_a_panic() {
    for source in [YUV420, PIL_422, PIL_420_RESTART] {
        for end in 0..source.len() {
            let _ = parse(source.get(..end).unwrap());
        }
    }
}

#[test]
fn marker_padding_and_standalone_markers_are_skipped() {
    let plain = Build::new(64, 48, 100).bytes();
    // Fill bytes before a marker, a TEM and an APP0 and a comment.
    let mut padded = vec![0xff, 0xd8, 0xff, 0xff, 0xff, 0xfe, 0x00, 0x04, b'h', b'i', 0xff, 0x01];
    padded.extend_from_slice(&plain[2..]);
    assert_eq!(parse(&padded).unwrap().scan, parse(&plain).unwrap().scan);
}

#[test]
fn fragments_cover_the_scan_in_order_with_the_tables_in_the_first() {
    let jpeg_bytes = Build::new(640, 360, 5000).bytes();
    let jpeg = parse(&jpeg_bytes).unwrap();
    let fragments: Vec<_> = jpeg.fragments(MAX_PAYLOAD).collect();
    assert!(fragments.len() > 4);
    let mut offset = 0usize;
    for (index, fragment) in fragments.iter().enumerate() {
        let header = fragment.header();
        assert!(header.len() + fragment.data.len() <= MAX_PAYLOAD);
        assert_eq!(header[0], 0, "type-specific");
        assert_eq!(usize::from(header[1]) << 16 | usize::from(header[2]) << 8 | usize::from(header[3]), offset);
        assert_eq!((header[4], header[5], header[6], header[7]), (1, 255, 80, 45));
        if index == 0 {
            assert_eq!(&header[8..12], &[0, 0, 0, 128], "MBZ, precision 0, length 128");
            assert_eq!(&header[12..], &jpeg.tables[..]);
        } else {
            assert_eq!(header.len(), 8, "no table after the first");
        }
        assert_eq!(fragment.data, &jpeg.scan[offset..offset + fragment.data.len()]);
        offset += fragment.data.len();
        assert_eq!(fragment.last, index + 1 == fragments.len());
    }
    assert_eq!(offset, jpeg.scan.len());
}

#[test]
fn a_restart_interval_is_repeated_in_every_packet() {
    let mut build = Build::new(64, 48, 3000);
    build.restart = Some(0x0102);
    let jpeg_bytes = build.bytes();
    let jpeg = parse(&jpeg_bytes).unwrap();
    let fragments: Vec<_> = jpeg.fragments(MAX_PAYLOAD).collect();
    assert!(fragments.len() >= 3);
    for fragment in &fragments {
        assert_eq!(fragment.header()[4], 65);
        assert_eq!(&fragment.header()[8..12], &[0x01, 0x02, 0xff, 0xff]);
    }
    assert_eq!(fragments[0].header().len(), 8 + 4 + 4 + 128);
    assert_eq!(fragments[1].header().len(), 8 + 4);
}

#[test]
fn sides_that_are_not_multiples_of_8_round_up() {
    let jpeg_bytes = Build::new(177, 99, 50).bytes();
    let jpeg = parse(&jpeg_bytes).unwrap();
    let header = jpeg.fragments(MAX_PAYLOAD).next().unwrap();
    assert_eq!((header.header()[6], header.header()[7]), (23, 13));
}

#[test]
fn no_room_for_the_headers_gives_no_packet() {
    let jpeg_bytes = Build::new(64, 48, 100).bytes();
    let jpeg = parse(&jpeg_bytes).unwrap();
    assert_eq!(jpeg.fragments(8 + 4 + 128).count(), 0);
    assert_eq!(jpeg.fragments(0).count(), 0);
    // One byte of scan after the first packet's headers, the rest after the 8-byte ones.
    assert_eq!(jpeg.fragments(8 + 4 + 128 + 1).count(), 2);
}

fn jpeg_spec() -> DepacketizerSpec {
    DepacketizerSpec {
        media: "video".into(),
        encoding: "jpeg".into(),
        clock_rate: NonZeroU32::new(90_000).unwrap(),
        channels: None,
        fmtp: None,
    }
}

/// The RTP packets of one frame, as the server writes them, read back by
/// retina's depacketizer: the JPEG files it rebuilds, and the packet count.
fn through_the_wire(track: &TrackInfo, data: &[u8]) -> (usize, Vec<Vec<u8>>) {
    let mut packetizer = Packetizer::new(track, payload_type(0, track), 0);
    let mut out = BytesMut::new();
    let frame = Frame::new(0, 9000, true, Bytes::copy_from_slice(data));
    let count = packetizer.packetize(&frame, &CodecParams::None, &mut out);
    let mut wire = out.as_ref();
    let mut depacketizer = TrackDepacketizer::new(jpeg_spec(), Codec::Jpeg, 1 << 22).unwrap();
    let mut frames = Vec::new();
    let mut markers = 0;
    while let [b'$', channel, hi, lo, rest @ ..] = wire {
        let (packet, after) = rest.split_at(usize::from(u16::from_be_bytes([*hi, *lo])));
        wire = after;
        if packet.get(1) == Some(&200) {
            continue;
        }
        assert_eq!(*channel, 0);
        assert_eq!(packet[1] & 0x7f, 26, "the static payload type of JPEG");
        assert!(packet.len() <= 12 + MAX_PAYLOAD);
        markers += usize::from(packet[1] & 0x80 != 0);
        depacketizer.push(packet, &mut frames).unwrap();
    }
    if count > 0 {
        assert_eq!(markers, 1, "the marker is on the last packet only");
    }
    (count, frames.into_iter().map(|f| f.data.to_vec()).collect())
}

#[test]
fn a_jpeg_frame_survives_the_wire_and_retinas_depacketizer() {
    let track = TrackInfo::video(Codec::Jpeg);
    for (file, width, height, kind) in
        [(YUV420, 48, 32, 1), (PIL_420, 64, 48, 1), (PIL_422, 64, 48, 0), (PIL_420_RESTART, 64, 48, 65)]
    {
        let sent = parse(file).unwrap();
        let (count, frames) = through_the_wire(&track, file);
        assert_eq!(count, sent.fragments(MAX_PAYLOAD).count());
        let [received] = frames.as_slice() else { panic!("{} frames", frames.len()) };
        let back = parse(received).unwrap();
        assert_eq!((back.width, back.height, back.kind), (width, height, kind));
        assert_eq!(back.scan, sent.scan, "the entropy-coded data is untouched");
        assert_eq!(back.tables, sent.tables, "so are the quantization tables");
    }
}

#[test]
fn a_big_jpeg_with_restarts_is_cut_and_put_back_together() {
    let track = TrackInfo::video(Codec::Jpeg);
    let mut build = Build::new(1920, 1080, 60_000);
    build.restart = Some(120);
    let file = build.bytes();
    let (count, frames) = through_the_wire(&track, &file);
    assert_eq!(count, parse(&file).unwrap().fragments(MAX_PAYLOAD).count());
    assert!(count > 50, "{count} packets");
    let [received] = frames.as_slice() else { panic!("{} frames", frames.len()) };
    let (sent, back) = (parse(&file).unwrap(), parse(received).unwrap());
    assert_eq!(back.scan, sent.scan);
    assert_eq!((back.kind, back.restart), (65, Some(120)));
    assert_eq!((back.width, back.height), (1920, 1080));
}

#[test]
fn a_jpeg_rtp_cannot_carry_is_dropped_without_a_report_and_the_next_one_goes() {
    let track = TrackInfo::video(Codec::Jpeg);
    let mut packetizer = Packetizer::new(&track, 26, 0);
    let mut out = BytesMut::new();
    for refused in [YUV444, YUV422, b"not a jpeg".as_slice()] {
        assert_eq!(
            packetizer.packetize(
                &Frame::new(0, 0, true, Bytes::copy_from_slice(refused)),
                &CodecParams::None,
                &mut out
            ),
            0
        );
        assert!(out.is_empty(), "no report for a frame that sends nothing");
        assert!(!packetizer.started());
    }
    assert_eq!(
        packetizer.packetize(&Frame::new(0, 0, true, Bytes::copy_from_slice(YUV420)), &CodecParams::None, &mut out),
        1
    );
    assert!(packetizer.started());
}

proptest! {
    #[test]
    fn whatever_the_bytes_the_reader_does_not_panic_and_fragments_stay_in_bounds(
        data in proptest::collection::vec(any::<u8>(), 0..400),
        max in 0usize..1300,
    ) {
        let mut file = vec![0xff, 0xd8];
        file.extend_from_slice(&data);
        if let Ok(jpeg) = parse(&file) {
            let total: usize = jpeg.fragments(max).map(|f| {
                prop_assert!(f.header().len() + f.data.len() <= max);
                Ok(f.data.len())
            }).collect::<Result<Vec<_>, TestCaseError>>()?.into_iter().sum();
            prop_assert!(total == 0 || total == jpeg.scan.len());
        }
    }
}
