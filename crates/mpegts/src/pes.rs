//! The PES packet header (ISO/IEC 13818-1, 2.4.3.6): a start code, the
//! stream's id, the length, the presentation time and, when it differs, the
//! decode time.

use crate::TsError;

/// `stream_id` of a video stream (the first one).
pub(crate) const VIDEO: u8 = 0xE0;
/// `stream_id` of an MPEG audio stream (the first one).
pub(crate) const AUDIO: u8 = 0xC0;
/// `stream_id` of `private_stream_1` (Opus).
pub(crate) const PRIVATE_1: u8 = 0xBD;

/// The 33 bits a time is written on.
pub(crate) const TIME_MASK: u64 = (1 << 33) - 1;

/// A time on five bytes, behind its 4-bit prefix, with its marker bits.
fn time(prefix: u8, value: u64) -> [u8; 5] {
    let value = value & TIME_MASK;
    [
        (prefix << 4) | (((value >> 29) & 0x0E) as u8) | 1,
        (value >> 22) as u8,
        (((value >> 14) & 0xFE) as u8) | 1,
        (value >> 7) as u8,
        (((value << 1) & 0xFE) as u8) | 1,
    ]
}

/// The header of a PES of `payload_len` bytes. `bounded` writes its length
/// (required for audio, and refused over 64 KiB); a video PES may say 0,
/// "as long as it lasts", which any length needs.
pub(crate) fn header(
    stream_id: u8,
    pts: u64,
    dts: Option<u64>,
    payload_len: usize,
    bounded: bool,
) -> Result<Vec<u8>, TsError> {
    let dts = dts.filter(|d| *d != pts);
    let data_length: u8 = if dts.is_some() { 10 } else { 5 };
    let length = if bounded {
        let total = 3 + usize::from(data_length) + payload_len;
        u16::try_from(total).map_err(|_| TsError::TooLarge(payload_len))?
    } else {
        0
    };
    let mut out = Vec::with_capacity(9 + usize::from(data_length));
    out.extend_from_slice(&[0, 0, 1, stream_id]);
    out.extend_from_slice(&length.to_be_bytes());
    // '10', not scrambled, no priority, data_alignment_indicator, no copyright, a copy.
    out.push(0x84);
    out.push(if dts.is_some() { 0xC0 } else { 0x80 });
    out.push(data_length);
    match dts {
        Some(dts) => {
            out.extend_from_slice(&time(0b0011, pts));
            out.extend_from_slice(&time(0b0001, dts));
        }
        None => out.extend_from_slice(&time(0b0010, pts)),
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read_time(b: &[u8]) -> u64 {
        (u64::from(b[0] & 0x0E) << 29)
            | (u64::from(b[1]) << 22)
            | (u64::from(b[2] >> 1) << 15)
            | (u64::from(b[3]) << 7)
            | u64::from(b[4] >> 1)
    }

    #[test]
    fn writes_the_times_with_their_markers() {
        let h = header(VIDEO, 0x1_2345_6789, None, 1000, false).unwrap();
        assert_eq!(&h[..9], &[0, 0, 1, 0xE0, 0, 0, 0x84, 0x80, 5]);
        assert_eq!(h[9] >> 4, 0b0010);
        assert_eq!(read_time(&h[9..14]), 0x1_2345_6789);
        assert!([h[9], h[11], h[13]].iter().all(|b| b & 1 == 1), "markers");
        let both = header(VIDEO, 9000, Some(5400), 10, false).unwrap();
        assert_eq!((both[7], both[8]), (0xC0, 10));
        assert_eq!((read_time(&both[9..14]), read_time(&both[14..19])), (9000, 5400));
        assert_eq!(header(VIDEO, 9000, Some(9000), 10, false).unwrap().len(), 14, "the same decode time is left out");
    }

    #[test]
    fn a_bounded_pes_says_its_length() {
        let h = header(AUDIO, 0, None, 300, true).unwrap();
        assert_eq!(u16::from_be_bytes([h[4], h[5]]), 3 + 5 + 300);
        assert!(header(AUDIO, 0, None, 70_000, true).is_err());
        assert!(header(VIDEO, 0, None, 70_000, false).is_ok());
        // 33 bits: a time past them wraps.
        let wrapped = header(VIDEO, (1 << 33) + 7, None, 0, false).unwrap();
        assert_eq!(read_time(&wrapped[9..14]), 7);
    }
}
