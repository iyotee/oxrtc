//! G.711 (ITU-T G.711, 1988): A-law and mu-law, one byte a sample, to and
//! from 16-bit linear PCM.
//!
//! Encoding follows the recommendation's decision levels: a sample is put in
//! the quantization interval it falls in (13 bits of magnitude for A-law, 14
//! for mu-law, as Table 1 and Table 2 of G.711 count them), and decoding gives
//! the interval's reconstruction value. Both directions are checked against
//! FFmpeg's `pcm_alaw` and `pcm_mulaw` for every input (`tests/g711.rs`).

/// The A-law code of a 16-bit sample.
#[must_use]
pub fn encode_alaw(sample: i16) -> u8 {
    // 13 bits of magnitude: the low 3 bits of a 16-bit sample are below
    // A-law's finest step.
    let mut value = i32::from(sample) >> 3;
    let mask = if value >= 0 {
        0xD5
    } else {
        value = -value - 1;
        0x55
    };
    // The segment: the number of the power of two the magnitude is below,
    // from 32 (segment 0) to 4096 (segment 7).
    let segment = (0u8..8).find(|&s| value < (32 << s)).unwrap_or(8);
    let code = match segment {
        8 => 0x7F,
        0 | 1 => (segment << 4) | ((value >> 1) & 0x0F) as u8,
        s => (s << 4) | ((value >> s) & 0x0F) as u8,
    };
    code ^ mask
}

/// The 16-bit sample of an A-law code.
#[must_use]
pub fn decode_alaw(code: u8) -> i16 {
    let code = code ^ 0x55;
    let mantissa = i32::from(code & 0x0F) << 4;
    let segment = (code & 0x70) >> 4;
    let magnitude = match segment {
        0 => mantissa + 8,
        s => (mantissa + 0x108) << (s - 1),
    };
    let value = if code & 0x80 != 0 { magnitude } else { -magnitude };
    // At most 32256 in magnitude: always a 16-bit value.
    i16::try_from(value).unwrap_or(i16::MAX)
}

/// mu-law's bias, in 14-bit units: the magnitude is offset by it so that
/// every segment starts on a power of two.
const MU_BIAS: i32 = 0x21;
/// The largest 14-bit magnitude mu-law encodes; larger ones are clipped.
const MU_CLIP: i32 = 8159;

/// The mu-law code of a 16-bit sample.
#[must_use]
pub fn encode_ulaw(sample: i16) -> u8 {
    // 14 bits: mu-law's finest step is 4 in 16-bit units.
    let mut value = i32::from(sample) >> 2;
    let mask = if value < 0 {
        value = -value;
        0x7F
    } else {
        0xFF
    };
    let value = value.min(MU_CLIP) + MU_BIAS;
    let segment = (0u8..8).find(|&s| value < (64 << s)).unwrap_or(8);
    let code = if segment >= 8 { 0x7F } else { (segment << 4) | ((value >> (segment + 1)) & 0x0F) as u8 };
    code ^ mask
}

/// The 16-bit sample of a mu-law code.
#[must_use]
pub fn decode_ulaw(code: u8) -> i16 {
    let code = !code;
    let magnitude = ((i32::from(code & 0x0F) << 3) + 0x84) << ((code & 0x70) >> 4);
    let value = if code & 0x80 != 0 { 0x84 - magnitude } else { magnitude - 0x84 };
    // At most 32124 in magnitude.
    i16::try_from(value).unwrap_or(i16::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Values of G.711's Tables 1 and 2 (the decoder outputs, scaled to 16
    /// bits as every implementation does): the smallest and largest step of
    /// each law, and the codes of silence.
    #[test]
    fn the_recommendations_reconstruction_values() {
        // A-law: 0xD5 is the first positive interval, 0x55 its negative twin.
        assert_eq!(decode_alaw(0xD5), 8);
        assert_eq!(decode_alaw(0x55), -8);
        assert_eq!(decode_alaw(0xAA), 32256);
        assert_eq!(decode_alaw(0x2A), -32256);
        // mu-law: 0xFF and 0x7F are both zero, 0x80 and 0x00 the extremes.
        assert_eq!(decode_ulaw(0xFF), 0);
        assert_eq!(decode_ulaw(0x7F), 0);
        assert_eq!(decode_ulaw(0x80), 32124);
        assert_eq!(decode_ulaw(0x00), -32124);
        assert_eq!(encode_alaw(0), 0xD5);
        assert_eq!(encode_ulaw(0), 0xFF);
        assert_eq!(encode_alaw(i16::MAX), 0xAA);
        assert_eq!(encode_alaw(i16::MIN), 0x2A);
        assert_eq!(encode_ulaw(i16::MAX), 0x80);
        assert_eq!(encode_ulaw(i16::MIN), 0x00);
    }

    /// A code decoded and encoded again is the same code (mu-law's negative
    /// zero, 0x7F, comes back as its positive zero).
    #[test]
    fn every_code_survives_a_round_trip() {
        for code in 0..=255u8 {
            assert_eq!(encode_alaw(decode_alaw(code)), code, "A-law {code:#04x}");
            let expected = if code == 0x7F { 0xFF } else { code };
            assert_eq!(encode_ulaw(decode_ulaw(code)), expected, "mu-law {code:#04x}");
        }
    }

    /// Each law's decoder is monotonic in the code's magnitude: a louder code
    /// is a larger sample.
    #[test]
    fn encoding_is_monotonic() {
        let mut last = (decode_alaw(encode_alaw(i16::MIN)), decode_ulaw(encode_ulaw(i16::MIN)));
        for sample in i16::MIN..=i16::MAX {
            let now = (decode_alaw(encode_alaw(sample)), decode_ulaw(encode_ulaw(sample)));
            assert!(now.0 >= last.0 && now.1 >= last.1, "{sample}");
            last = now;
        }
    }
}
