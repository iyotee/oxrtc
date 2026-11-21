//! The fuzz targets' harnesses on stable, under proptest: random bytes, input
//! close enough to real to get past the first check, and the committed seeds.

use proptest::prelude::*;

use super::*;

/// Cases per property: 1000 in `cargo test`, more for a soak run with
/// `OXRTC_PROPTEST_CASES=200000` (see `fuzz/README.md`).
fn cases() -> u32 {
    std::env::var("OXRTC_PROPTEST_CASES").ok().and_then(|v| v.parse().ok()).unwrap_or(1000)
}

/// Writer input: a track choice, a few bytes of parameter set, then frames
/// of every shape the harness knows.
fn writer_input() -> impl Strategy<Value = Vec<u8>> {
    let op = (any::<u8>(), 0u8..64, any::<u8>(), prop::collection::vec(any::<u8>(), 0..64)).prop_map(
        |(op, len, step, mut payload)| {
            payload.resize(usize::from(len), 0x21);
            let mut out = vec![op, len, step];
            out.extend(payload);
            out
        },
    );
    (any::<u8>(), 0u8..48, prop::collection::vec(any::<u8>(), 0..48), prop::collection::vec(op, 0..120)).prop_map(
        |(choice, extra_len, mut extra, ops)| {
            extra.resize(usize::from(extra_len), 0);
            let mut out = vec![choice, extra_len];
            out.extend(extra);
            out.extend(ops.concat());
            out
        },
    )
}

/// An init segment and the fragments of `frames` frames: H.264 and PCMA in
/// groups of ten pictures, or Opus alone.
fn stream(sound: bool, frames: usize) -> Vec<u8> {
    let tracks = if sound {
        vec![
            TrackInfo::video(Codec::H264).with_params(CodecParams::H264 { sps: hex(H264_SPS), pps: hex(H264_PPS) }),
            TrackInfo::audio(Codec::Pcma, 8000, 1),
        ]
    } else {
        vec![TrackInfo::audio(Codec::Opus, 48_000, 2)]
    };
    let mut w =
        Writer::new(&tracks, Options { fragmenting: Fragmenting::GroupOfPictures, ..Options::default() }).unwrap();
    let mut out = w.init_segment().to_vec();
    for i in 0..frames as i64 {
        let data = if sound { Bytes::from(vec![0, 0, 0, 2, 0x65, i as u8]) } else { Bytes::from_static(&[0xf8, 1]) };
        if let Some(f) = w.push(0, &Frame::new(0, i * 3600, i % 10 == 0, data)).unwrap() {
            out.extend_from_slice(&f.bytes);
        }
        if sound && let Some(f) = w.push(1, &Frame::new(1, i * 320, true, Bytes::from(vec![0xd5; 320]))).unwrap() {
            out.extend_from_slice(&f.bytes);
        }
    }
    if let Some(f) = w.flush().unwrap() {
        out.extend_from_slice(&f.bytes);
    }
    out
}

/// What the writer makes, so that the reader is fed real boxes, then cut or
/// flipped somewhere.
fn written() -> impl Strategy<Value = (Vec<u8>, usize, u8)> {
    (any::<bool>(), 1usize..40, any::<usize>(), any::<u8>()).prop_map(|(sound, frames, at, flip)| {
        let out = stream(sound, frames);
        let at = at % out.len();
        (out, at, flip)
    })
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(cases()))]

    #[test]
    fn the_writer_reads_back_whatever_it_is_fed(data in writer_input()) {
        prop_assert_eq!(writer_bytes(&data), Ok(()));
    }

    #[test]
    fn the_writer_takes_any_bytes(data in prop::collection::vec(any::<u8>(), 0..2048)) {
        prop_assert_eq!(writer_bytes(&data), Ok(()));
    }

    #[test]
    fn the_reader_takes_any_bytes(data in prop::collection::vec(any::<u8>(), 0..2048)) {
        prop_assert_eq!(reader_bytes(&data), Ok(()));
    }

    #[test]
    fn the_reader_takes_real_boxes_cut_or_flipped((data, at, flip) in written()) {
        prop_assert_eq!(reader_bytes(&data), Ok(()));
        prop_assert_eq!(reader_bytes(&data[..at]), Ok(()));
        prop_assert_eq!(reader_bytes(&data[at..]), Ok(()));
        let mut flipped = data.clone();
        flipped[at] ^= flip;
        prop_assert_eq!(reader_bytes(&flipped), Ok(()));
    }
}

/// A fragment whose second run's data offset points into the first run's
/// bytes (one flipped byte of a real fragment, found by the proptest above):
/// the reader read the same bytes twice, more sample bytes than the input
/// held, and `mp4_reader` took it for a broken invariant. Two samples never
/// share a byte; the reader now refuses it.
#[test]
fn runs_sharing_bytes_are_refused() {
    let tracks = [
        TrackInfo::video(Codec::H264).with_params(CodecParams::H264 { sps: hex(H264_SPS), pps: hex(H264_PPS) }),
        TrackInfo::audio(Codec::Pcma, 8000, 1),
    ];
    let mut w =
        Writer::new(&tracks, Options { fragmenting: Fragmenting::GroupOfPictures, ..Options::default() }).unwrap();
    let mut picture = vec![0, 0, 0x0f, 0xa1, 0x65];
    picture.resize(4005, 7);
    for i in 0..2i64 {
        w.push(0, &Frame::new(0, i * 3600, i == 0, Bytes::from(picture.clone()))).unwrap();
        w.push(1, &Frame::new(1, i * 320, true, Bytes::from(vec![0xd5; 320]))).unwrap();
    }
    let mut bytes = w.flush().unwrap().unwrap().bytes.to_vec();
    assert_eq!(reader_bytes(&bytes), Ok(()));
    let truns: Vec<usize> = bytes.windows(4).enumerate().filter(|(_, t)| t == b"trun").map(|(at, _)| at).collect();
    assert_eq!(truns.len(), 2);
    // The data offset follows the type, the version and flags, the sample count.
    let first: [u8; 4] = bytes[truns[0] + 12..truns[0] + 16].try_into().unwrap();
    bytes[truns[1] + 12..truns[1] + 16].copy_from_slice(&first);
    let init = read_init(w.init_segment()).unwrap();
    let refused = read_fragment(&bytes, &init.trex).unwrap_err();
    assert!(refused.contains("sharing bytes"), "{refused}");
    assert_eq!(reader_bytes(&bytes), Ok(()));
}

#[test]
fn the_harness_reads_a_valid_stream() {
    // H.264 and AAC, one fragment per frame: three keyframes, three sound frames.
    let mut data = vec![1 | (1 << 3), 0];
    for i in 0..6u8 {
        data.extend_from_slice(&[(i % 2) | 2, 4, 0, 0x21, 0x22, 0x23, 0x24]);
    }
    assert_eq!(writer_bytes(&data), Ok(()));
}

/// Writes the committed seeds again: `cargo test -p oxrtc-mp4 --lib write_seeds -- --ignored`.
#[test]
#[ignore = "writes fuzz/seeds"]
fn write_seeds() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fuzz/seeds");
    let reader = root.join("mp4_reader");
    std::fs::create_dir_all(&reader).unwrap();
    std::fs::write(reader.join("h264_pcma_gop"), stream(true, 25)).unwrap();
    std::fs::write(reader.join("opus_alone"), stream(false, 12)).unwrap();
    let writer = root.join("mp4_writer");
    std::fs::create_dir_all(&writer).unwrap();
    // Every frame of H.264 and AAC; H.265 and FLAC (A-law) in groups; a fuzzed SPS.
    let frames = |shape: &[u8]| -> Vec<u8> {
        (0..8u8).flat_map(|i| [(i % 2) | (i & 2), 4, i, 0x21, 0x22, 0x23, 0x24]).chain(shape.iter().copied()).collect()
    };
    std::fs::write(writer.join("h264_aac_every_frame"), [&[1 | (1 << 3), 0][..], &frames(&[])].concat()).unwrap();
    std::fs::write(writer.join("h265_pcma_gop"), [&[2 | (3 << 3) | (1 << 6), 0][..], &frames(&[])].concat()).unwrap();
    std::fs::write(
        writer.join("fuzzed_sps_parts"),
        [&[3 | (2 << 3) | (2 << 6), 6, 0x64, 0, 0x1e, 0xac, 0xd9, 0x40][..], &frames(&[0x10, 2, 9, 1, 2])].concat(),
    )
    .unwrap();
}

#[test]
fn the_committed_seeds_pass() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fuzz/seeds");
    let mut replayed = 0;
    for (target, harness) in
        [("mp4_reader", reader_bytes as fn(&[u8]) -> Result<(), String>), ("mp4_writer", writer_bytes)]
    {
        for entry in std::fs::read_dir(root.join(target)).unwrap() {
            let path = entry.unwrap().path();
            assert_eq!(harness(&std::fs::read(&path).unwrap()), Ok(()), "{}", path.display());
            replayed += 1;
        }
    }
    assert!(replayed >= 4, "{replayed} seeds");
}
