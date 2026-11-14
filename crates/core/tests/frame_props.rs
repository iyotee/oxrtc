//! Properties of the NAL unit parsing: it never panics, and round-trips.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing, clippy::panic)]

use oxrtc_core::frame::{NalUnits, annex_b_to_avcc, avcc_to_annex_b, validate_avcc};
use proptest::prelude::*;

proptest! {
    #[test]
    fn validation_never_panics(data in proptest::collection::vec(any::<u8>(), 0..512)) {
        let _ = validate_avcc(&data);
        let _ = NalUnits::new(&data).count();
        let _ = annex_b_to_avcc(&data);
    }

    #[test]
    fn avcc_and_annex_b_round_trip(units in proptest::collection::vec(proptest::collection::vec(1u8..=255, 1..64), 1..8)) {
        let mut avcc = Vec::new();
        for unit in &units {
            avcc.extend_from_slice(&(unit.len() as u32).to_be_bytes());
            avcc.extend_from_slice(unit);
        }
        prop_assert_eq!(validate_avcc(&avcc), Ok(units.len()));
        let annex_b = avcc_to_annex_b(&avcc);
        let back = annex_b_to_avcc(&annex_b).unwrap();
        prop_assert_eq!(&back[..], &avcc[..]);
    }
}

#[test]
fn truncated_and_empty_units_are_refused() {
    use oxrtc_core::FrameError;
    assert_eq!(validate_avcc(&[]), Err(FrameError::Empty));
    assert_eq!(validate_avcc(&[0, 0, 0]), Err(FrameError::TruncatedLength));
    assert_eq!(validate_avcc(&[0, 0, 0, 0]), Err(FrameError::EmptyNal));
    assert_eq!(validate_avcc(&[0, 0, 0, 9, 1, 2]), Err(FrameError::TruncatedNal { declared: 9, available: 2 }));
    assert_eq!(validate_avcc(&[0, 0, 0, 1, 0x65, 0, 0, 0, 1, 0x41]), Ok(2));
}

#[test]
fn annex_b_with_three_and_four_byte_start_codes() {
    let data = [0, 0, 0, 1, 0x67, 0x42, 0, 0, 1, 0x68, 0xce, 0, 0, 0, 1, 0x65, 0x88, 0x84];
    let avcc = annex_b_to_avcc(&data).unwrap();
    let units: Vec<&[u8]> = NalUnits::new(&avcc).collect();
    assert_eq!(units, vec![&[0x67, 0x42][..], &[0x68, 0xce][..], &[0x65, 0x88, 0x84][..]]);
}
