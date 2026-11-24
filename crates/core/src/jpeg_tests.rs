use proptest::prelude::*;

use super::*;

/// A picture as an encoder writes one: SOI, JFIF, a quantization table, a
/// frame header of `width` by `height`, the standard tables when asked, a
/// scan whose data holds stuffed bytes and a restart marker, EOI.
fn picture(width: u16, height: u16, tables: bool, scan: &[u8]) -> Vec<u8> {
    let mut out = vec![0xff, 0xd8];
    out.extend_from_slice(&[0xff, 0xe0, 0, 16, b'J', b'F', b'I', b'F', 0, 1, 1, 0, 0, 1, 0, 1, 0, 0]);
    out.extend_from_slice(&[0xff, 0xdb, 0, 67, 0]);
    out.extend_from_slice(&[1; 64]);
    out.extend_from_slice(&[0xff, 0xc0, 0, 17, 8]);
    out.extend_from_slice(&height.to_be_bytes());
    out.extend_from_slice(&width.to_be_bytes());
    out.extend_from_slice(&[3, 1, 0x22, 0, 2, 0x11, 1, 3, 0x11, 1]);
    if tables {
        out.extend_from_slice(&standard_dht());
    }
    out.extend_from_slice(&[0xff, 0xda, 0, 12, 3, 1, 0, 2, 0x11, 3, 0x11, 0, 63, 0]);
    out.extend_from_slice(scan);
    out.extend_from_slice(&[0xff, 0xd9]);
    out
}

/// Scan data with what a scan may hold: a stuffed 0xFF and a restart marker.
const SCAN: &[u8] = &[0x12, 0xff, 0x00, 0x34, 0xff, 0xd0, 0x56, 0xff, 0x00];

#[test]
fn the_standard_tables_are_one_segment_of_the_four() {
    let dht = standard_dht();
    assert_eq!(dht.get(..2), Some(&[0xff, 0xc4][..]));
    assert_eq!(dht.len(), 2 + 2 + 4 + DC_LUMA.len() + AC_LUMA.len() + DC_CHROMA.len() + AC_CHROMA.len());
    assert_eq!(usize::from(u16::from_be_bytes([dht[2], dht[3]])), dht.len() - 2);
    assert_eq!((dht[4], dht[4 + 29]), (0x00, 0x10));
}

#[test]
fn the_header_says_the_size_and_whether_tables_come() {
    let with = picture(1920, 1080, true, SCAN);
    let read = header(&with).unwrap();
    assert_eq!(read.size, Some((1920, 1080)));
    assert!(read.has_tables);
    assert_eq!(read.first_scan.map(|at| with[at + 1]), Some(0xda));
    assert!(!header(&picture(64, 48, false, SCAN)).unwrap().has_tables);
    assert_eq!(header(b"not a jpeg"), None);
    assert_eq!(header(&with[..20]), None);
}

#[test]
fn a_picture_without_tables_gets_the_standard_ones_before_its_scan() {
    let without = Bytes::from(picture(64, 48, false, SCAN));
    let fixed = with_huffman_tables(&without);
    assert_eq!(fixed.len(), without.len() + standard_dht().len());
    assert_eq!(&fixed[..], &picture(64, 48, true, SCAN)[..]);
    let with = Bytes::from(picture(64, 48, true, SCAN));
    assert_eq!(with_huffman_tables(&with), with);
    let junk = Bytes::from_static(b"\xff\xd8junk");
    assert_eq!(with_huffman_tables(&junk), junk);
}

#[test]
fn pictures_are_cut_at_their_end_whatever_the_reads() {
    let a = picture(64, 48, true, SCAN);
    let b = picture(32, 16, false, &[0xff, 0x00, 0xff, 0xd7, 9]);
    let mut stream = b"junk before\xff".to_vec();
    stream.extend_from_slice(&a);
    stream.extend_from_slice(b"\r\n");
    stream.extend_from_slice(&b);
    for read in [1, 2, 3, 7, 64, stream.len()] {
        let (pictures, error, _) = split_all(&stream, read, 1 << 16);
        assert_eq!(error, None, "reads of {read}");
        assert_eq!(pictures, vec![Bytes::from(a.clone()), Bytes::from(b.clone())], "reads of {read}");
    }
}

#[test]
fn an_end_of_image_inside_a_segment_does_not_cut_the_picture() {
    // An Exif thumbnail: a whole JPEG inside an APP1 segment.
    let thumbnail = picture(8, 8, false, &[1]);
    let mut exif = vec![0xff, 0xe1];
    exif.extend_from_slice(&u16::try_from(thumbnail.len() + 2).unwrap().to_be_bytes());
    exif.extend_from_slice(&thumbnail);
    let main = picture(64, 48, true, SCAN);
    let mut whole = vec![0xff, 0xd8];
    whole.extend_from_slice(&exif);
    whole.extend_from_slice(&main[2..]);
    let (pictures, error, _) = split_all(&whole, 5, 1 << 16);
    assert_eq!((pictures, error), (vec![Bytes::from(whole.clone())], None));
}

#[test]
fn a_broken_picture_is_dropped_and_the_next_one_read() {
    let good = picture(16, 16, true, SCAN);
    let mut stream = vec![0xff, 0xd8, 0xff, 0xe0, 0, 4, 1, 2, 0x55, 0x55];
    stream.extend_from_slice(&good);
    let (pictures, error, _) = split_all(&stream, 3, 1 << 16);
    assert_eq!((pictures, error), (vec![Bytes::from(good)], None));
}

#[test]
fn a_stream_without_a_picture_in_the_limit_is_an_error_not_memory() {
    let mut stream = vec![0xff, 0xd8, 0xff, 0xda, 0, 2];
    stream.extend(std::iter::repeat_n(0x11, 10_000));
    let (pictures, error, most) = split_all(&stream, 100, 1000);
    assert!(pictures.is_empty());
    assert_eq!(error, Some(SplitError::TooLarge(1000)));
    assert!(most <= 1101, "{most}");
    // And after it, the next picture is read.
    let good = picture(16, 16, true, SCAN);
    stream.extend_from_slice(&good);
    let (pictures, _, _) = split_all(&stream, 100, 1000);
    assert_eq!(pictures, vec![Bytes::from(good)]);
}

#[test]
fn the_invariants_hold_on_written_pictures() {
    let mut stream = Vec::new();
    for i in 0..5u16 {
        stream.extend_from_slice(&picture(16 * (i + 1), 16, i % 2 == 0, SCAN));
    }
    for read in [1, 5, 1000] {
        assert_eq!(split_invariants(&stream, read, 1 << 16), Ok(5));
    }
}

#[test]
fn the_committed_seeds_pass_and_ffmpeg_s_pictures_are_cut() {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fuzz/seeds/jpeg_split");
    let mut seen = 0;
    for entry in std::fs::read_dir(&dir).unwrap() {
        let data = std::fs::read(entry.unwrap().path()).unwrap();
        let [read, kib, rest @ ..] = &data[..] else { continue };
        let (read, max) = (usize::from(*read) + 1, (usize::from(*kib) + 1) * 1024);
        assert!(split_invariants(rest, read, max).is_ok(), "{:?}", split_invariants(rest, read, max));
        seen += 1;
    }
    assert!(seen > 0, "no seed in {}", dir.display());
    // FFmpeg 8.0.1, `testsrc=size=64x48 -frames:v 3 -c:v mjpeg -f mjpeg`.
    let ffmpeg = std::fs::read(dir.join("ffmpeg_three_pictures_reads_of_8")).unwrap();
    let (pictures, error, _) = split_all(&ffmpeg[2..], 8, 1 << 16);
    assert_eq!((pictures.len(), error), (3, None));
    assert!(pictures.iter().all(|p| header(p).and_then(|h| h.size) == Some((64, 48))));
}

fn cases() -> u32 {
    std::env::var("OXRTC_PROPTEST_CASES").ok().and_then(|v| v.parse().ok()).unwrap_or(256)
}

/// Pieces that make streams close to real ones: whole pictures, their parts,
/// stuffed bytes, markers, junk.
fn piece() -> impl Strategy<Value = Vec<u8>> {
    prop_oneof![
        (1u16..64, 1u16..64, any::<bool>(), prop::collection::vec(any::<u8>(), 0..40)).prop_map(|(w, h, t, scan)| {
            let scan: Vec<u8> =
                scan.into_iter().flat_map(|b| if b == 0xff { vec![0xff, 0] } else { vec![b] }).collect();
            picture(w, h, t, &scan)
        }),
        Just(vec![0xff, 0xd8]),
        Just(vec![0xff, 0xd9]),
        Just(vec![0xff, 0xda, 0, 2]),
        Just(vec![0xff, 0x00]),
        Just(vec![0xff, 0xd3]),
        prop::collection::vec(any::<u8>(), 0..32),
    ]
}

#[test]
fn a_still_is_encoded_again_when_go2rtc_would_encode_it_again() {
    let jfif = picture(64, 48, true, SCAN);
    assert!(!wants_encoding_again(&jfif), "JFIF first");
    let mut exif = jfif.clone();
    exif[3] = 0xe1;
    exif[6..10].copy_from_slice(b"Exif");
    assert!(!wants_encoding_again(&exif), "Exif first");
    let mut avi1 = jfif.clone();
    avi1[6..10].copy_from_slice(b"AVI1");
    assert!(!wants_encoding_again(&avi1), "a webcam's AVI1: its tables are written in instead");
    // No application segment: the quantization table first (an RTP/JPEG
    // camera's picture as retina rebuilds it).
    let mut bare = vec![0xff, 0xd8];
    bare.extend_from_slice(&jfif[20..]);
    assert_eq!(&bare[2..4], &[0xff, 0xdb]);
    assert!(!wants_encoding_again(&bare), "a quantization table first");
    // A comment first (mjpg-streamer), another application segment, the
    // frame header first: encoded again.
    let mut comment = vec![0xff, 0xd8, 0xff, 0xfe, 0, 8, b'c', b'a', b'm', b'e', b'r', b'a'];
    comment.extend_from_slice(&jfif[2..]);
    assert!(wants_encoding_again(&comment));
    let mut other = jfif.clone();
    other[6..10].copy_from_slice(b"JFXX");
    assert!(wants_encoding_again(&other));
    let mut frame_first = vec![0xff, 0xd8];
    frame_first.extend_from_slice(&jfif[89..]);
    assert_eq!(&frame_first[2..4], &[0xff, 0xc0]);
    assert!(wants_encoding_again(&frame_first));
    // Too short, or not a JPEG: as it is.
    assert!(!wants_encoding_again(&[0xff, 0xd8, 0xff, 0xfe, 0, 4, 1, 2, 0xff]));
    assert!(!wants_encoding_again(b"not a jpeg at all"));
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(cases()))]

    /// The twin of the fuzz target `jpeg_split`, on any bytes.
    #[test]
    fn any_bytes_keep_the_invariants(data in prop::collection::vec(any::<u8>(), 0..3000), read in 1usize..400, max in 4usize..2000) {
        prop_assert!(split_invariants(&data, read, max).is_ok(), "{:?}", split_invariants(&data, read, max));
    }

    /// And on streams of pictures and their pieces.
    #[test]
    fn streams_of_pictures_keep_the_invariants(pieces in prop::collection::vec(piece(), 0..12), read in 1usize..300) {
        let data = pieces.concat();
        prop_assert!(split_invariants(&data, read, 1 << 16).is_ok(), "{:?}", split_invariants(&data, read, 1 << 16));
    }

    /// Pictures written one after the other come back whole, each.
    #[test]
    fn written_pictures_come_back(sizes in prop::collection::vec((1u16..2000, 1u16..2000, any::<bool>()), 1..8), read in 1usize..500) {
        let written: Vec<Bytes> = sizes.iter().map(|&(w, h, t)| Bytes::from(picture(w, h, t, SCAN))).collect();
        let data: Vec<u8> = written.iter().flat_map(|p| p.iter().copied()).collect();
        let (pictures, error, _) = split_all(&data, read, 1 << 16);
        prop_assert_eq!(error, None);
        prop_assert_eq!(&pictures, &written);
        for (picture, &(w, h, _)) in pictures.iter().zip(&sizes) {
            prop_assert_eq!(header(picture).and_then(|r| r.size), Some((w, h)));
            prop_assert!(header(&with_huffman_tables(picture)).is_some_and(|r| r.has_tables));
        }
    }
}
