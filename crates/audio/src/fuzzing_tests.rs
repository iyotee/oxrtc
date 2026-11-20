//! The fuzz target's harness on stable, under proptest: random bytes, and
//! streams close enough to a real one to reach past the header.

use proptest::prelude::*;

use super::*;

/// Cases per property: 1000 in `cargo test`, more for a soak run with
/// `OXRTC_PROPTEST_CASES=200000` (see `fuzz/README.md`).
fn cases() -> u32 {
    std::env::var("OXRTC_PROPTEST_CASES").ok().and_then(|v| v.parse().ok()).unwrap_or(1000)
}

/// A header the way writers make one: a format of any tag, size and rate,
/// chunks before `data`, an endless or a finite `data` size, then samples.
fn stream() -> impl Strategy<Value = Vec<u8>> {
    (
        prop::sample::select(vec![1u16, 3, 6, 7, 0xFFFE, 0x55]),
        prop::sample::select(vec![8u16, 16, 24]),
        0u16..4,
        prop::sample::select(vec![8000u32, 16_000, 44_100, 48_000, 0, 500_000]),
        prop::collection::vec(any::<u8>(), 0..64),
        prop::sample::select(vec![0u32, 0xFFFF_FFFF, 7, 100, 4000]),
        prop::collection::vec(any::<u8>(), 0..600),
        any::<bool>(),
    )
        .prop_map(|(tag, bits, channels, rate, list, data_size, samples, extensible_body)| {
            let mut fmt = Vec::new();
            fmt.extend_from_slice(&tag.to_le_bytes());
            fmt.extend_from_slice(&channels.to_le_bytes());
            fmt.extend_from_slice(&rate.to_le_bytes());
            fmt.extend_from_slice(&(rate * u32::from(channels)).to_le_bytes());
            fmt.extend_from_slice(&channels.to_le_bytes());
            fmt.extend_from_slice(&bits.to_le_bytes());
            if tag == 0xFFFE && extensible_body {
                fmt.extend_from_slice(&[22, 0, 16, 0, 4, 0, 0, 0, 6, 0]);
                fmt.extend_from_slice(&[0; 14]);
            }
            let mut out = b"RIFF\xff\xff\xff\xffWAVE".to_vec();
            if !list.is_empty() {
                out.extend_from_slice(b"LIST");
                out.extend_from_slice(&(list.len() as u32).to_le_bytes());
                out.extend_from_slice(&list);
                if list.len() % 2 == 1 {
                    out.push(0);
                }
            }
            out.extend_from_slice(b"fmt ");
            out.extend_from_slice(&(fmt.len() as u32).to_le_bytes());
            out.extend_from_slice(&fmt);
            out.extend_from_slice(b"data");
            out.extend_from_slice(&data_size.to_le_bytes());
            out.extend_from_slice(&samples);
            out
        })
}

#[test]
fn the_harness_reads_a_valid_stream() {
    let mut data = b"RIFF\xff\xff\xff\xffWAVEfmt \x10\x00\x00\x00\x06\x00\x01\x00\x40\x1f\x00\x00\x40\x1f\x00\x00\x01\x00\x08\x00data\xff\xff\xff\xff".to_vec();
    data.extend_from_slice(&[0xD5; 100]);
    for chunk in [1, 3, 7, 44, 1000] {
        assert_eq!(wav(chunk, &data), Ok(()), "{chunk}");
    }
    assert_eq!(read(5, &data).map(|(_, samples, _)| samples.len()), Ok(100));
}

/// Every seed of the fuzz target passes its harness, and is a stream that
/// gives a format and samples, not one refused at its first byte.
#[test]
fn fuzz_seeds_replay() {
    let seeds = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fuzz/seeds/wav_stream");
    let mut replayed = 0;
    for entry in std::fs::read_dir(&seeds).unwrap() {
        let path = entry.unwrap().path();
        let data = std::fs::read(&path).unwrap();
        assert_eq!(wav_bytes(&data), Ok(()), "{}", path.display());
        let (format, samples, refused) = read(usize::MAX, &data[1..]).unwrap();
        assert!(format.is_some() && !samples.is_empty() && !refused, "{} reads nothing", path.display());
        replayed += 1;
    }
    assert!(replayed >= 3, "only {replayed} seeds under {}", seeds.display());
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(cases()))]

    #[test]
    fn any_bytes_hold_the_bounds(data in prop::collection::vec(any::<u8>(), 0..2000)) {
        prop_assert_eq!(wav_bytes(&data), Ok(()));
    }

    #[test]
    fn a_writers_stream_reads_the_same_whatever_the_cuts(stream in stream(), chunk in 1usize..80) {
        prop_assert_eq!(wav(chunk, &stream), Ok(()));
    }

    #[test]
    fn a_damaged_stream_holds_the_bounds(stream in stream(), at in any::<prop::sample::Index>(), byte in any::<u8>()) {
        let mut stream = stream;
        let i = at.index(stream.len());
        stream[i] = byte;
        prop_assert_eq!(wav(13, &stream), Ok(()));
    }
}
