//! AAC: the `AudioSpecificConfig` fields the track needs (ISO/IEC 14496-3,
//! 1.6.2.1), and `esds` (ISO/IEC 14496-1, 7.2.6.5, and 14496-14, 5.6).

use crate::Mp4Error;
use oxrtc_core::bits::{BitReader, Exhausted};

/// Longest `AudioSpecificConfig` carried: real ones are 2 to 7 bytes.
const MAX_CONFIG: usize = 64;

const RATES: [u32; 13] =
    [96_000, 88_200, 64_000, 48_000, 44_100, 32_000, 24_000, 22_050, 16_000, 12_000, 11_025, 8_000, 7_350];

/// What the writer takes from an `AudioSpecificConfig`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Config {
    /// The first audio object type: 2 for AAC-LC, 5 or 29 for HE-AAC.
    pub object_type: u8,
    /// The core coder's sampling rate.
    pub core_rate: u32,
    /// Samples of the core coder per frame: 1024, or 960.
    pub frame_length: u32,
}

fn object_type(r: &mut BitReader<'_>) -> Result<u8, Exhausted> {
    let kind = r.u8(5)?;
    if kind == 31 { Ok(32 + r.u8(6)?) } else { Ok(kind) }
}

fn rate(r: &mut BitReader<'_>) -> Result<u32, Exhausted> {
    let index = r.u8(4)?;
    if index == 15 {
        return u32::try_from(r.bits(24)?).map_err(|_| Exhausted);
    }
    RATES.get(usize::from(index)).copied().ok_or(Exhausted)
}

pub(crate) fn parse(config: &[u8]) -> Result<Config, Mp4Error> {
    let bad = |_: Exhausted| Mp4Error::invalid("AAC AudioSpecificConfig", "ends early or names a reserved rate");
    if config.len() > MAX_CONFIG {
        return Err(Mp4Error::invalid("AAC AudioSpecificConfig", "over 64 bytes"));
    }
    let mut r = BitReader::new(config);
    let first = object_type(&mut r).map_err(bad)?;
    let core_rate = rate(&mut r).map_err(bad)?;
    r.u8(4).map_err(bad)?; // channelConfiguration
    let mut core = first;
    if matches!(first, 5 | 29) {
        rate(&mut r).map_err(bad)?; // extensionSamplingFrequency
        core = object_type(&mut r).map_err(bad)?;
    }
    let mut frame_length = 1024;
    if matches!(core, 1..=4 | 6 | 7 | 17 | 19..=23) && r.bit().map_err(bad)? {
        frame_length = 960;
    }
    if core_rate == 0 || first == 0 {
        return Err(Mp4Error::invalid("AAC AudioSpecificConfig", "no object type or no rate"));
    }
    Ok(Config { object_type: first, core_rate, frame_length })
}

/// A frame's duration in units of `clock_rate`: the core coder's samples,
/// at the clock the track is timed with (an SBR stream timed at its output
/// rate lasts twice its core samples).
pub(crate) fn frame_duration(config: &Config, clock_rate: u32) -> u32 {
    let ticks = u64::from(config.frame_length) * u64::from(clock_rate) / u64::from(config.core_rate.max(1));
    u32::try_from(ticks).unwrap_or(u32::MAX).max(1)
}

/// A descriptor's tag and length, the length on four bytes as most writers
/// put it.
fn descriptor(out: &mut Vec<u8>, tag: u8, body: &[u8]) {
    let len = body.len();
    out.push(tag);
    out.extend_from_slice(&[
        0x80 | ((len >> 21) & 0x7f) as u8,
        0x80 | ((len >> 14) & 0x7f) as u8,
        0x80 | ((len >> 7) & 0x7f) as u8,
        (len & 0x7f) as u8,
    ]);
    out.extend_from_slice(body);
}

/// The `esds` box around `config`.
pub(crate) fn esds(config: &[u8], track_id: u16) -> Result<Vec<u8>, Mp4Error> {
    if config.len() > MAX_CONFIG {
        return Err(Mp4Error::invalid("AAC AudioSpecificConfig", "over 64 bytes"));
    }
    let mut specific = Vec::new();
    descriptor(&mut specific, 0x05, config);
    let mut decoder = vec![0x40, 0x15, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];
    decoder.extend_from_slice(&specific);
    let mut es = Vec::new();
    es.extend_from_slice(&track_id.to_be_bytes());
    es.push(0); // no stream dependence, URL or OCR
    descriptor(&mut es, 0x04, &decoder);
    descriptor(&mut es, 0x06, &[0x02]);
    let mut body = Vec::new();
    descriptor(&mut body, 0x03, &es);
    let mut w = crate::boxes::BoxWriter::with_capacity(12 + body.len());
    let open = w.start_full(b"esds", 0, 0);
    w.bytes(&body);
    w.end(open)?;
    Ok(w.into_inner())
}

/// `mp4a.40.<object type>` (RFC 6381, 3.3).
pub(crate) fn codec_string(config: &Config) -> String {
    format!("mp4a.40.{}", config.object_type)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lc_he_and_960_sample_configs_are_read() {
        // AAC-LC, 48 kHz, stereo: what FFmpeg's aac encoder announces.
        let lc = parse(&[0x11, 0x90]).unwrap();
        assert_eq!(lc, Config { object_type: 2, core_rate: 48_000, frame_length: 1024 });
        assert_eq!(codec_string(&lc), "mp4a.40.2");
        assert_eq!(frame_duration(&lc, 48_000), 1024);
        // HE-AAC, explicit: object type 5, core 24 kHz, extension 48 kHz, core AAC-LC.
        let he = parse(&[0x2b, 0x11, 0x88, 0x00]).unwrap();
        assert_eq!((he.object_type, he.core_rate), (5, 24_000));
        assert_eq!(codec_string(&he), "mp4a.40.5");
        assert_eq!(frame_duration(&he, 48_000), 2048);
        // AAC-LC, 16 kHz mono, 960 samples per frame.
        let short = parse(&[0x14, 0x0c]).unwrap();
        assert_eq!(short.frame_length, 960);
    }

    #[test]
    fn a_reserved_rate_or_an_empty_config_is_refused() {
        assert!(parse(&[]).is_err());
        assert!(parse(&[0x16, 0x80]).is_err(), "rate index 13 is reserved");
        assert!(parse(&[0; 65]).is_err());
        assert!(parse(&[0x01, 0x80]).is_err(), "object type 0");
    }

    #[test]
    fn esds_carries_the_config_in_its_descriptors() {
        let boxed = esds(&[0x11, 0x90], 2).unwrap();
        assert_eq!(&boxed[4..12], b"esds\0\0\0\0");
        let body = &boxed[12..];
        assert_eq!(body[0], 0x03);
        assert_eq!(&body[5..8], &[0, 2, 0], "ES_ID and flags");
        assert_eq!(body[8], 0x04);
        assert_eq!(&body[13..15], &[0x40, 0x15], "MPEG-4 audio, an audio stream");
        assert_eq!(body[26], 0x05);
        assert_eq!(&body[27..33], &[0x80, 0x80, 0x80, 2, 0x11, 0x90]);
        assert_eq!(&body[33..], &[0x06, 0x80, 0x80, 0x80, 1, 2]);
    }
}
