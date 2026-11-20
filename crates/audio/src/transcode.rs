//! One sound turned into another: frames of G.711 or L16 in, at their rate
//! and channels, packets of 20 ms of Opus, G.711, L16 or G.722 out.
//!
//! The chain: decode to 16-bit samples, mix to one channel, resample to the
//! encoder's rate ([`crate::resample`]), cut into 20 ms, encode. The
//! input's clock is kept (D12): a packet is dated by where its first sample
//! was in the input, converted to the output's clock, so the sound sits
//! where the source put it. An input frame that does not follow the one
//! before it (a gap or an overlap of more than [`MAX_JITTER_MS`]) starts the
//! chain again from that frame's date; a smaller difference is taken for the
//! source's own rounding, and the samples stay continuous.

use crate::g711;
use crate::g722;
use crate::pcm;
use crate::resample::{RateError, Resampler};

/// A date this far from where the previous frame ended starts again.
pub const MAX_JITTER_MS: i64 = 100;
/// The most input bytes one call takes: a second of 16-bit stereo at 48 kHz.
pub const MAX_INPUT_BYTES: usize = 192_000;

/// The coding of the input.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputCodec {
    /// G.711 A-law.
    Alaw,
    /// G.711 mu-law.
    Ulaw,
    /// 16-bit PCM, big-endian (RTP's L16).
    L16,
    /// 16-bit PCM, little-endian (a WAV file's).
    PcmLe,
}

/// The input sound.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Input {
    /// Its coding.
    pub codec: InputCodec,
    /// Samples a second, per channel: also the clock of its dates.
    pub rate: u32,
    /// Interleaved channels.
    pub channels: u16,
}

/// The coding of the output.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputCodec {
    /// Opus (needs the `opus` feature).
    Opus,
    /// G.711 A-law.
    Alaw,
    /// G.711 mu-law.
    Ulaw,
    /// 16-bit PCM, big-endian.
    L16,
    /// G.722 at 64 kbit/s, from 16 kHz.
    G722,
}

/// The output sound.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Output {
    /// Its coding.
    pub codec: OutputCodec,
    /// Samples a second for G.711 and L16; Opus is encoded from the input's
    /// rate when libopus takes it, else from 48 kHz, and G.722 from 16 kHz,
    /// whatever this says.
    pub rate: u32,
}

impl Output {
    /// The RTP clock of the output's dates: 48000 for Opus, 8000 for G.722
    /// (RFC 3551), the rate otherwise.
    #[must_use]
    pub fn clock_rate(&self) -> u32 {
        match self.codec {
            OutputCodec::Opus => 48_000,
            OutputCodec::G722 => 8000,
            _ => self.rate,
        }
    }

    /// The channels an RTP description says: 2 for Opus (RFC 7587, whatever
    /// the stream holds), else 1.
    #[must_use]
    pub fn described_channels(&self) -> u16 {
        if self.codec == OutputCodec::Opus { 2 } else { 1 }
    }
}

/// A packet of the output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Packet {
    /// The coded sound.
    pub data: Vec<u8>,
    /// Its date in the output's clock ([`Output::clock_rate`]).
    pub pts: i64,
    /// Where its first sample was in the input, in the input's clock.
    pub source_pts: i64,
}

/// Why a transcode cannot be built or go on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TranscodeError {
    /// The rates cannot be resampled.
    Rate(RateError),
    /// No input channel, or more than a WAV may say.
    Channels(u16),
    /// The Opus encoder failed, or this build has none.
    Opus(String),
}

impl std::fmt::Display for TranscodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Rate(e) => e.fmt(f),
            Self::Channels(n) => write!(f, "{n} channels"),
            Self::Opus(e) => write!(f, "opus: {e}"),
        }
    }
}

impl std::error::Error for TranscodeError {}

enum Encoder {
    #[cfg(feature = "opus")]
    Opus(Box<crate::opus::Encoder>),
    Alaw,
    Ulaw,
    L16,
    G722(Box<g722::Encoder>),
}

/// Turns one input into one output; see the module.
pub struct Transcoder {
    input: Input,
    output: Output,
    encode_rate: u32,
    frame: usize,
    resampler: Resampler,
    encoder: Encoder,
    decoded: Vec<i16>,
    pending: Vec<i16>,
    /// The input date of the run's first sample, and the encoder-rate
    /// samples packed since it.
    anchor: Option<i64>,
    packed: u64,
    /// Where the next input frame is expected to start.
    expected: Option<i64>,
}

impl std::fmt::Debug for Transcoder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Transcoder")
            .field("input", &self.input)
            .field("output", &self.output)
            .field("encode_rate", &self.encode_rate)
            .finish_non_exhaustive()
    }
}

/// `value` in `from`'s clock, in `to`'s; rounded down.
fn rescale(value: i64, from: u32, to: u32) -> i64 {
    let scaled = i128::from(value) * i128::from(to) / i128::from(from.max(1));
    i64::try_from(scaled).unwrap_or(if scaled < 0 { i64::MIN } else { i64::MAX })
}

impl Transcoder {
    /// A transcoder of `input` to `output`.
    ///
    /// # Errors
    ///
    /// Rates the resampler does not take, no channel, or an Opus encoder
    /// that cannot be made (or a build without the `opus` feature).
    pub fn new(input: Input, output: Output) -> Result<Self, TranscodeError> {
        if input.channels == 0 || input.channels > crate::wav::MAX_CHANNELS {
            return Err(TranscodeError::Channels(input.channels));
        }
        let (encode_rate, encoder) = match output.codec {
            #[cfg(feature = "opus")]
            OutputCodec::Opus => {
                let rate = if crate::opus::RATES.contains(&input.rate) { input.rate } else { 48_000 };
                (rate, Encoder::Opus(Box::new(crate::opus::Encoder::new(rate).map_err(|e| TranscodeError::Opus(e.0))?)))
            }
            #[cfg(not(feature = "opus"))]
            OutputCodec::Opus => return Err(TranscodeError::Opus("this build has no Opus encoder".into())),
            OutputCodec::Alaw => (output.rate, Encoder::Alaw),
            OutputCodec::Ulaw => (output.rate, Encoder::Ulaw),
            OutputCodec::L16 => (output.rate, Encoder::L16),
            OutputCodec::G722 => (16_000, Encoder::G722(Box::default())),
        };
        let resampler = Resampler::new(input.rate, encode_rate).map_err(TranscodeError::Rate)?;
        Ok(Self {
            input,
            output,
            encode_rate,
            frame: encode_rate as usize / 50,
            resampler,
            encoder,
            decoded: Vec::new(),
            pending: Vec::new(),
            anchor: None,
            packed: 0,
            expected: None,
        })
    }

    /// The rate the encoder is fed at.
    #[must_use]
    pub fn encode_rate(&self) -> u32 {
        self.encode_rate
    }

    /// The input this transcoder reads.
    #[must_use]
    pub fn input(&self) -> Input {
        self.input
    }

    /// The output this transcoder makes.
    #[must_use]
    pub fn output(&self) -> Output {
        self.output
    }

    /// Samples held between calls, at the encoder's rate (less than one
    /// packet), and the resampler's input: the bounds of what it keeps.
    #[must_use]
    pub fn held(&self) -> (usize, usize) {
        (self.pending.len(), self.resampler.held())
    }

    /// Forgets the run: the next frame starts again from its own date.
    pub fn reset(&mut self) {
        self.resampler.reset();
        self.pending.clear();
        self.anchor = None;
        self.packed = 0;
        self.expected = None;
    }

    /// Transcodes one input frame dated `pts` (the input's clock) and appends
    /// the packets it completes to `out`. A frame longer than
    /// [`MAX_INPUT_BYTES`] is cut there.
    ///
    /// # Errors
    ///
    /// The Opus encoder failed; the transcoder is reset.
    pub fn push(&mut self, payload: &[u8], pts: i64, out: &mut Vec<Packet>) -> Result<(), TranscodeError> {
        let payload = payload.get(..payload.len().min(MAX_INPUT_BYTES)).unwrap_or_default();
        self.decoded.clear();
        match self.input.codec {
            InputCodec::Alaw => self.decoded.extend(payload.iter().map(|&c| g711::decode_alaw(c))),
            InputCodec::Ulaw => self.decoded.extend(payload.iter().map(|&c| g711::decode_ulaw(c))),
            InputCodec::L16 => pcm::decode_be(payload, &mut self.decoded),
            InputCodec::PcmLe => pcm::decode_le(payload, &mut self.decoded),
        }
        pcm::mix_down(&mut self.decoded, self.input.channels);
        let samples = i64::try_from(self.decoded.len()).unwrap_or(i64::MAX);
        let jitter = i64::from(self.input.rate) * MAX_JITTER_MS / 1000;
        if self.expected.is_some_and(|expected| (pts - expected).abs() > jitter) {
            self.reset();
        }
        let anchor = *self.anchor.get_or_insert(pts);
        self.expected = Some(self.expected.unwrap_or(anchor).saturating_add(samples));
        let decoded = std::mem::take(&mut self.decoded);
        self.resampler.process(&decoded, &mut self.pending);
        self.decoded = decoded;
        let mut start = 0;
        while self.pending.len() - start >= self.frame {
            let frame = self.pending.get(start..start + self.frame).unwrap_or_default();
            let data = match encode(&mut self.encoder, frame) {
                Ok(data) => data,
                Err(error) => {
                    self.reset();
                    return Err(error);
                }
            };
            let packed = i64::try_from(self.packed).unwrap_or(i64::MAX);
            out.push(Packet {
                data,
                pts: rescale(anchor, self.input.rate, self.output.clock_rate())
                    + rescale(packed, self.encode_rate, self.output.clock_rate()),
                source_pts: anchor + rescale(packed, self.encode_rate, self.input.rate),
            });
            self.packed += self.frame as u64;
            start += self.frame;
        }
        self.pending.drain(..start);
        Ok(())
    }
}

/// One packet of `frame`.
fn encode(encoder: &mut Encoder, frame: &[i16]) -> Result<Vec<u8>, TranscodeError> {
    Ok(match encoder {
        #[cfg(feature = "opus")]
        Encoder::Opus(encoder) => encoder.encode(frame).map_err(|e| TranscodeError::Opus(e.0))?,
        Encoder::Alaw => frame.iter().map(|&s| g711::encode_alaw(s)).collect(),
        Encoder::Ulaw => frame.iter().map(|&s| g711::encode_ulaw(s)).collect(),
        Encoder::L16 => {
            let mut out = Vec::with_capacity(frame.len() * 2);
            pcm::encode_be(frame, &mut out);
            out
        }
        Encoder::G722(encoder) => {
            let mut out = Vec::with_capacity(frame.len() / 2);
            encoder.encode(frame, &mut out);
            out
        }
    })
}

#[cfg(test)]
#[path = "transcode_tests.rs"]
mod tests;
