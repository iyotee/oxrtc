//! Opus: a packet's duration from its table of contents (RFC 6716, 3.1), and
//! `dOps` (the Opus-in-ISOBMFF encapsulation, 4.3.2).

use crate::Mp4Error;

/// Opus is always timed at 48 kHz.
pub(crate) const RATE: u32 = 48_000;

/// Longest packet, in samples at 48 kHz: 120 ms (RFC 6716, 3.2.5).
const MAX_PACKET_SAMPLES: u32 = 5760;

/// Samples at 48 kHz of one frame of a packet whose configuration is `config`.
fn frame_samples(config: u8) -> u32 {
    // In 2.5 ms steps: SILK 10, 20, 40, 60 ms; hybrid 10, 20; CELT 2.5, 5, 10, 20.
    let steps = match (config, config % 4) {
        (0..=11, 0) | (12..=15, 0 | 2) | (16.., 2) => 4,
        (0..=11, 1) | (12..=15, _) | (16.., 3) => 8,
        (0..=11, 2) => 16,
        (0..=11, _) => 24,
        (_, 0) => 1,
        _ => 2,
    };
    steps * 120
}

/// A packet's duration in samples at 48 kHz.
pub(crate) fn packet_samples(packet: &[u8]) -> Result<u32, Mp4Error> {
    let (&toc, rest) = packet.split_first().ok_or(Mp4Error::BadFrame("an empty Opus packet"))?;
    let frames = match toc & 3 {
        0 => 1,
        1 | 2 => 2,
        _ => {
            let count = rest.first().ok_or(Mp4Error::BadFrame("an Opus packet of code 3 with no frame count"))?;
            u32::from(count & 0x3f)
        }
    };
    let samples = frames * frame_samples(toc >> 3);
    if samples == 0 || samples > MAX_PACKET_SAMPLES {
        return Err(Mp4Error::BadFrame("an Opus packet of no frame or over 120 ms"));
    }
    Ok(samples)
}

/// The `dOps` box: no pre-skip (a live stream has no encoder delay to hide),
/// the input rate, no gain, channel mapping family 0 (mono or stereo).
pub(crate) fn dops(channels: u16) -> Result<Vec<u8>, Mp4Error> {
    let channels = u8::try_from(channels)
        .ok()
        .filter(|c| (1..=2).contains(c))
        .ok_or(Mp4Error::Unsupported("Opus with more than two channels"))?;
    let mut w = crate::boxes::BoxWriter::with_capacity(19);
    let open = w.start(b"dOps");
    w.u8(0); // Version
    w.u8(channels);
    w.u16(0); // PreSkip
    w.u32(RATE); // InputSampleRate
    w.u16(0); // OutputGain
    w.u8(0); // ChannelMappingFamily
    w.end(open)?;
    Ok(w.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_table_of_contents_gives_the_duration() {
        // Config 31 (CELT fullband 20 ms) code 0: what libopus sends at 20 ms.
        assert_eq!(packet_samples(&[31 << 3, 0xff]), Ok(960));
        assert_eq!(packet_samples(&[28 << 3]), Ok(120), "fullband 2.5 ms");
        // Config 1 (SILK 20 ms), code 1: two frames.
        assert_eq!(packet_samples(&[(1 << 3) | 1]), Ok(1920));
        // Config 16 (CELT 2.5 ms), code 3 with 6 frames.
        assert_eq!(packet_samples(&[(16 << 3) | 3, 6]), Ok(720));
        // Config 15 (hybrid 20 ms), code 2.
        assert_eq!(packet_samples(&[(15 << 3) | 2]), Ok(1920));
    }

    #[test]
    fn an_impossible_packet_is_refused() {
        assert!(packet_samples(&[]).is_err());
        assert!(packet_samples(&[3]).is_err(), "code 3 with no count");
        assert!(packet_samples(&[3, 0]).is_err(), "no frame");
        assert!(packet_samples(&[(3 << 3) | 3, 3]).is_err(), "3 x 60 ms");
    }

    #[test]
    fn dops_names_the_channels() {
        assert_eq!(dops(2).unwrap(), vec![0, 0, 0, 19, b'd', b'O', b'p', b's', 0, 2, 0, 0, 0, 0, 0xbb, 0x80, 0, 0, 0]);
        assert!(dops(3).is_err());
        assert!(dops(0).is_err());
    }
}
