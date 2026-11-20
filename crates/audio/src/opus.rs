//! The Opus encoder: libopus 1.6 through the `opus` crate (D28, D37).
//!
//! Set as FFmpeg's `libopus` is set by go2rtc's `opus` template
//! (`-c:a libopus -application:a lowdelay`): the restricted low-delay
//! application (CELT only, 2.5 ms of look-ahead), FFmpeg's default bit rate
//! for one channel (64 kbit/s) and complexity (10), 20 ms a packet, one
//! channel. RTP always carries Opus at a clock of 48000 (RFC 7587).

/// Why the encoder failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpusError(pub String);

impl std::fmt::Display for OpusError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "opus: {}", self.0)
    }
}

impl std::error::Error for OpusError {}

fn error(e: ::opus::Error) -> OpusError {
    OpusError(e.to_string())
}

/// The rates libopus encodes from.
pub const RATES: [u32; 5] = [8000, 12_000, 16_000, 24_000, 48_000];
/// The bit rate, FFmpeg's default for one channel.
pub const BITRATE: i32 = 64_000;
/// The encoder's complexity, FFmpeg's default.
pub const COMPLEXITY: i32 = 10;
/// The largest packet written: more than 20 ms at 510 kbit/s takes.
pub const MAX_PACKET: usize = 1500;

/// One channel of Opus, 20 ms a packet.
pub struct Encoder {
    inner: ::opus::Encoder,
    rate: u32,
    scratch: Vec<u8>,
}

impl std::fmt::Debug for Encoder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Encoder").field("rate", &self.rate).finish_non_exhaustive()
    }
}

impl Encoder {
    /// An encoder of sound at `rate`, one of [`RATES`].
    ///
    /// # Errors
    ///
    /// libopus refused the rate or a setting.
    pub fn new(rate: u32) -> Result<Self, OpusError> {
        let mut inner =
            ::opus::Encoder::new(rate, ::opus::Channels::Mono, ::opus::Application::LowDelay).map_err(error)?;
        inner.set_bitrate(::opus::Bitrate::Bits(BITRATE)).map_err(error)?;
        inner.set_complexity(COMPLEXITY).map_err(error)?;
        Ok(Self { inner, rate, scratch: vec![0; MAX_PACKET] })
    }

    /// Samples in one packet: 20 ms at the encoder's rate.
    #[must_use]
    pub fn frame_samples(&self) -> usize {
        self.rate as usize / 50
    }

    /// The packet of one frame of [`Self::frame_samples`] samples.
    ///
    /// # Errors
    ///
    /// libopus failed, or the frame is not 20 ms long.
    pub fn encode(&mut self, frame: &[i16]) -> Result<Vec<u8>, OpusError> {
        let written = self.inner.encode(frame, &mut self.scratch).map_err(error)?;
        Ok(self.scratch.get(..written).unwrap_or_default().to_vec())
    }

    /// libopus' version, for the startup line.
    #[must_use]
    pub fn version() -> &'static str {
        ::opus::version()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_frame_of_each_rate_is_one_packet() {
        for rate in RATES {
            let mut encoder = Encoder::new(rate).unwrap();
            let frame: Vec<i16> =
                (0..encoder.frame_samples()).map(|i| ((i as f64 * 0.3).sin() * 8000.0) as i16).collect();
            let packet = encoder.encode(&frame).unwrap();
            assert!(!packet.is_empty() && packet.len() <= MAX_PACKET, "{rate}: {}", packet.len());
            // 64 kbit/s: 160 bytes in 20 ms, a constrained variable rate around it.
            assert!(packet.len() < 400, "{rate}: {}", packet.len());
        }
        assert!(Encoder::new(44_100).is_err());
        assert!(Encoder::new(8000).unwrap().encode(&[0; 7]).is_err());
        assert!(Encoder::version().starts_with("libopus"));
    }
}
