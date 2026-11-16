//! The fuzz target's harness on stable, under proptest: random bytes, and
//! streams close enough to a camera's to reach past the first check.

use proptest::prelude::*;

use super::*;

/// Cases per property: 1000 in `cargo test`, more for a soak run with
/// `OXRTC_PROPTEST_CASES=200000` (see `fuzz/README.md`).
fn cases() -> u32 {
    std::env::var("OXRTC_PROPTEST_CASES").ok().and_then(|v| v.parse().ok()).unwrap_or(1000)
}

/// A stream the way a camera writes it: parts with and without a length,
/// junk between, a closing delimiter.
fn stream() -> impl Strategy<Value = Vec<u8>> {
    let part = (
        prop::collection::vec(any::<u8>(), 0..300),
        any::<bool>(),
        prop::sample::select(vec!["--frame", "----frame", "--frame ", "-- frame", "--fram"]),
        prop::sample::select(vec!["\r\n", "\n", ""]),
    )
        .prop_map(|(picture, length, delimiter, end)| {
            let mut out = format!("{delimiter}\r\nContent-Type: image/jpeg\r\n").into_bytes();
            if length {
                out.extend_from_slice(format!("Content-Length: {}\r\n", picture.len()).as_bytes());
            }
            out.extend_from_slice(b"\r\n");
            out.extend_from_slice(&picture);
            out.extend_from_slice(end.as_bytes());
            out
        });
    prop::collection::vec(part, 1..6).prop_map(|parts| parts.concat())
}

#[test]
fn the_harness_accepts_a_valid_stream_and_reads_its_pictures() {
    let mut data = Vec::new();
    for i in 0..3u8 {
        data.extend_from_slice(b"--frame\r\nContent-Type: image/jpeg\r\nContent-Length: 6\r\n\r\n");
        data.extend_from_slice(&[0xff, 0xd8, i, 1, 2, 0xff]);
        data.extend_from_slice(b"\r\n");
    }
    assert_eq!(multipart(Some("frame"), 1000, 7, &data), Ok(()));
    assert_eq!(read(Some("frame"), 1000, 7, &data).unwrap().len(), 3);
    let mut bytes = vec![1, 15, 6];
    bytes.extend_from_slice(&data);
    assert_eq!(multipart_bytes(&bytes), Ok(()));
}

/// Every seed of `fuzz/seeds/multipart/` (and every crash a fuzzer found, once
/// it is copied there) through its harness, in `cargo test`.
#[test]
fn fuzz_seeds_replay() {
    let seeds = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fuzz/seeds/multipart");
    let mut replayed = 0;
    for entry in std::fs::read_dir(&seeds).unwrap() {
        let path = entry.unwrap().path();
        let data = std::fs::read(&path).unwrap();
        assert_eq!(multipart_bytes(&data), Ok(()), "{}", path.display());
        // A seed is a stream that gives pictures, not one refused at its first byte.
        let (head, stream) = data.split_at(3);
        let boundary =
            [None, Some("frame"), Some("--myboundary"), Some("video boundary--"), Some("a")][usize::from(head[0] % 5)];
        let max = 64 * (usize::from(head[1]) + 1);
        assert!(!read(boundary, max, usize::MAX, stream).unwrap().is_empty(), "{} gives no picture", path.display());
        replayed += 1;
    }
    assert!(replayed >= 5, "only {replayed} seeds under {}", seeds.display());
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(cases()))]

    /// Random bytes, any boundary, any bound, any cut.
    #[test]
    fn random_streams(
        data in prop::collection::vec(any::<u8>(), 0..3000),
        boundary in prop::sample::select(vec![None, Some("frame"), Some("--x"), Some("a b--")]),
        max in 1usize..2000,
        chunk in 1usize..600,
    ) {
        prop_assert_eq!(multipart(boundary, max, chunk, &data), Ok(()));
    }

    /// Streams shaped like a camera's, with delimiters that do and do not match.
    #[test]
    fn plausible_streams(
        data in stream(),
        boundary in prop::sample::select(vec![None, Some("frame"), Some("--frame")]),
        max in 1usize..400,
        chunk in 1usize..200,
    ) {
        prop_assert_eq!(multipart(boundary, max, chunk, &data), Ok(()));
    }

    /// The byte-driven entry point the fuzzer uses.
    #[test]
    fn fuzzer_entry(data in prop::collection::vec(any::<u8>(), 0..3000)) {
        prop_assert_eq!(multipart_bytes(&data), Ok(()));
    }
}
