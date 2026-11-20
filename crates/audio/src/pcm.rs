//! Linear 16-bit PCM: L16 as RTP carries it (RFC 3551, big-endian) and as a
//! WAV file carries it (little-endian), and the mix of a stereo stream down
//! to one channel.

/// The samples of an L16 payload (big-endian); an odd last byte is dropped.
pub fn decode_be(payload: &[u8], out: &mut Vec<i16>) {
    out.extend(payload.as_chunks::<2>().0.iter().map(|pair| i16::from_be_bytes(*pair)));
}

/// The samples of little-endian PCM (a WAV file's); an odd last byte is dropped.
pub fn decode_le(payload: &[u8], out: &mut Vec<i16>) {
    out.extend(payload.as_chunks::<2>().0.iter().map(|pair| i16::from_le_bytes(*pair)));
}

/// `samples` as an L16 payload.
pub fn encode_be(samples: &[i16], out: &mut Vec<u8>) {
    out.reserve(samples.len() * 2);
    for sample in samples {
        out.extend_from_slice(&sample.to_be_bytes());
    }
}

/// Interleaved frames of `channels` samples mixed to one channel, in place:
/// the mean of each frame. One channel is left as it is; a partial last
/// frame is dropped.
pub fn mix_down(samples: &mut Vec<i16>, channels: u16) {
    let channels = usize::from(channels);
    if channels <= 1 {
        return;
    }
    let frames = samples.len() / channels;
    for frame in 0..frames {
        let start = frame * channels;
        let sum: i32 = samples.get(start..start + channels).map_or(0, |s| s.iter().map(|&v| i32::from(v)).sum());
        let mean = sum / i32::try_from(channels).unwrap_or(1);
        if let Some(slot) = samples.get_mut(frame) {
            *slot = i16::try_from(mean).unwrap_or(0);
        }
    }
    samples.truncate(frames);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn both_byte_orders_read_and_write() {
        let mut out = Vec::new();
        decode_be(&[0x12, 0x34, 0xFF, 0xFE, 0x01], &mut out);
        assert_eq!(out, [0x1234, -2]);
        let mut le = Vec::new();
        decode_le(&[0x34, 0x12, 0xFE, 0xFF], &mut le);
        assert_eq!(le, [0x1234, -2]);
        let mut bytes = Vec::new();
        encode_be(&out, &mut bytes);
        assert_eq!(bytes, [0x12, 0x34, 0xFF, 0xFE]);
    }

    #[test]
    fn stereo_is_mixed_to_its_mean() {
        let mut samples = vec![100, 300, -32768, -32768, 32767, -32767, 7];
        mix_down(&mut samples, 2);
        assert_eq!(samples, [200, -32768, 0]);
        let mut mono = vec![1, 2, 3];
        mix_down(&mut mono, 1);
        assert_eq!(mono, [1, 2, 3]);
    }
}
