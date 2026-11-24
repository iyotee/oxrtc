//! AAC's `AudioSpecificConfig` (ISO/IEC 14496-3, 1.6.2.1): the fields every
//! writer of AAC reads from it, in one parser - the fragmented MP4 writer
//! (`esds`, the codec string, a frame's duration) and the MPEG-TS writer
//! (each frame's ADTS header, ISO/IEC 14496-3, 1.A.2).

use crate::bits::{BitReader, Exhausted};

/// Longest `AudioSpecificConfig` read: real ones are 2 to 7 bytes.
pub const MAX_CONFIG: usize = 64;

/// The sampling rates a 4-bit index names (`samplingFrequencyIndex`).
pub const RATES: [u32; 13] =
    [96_000, 88_200, 64_000, 48_000, 44_100, 32_000, 24_000, 22_050, 16_000, 12_000, 11_025, 8_000, 7_350];

/// What an `AudioSpecificConfig` says.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AudioConfig {
    /// The first audio object type: 2 for AAC-LC, 5 or 29 for HE-AAC.
    pub object_type: u8,
    /// The core coder's object type: the first one, or the one after an
    /// HE-AAC's extension rate (2 for HE-AAC over AAC-LC).
    pub core_object_type: u8,
    /// The core coder's sampling rate.
    pub core_rate: u32,
    /// The index that names [`AudioConfig::core_rate`] in [`RATES`], when
    /// the config gave one (an explicit 24-bit rate has none, and no ADTS
    /// header can carry it).
    pub core_rate_index: Option<u8>,
    /// `channelConfiguration`: 1 to 7 a layout by number, 0 one the
    /// stream's program config element describes.
    pub channel_configuration: u8,
    /// Samples of the core coder per frame: 1024, or 960.
    pub frame_length: u32,
}

/// Why a config cannot be read.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum AacConfigError {
    /// Over [`MAX_CONFIG`] bytes.
    #[error("over 64 bytes")]
    TooLong,
    /// It ends early or names a reserved rate.
    #[error("ends early or names a reserved rate")]
    Truncated,
    /// Its object type or its rate is 0.
    #[error("no object type or no rate")]
    Empty,
}

fn object_type(r: &mut BitReader<'_>) -> Result<u8, Exhausted> {
    let kind = r.u8(5)?;
    if kind == 31 { Ok(32 + r.u8(6)?) } else { Ok(kind) }
}

fn rate(r: &mut BitReader<'_>) -> Result<(u32, Option<u8>), Exhausted> {
    let index = r.u8(4)?;
    if index == 15 {
        return u32::try_from(r.bits(24)?).map(|rate| (rate, None)).map_err(|_| Exhausted);
    }
    RATES.get(usize::from(index)).map(|rate| (*rate, Some(index))).ok_or(Exhausted)
}

impl AudioConfig {
    /// Reads `config`.
    pub fn parse(config: &[u8]) -> Result<Self, AacConfigError> {
        if config.len() > MAX_CONFIG {
            return Err(AacConfigError::TooLong);
        }
        let bad = |_: Exhausted| AacConfigError::Truncated;
        let mut r = BitReader::new(config);
        let first = object_type(&mut r).map_err(bad)?;
        let (core_rate, core_rate_index) = rate(&mut r).map_err(bad)?;
        let channel_configuration = r.u8(4).map_err(bad)?;
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
            return Err(AacConfigError::Empty);
        }
        Ok(Self {
            object_type: first,
            core_object_type: core,
            core_rate,
            core_rate_index,
            channel_configuration,
            frame_length,
        })
    }

    /// A frame's duration in units of `clock_rate`: the core coder's
    /// samples, at the clock the track is timed with (an SBR stream timed at
    /// its output rate lasts twice its core samples).
    pub fn frame_duration(&self, clock_rate: u32) -> u32 {
        let ticks = u64::from(self.frame_length) * u64::from(clock_rate) / u64::from(self.core_rate.max(1));
        u32::try_from(ticks).unwrap_or(u32::MAX).max(1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_lc_he_and_explicit_rates() {
        // AAC-LC, 48 kHz, stereo.
        let lc = AudioConfig::parse(&[0x11, 0x90]).unwrap();
        assert_eq!((lc.object_type, lc.core_object_type, lc.core_rate), (2, 2, 48_000));
        assert_eq!((lc.core_rate_index, lc.channel_configuration, lc.frame_length), (Some(3), 2, 1024));
        assert_eq!(lc.frame_duration(48_000), 1024);
        // HE-AAC (5): 24 kHz core, 48 kHz extension, AAC-LC core.
        let he = AudioConfig::parse(&[0x2b, 0x11, 0x88, 0x00]).unwrap();
        assert_eq!((he.object_type, he.core_object_type, he.core_rate_index), (5, 2, Some(6)));
        assert_eq!(he.frame_duration(48_000), 2048);
        // An explicit rate: no index.
        let explicit = AudioConfig::parse(&[0x17, 0x80, 0x5d, 0xc0, 0x10]).unwrap();
        assert_eq!((explicit.core_rate, explicit.core_rate_index), (48_000, None));
        // 960-sample frames.
        assert_eq!(AudioConfig::parse(&[0x11, 0x94]).unwrap().frame_length, 960);
    }

    #[test]
    fn refuses_what_it_cannot_read() {
        assert_eq!(AudioConfig::parse(&[]), Err(AacConfigError::Truncated));
        assert_eq!(AudioConfig::parse(&[0x17]), Err(AacConfigError::Truncated));
        assert_eq!(AudioConfig::parse(&[0x01, 0x90]), Err(AacConfigError::Empty));
        assert_eq!(AudioConfig::parse(&[0u8; 65]), Err(AacConfigError::TooLong));
    }
}
