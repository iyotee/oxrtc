//! The fuzz targets' harnesses on stable, under proptest, and the committed seeds.

use proptest::prelude::*;

use super::*;

/// Cases per property: 1000 in `cargo test`, more with
/// `OXRTC_PROPTEST_CASES=200000` (see `fuzz/README.md`).
fn cases() -> u32 {
    std::env::var("OXRTC_PROPTEST_CASES").ok().and_then(|v| v.parse().ok()).unwrap_or(1000)
}

fn writer_input() -> impl Strategy<Value = Vec<u8>> {
    let op = (any::<u8>(), 0u8..200, any::<u8>(), prop::collection::vec(any::<u8>(), 0..200)).prop_map(
        |(op, len, step, mut payload)| {
            payload.resize(usize::from(len), 0x65);
            let mut out = vec![op, len, step];
            out.extend(payload);
            out
        },
    );
    (any::<u8>(), 0u8..16, prop::collection::vec(any::<u8>(), 0..16), prop::collection::vec(op, 0..60)).prop_map(
        |(choice, extra_len, mut extra, ops)| {
            extra.resize(usize::from(extra_len), 0x12);
            let mut out = vec![choice, extra_len];
            out.extend(extra);
            out.extend(ops.concat());
            out
        },
    )
}

/// A real stream with a few bytes changed: past the sync checks.
fn damaged_stream() -> impl Strategy<Value = Vec<u8>> {
    (writer_input(), prop::collection::vec((any::<usize>(), any::<u8>()), 0..8)).prop_map(|(input, flips)| {
        let mut out = stream_of(&input);
        for (at, value) in flips {
            if !out.is_empty() {
                let len = out.len();
                out[at % len] = value;
            }
        }
        out
    })
}

/// What the writer makes of an input of the writer's harness.
fn stream_of(input: &[u8]) -> Vec<u8> {
    let Some((&choice, rest)) = input.split_first() else { return Vec::new() };
    let infos = tracks(choice | 1, rest.get(1..3).unwrap_or_default());
    let Ok(mut w) = Writer::new(&infos) else { return Vec::new() };
    let mut out = w.tables().to_vec();
    for (i, chunk) in rest.chunks(40).enumerate() {
        let data = Bytes::from([&(chunk.len() as u32).to_be_bytes()[..], chunk].concat());
        if !chunk.is_empty()
            && let Ok(bytes) = w.push(0, &Frame::new(0, i as i64 * 3600, i % 5 == 0, data))
        {
            out.extend_from_slice(&bytes);
        }
    }
    out
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(cases()))]

    #[test]
    fn any_bytes_are_read_or_refused(data in prop::collection::vec(any::<u8>(), 0..2000)) {
        prop_assert_eq!(reader_bytes(&data), Ok(()));
    }

    #[test]
    fn a_damaged_stream_is_read_or_refused(data in damaged_stream()) {
        prop_assert_eq!(reader_bytes(&data), Ok(()));
    }

    #[test]
    fn any_frames_read_back_as_written(data in writer_input()) {
        prop_assert_eq!(writer_bytes(&data), Ok(()));
    }
}

#[test]
fn the_committed_seeds_pass() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fuzz/seeds");
    for (target, harness) in
        [("mpegts_reader", reader_bytes as fn(&[u8]) -> Result<(), String>), ("mpegts_writer", writer_bytes)]
    {
        let dir = root.join(target);
        let seeds: Vec<_> = std::fs::read_dir(&dir).unwrap().map(|e| e.unwrap().path()).collect();
        assert!(!seeds.is_empty(), "{target}: no seed");
        for seed in seeds {
            assert_eq!(harness(&std::fs::read(&seed).unwrap()), Ok(()), "{}", seed.display());
        }
    }
}

/// Writes the seeds again (`cargo test -p oxrtc-mpegts -- --ignored write_seeds`).
#[test]
#[ignore = "writes fuzz/seeds"]
fn write_seeds() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fuzz/seeds");
    let h264_with_aac: Vec<u8> = [
        &[0x05u8, 0][..],
        &[0x80, 4, 10, 1, 2, 3, 4],
        &[0x01, 6, 10, 0x21, 0x21, 0x21, 0x21, 0x21, 0x21],
        &[0x40, 3, 10, 0x41, 9, 9],
        &[0x20, 3, 10, 0, 0, 9],
    ]
    .concat();
    let h265_opus: Vec<u8> =
        [&[0x0E, 0][..], &[0x80, 3, 10, 0x26, 1, 1], &[0x01, 2, 10, 0xF8, 0xFF], &[0x10, 3, 10, 0x02, 1, 1]].concat();
    let aac_of_input: Vec<u8> = [&[0x08, 2, 0x12, 0x08][..], &[0x00, 4, 20, 1, 2, 3, 4]].concat();
    let writer = [
        ("h264_with_aac", h264_with_aac.clone()),
        ("h265_opus", h265_opus.clone()),
        ("aac_config_of_input", aac_of_input),
    ];
    let reader = [
        ("h264_with_aac", stream_of(&h264_with_aac)),
        ("h265", stream_of(&h265_opus)),
        ("one_packet", vec![0x47; 188]),
    ];
    for (target, seeds) in [("mpegts_writer", &writer[..]), ("mpegts_reader", &reader[..])] {
        std::fs::create_dir_all(root.join(target)).unwrap();
        for (name, bytes) in seeds {
            std::fs::write(root.join(target).join(name), bytes).unwrap();
        }
    }
}
