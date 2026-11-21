//! The PCM family as FLAC, the way go2rtc gives it to browsers (MSE has no
//! G.711 nor raw PCM): G.711 A-law and mu-law decoded to 16-bit samples with
//! `oxrtc-audio`'s decoders, L16 as it is, each frame written as one FLAC frame
//! (RFC 9639) of verbatim subframes - a frame header with its CRC-8, one
//! subframe per channel, the CRC-16. Nothing is compressed: a FLAC decoder
//! gives back the very samples.
//!
//! The stream is of variable block size (a frame's samples are those of the
//! source frame), so each header carries its first sample's number.

use oxrtc_audio::{g711, pcm};
use oxrtc_core::Codec;

use crate::Mp4Error;

/// Most samples per channel in one FLAC frame (its block size field).
pub(crate) const MAX_BLOCK: usize = 65_535;

/// Most channels a FLAC frame of independent channels holds.
pub(crate) const MAX_CHANNELS: u16 = 8;

/// Highest rate STREAMINFO holds (20 bits).
pub(crate) const MAX_RATE: u32 = (1 << 20) - 1;

/// Sample numbers are coded on 36 bits and wrap there.
const SAMPLE_NUMBER_MASK: u64 = (1 << 36) - 1;

/// The PCM coding a FLAC track is made from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Pcm {
    Alaw,
    Ulaw,
    L16,
}

impl Pcm {
    pub(crate) fn of(codec: Codec) -> Option<Self> {
        match codec {
            Codec::Pcma => Some(Self::Alaw),
            Codec::Pcmu => Some(Self::Ulaw),
            Codec::L16 => Some(Self::L16),
            _ => None,
        }
    }

    /// The interleaved 16-bit samples of a frame.
    pub(crate) fn decode(self, payload: &[u8], channels: u16, out: &mut Vec<i16>) -> Result<(), Mp4Error> {
        let width = if self == Self::L16 { 2 } else { 1 };
        let frame = width * usize::from(channels.max(1));
        if payload.is_empty() || !payload.len().is_multiple_of(frame) {
            return Err(Mp4Error::BadFrame("PCM that is not whole sample frames"));
        }
        match self {
            Self::Alaw => out.extend(payload.iter().map(|&code| g711::decode_alaw(code))),
            Self::Ulaw => out.extend(payload.iter().map(|&code| g711::decode_ulaw(code))),
            Self::L16 => pcm::decode_be(payload, out),
        }
        Ok(())
    }
}

/// CRC-8, polynomial x^8 + x^2 + x + 1, initial 0: the frame header's.
pub(crate) fn crc8(data: &[u8]) -> u8 {
    data.iter().fold(0u8, |mut crc, &byte| {
        crc ^= byte;
        for _ in 0..8 {
            crc = if crc & 0x80 != 0 { (crc << 1) ^ 0x07 } else { crc << 1 };
        }
        crc
    })
}

/// CRC-16, polynomial x^16 + x^15 + x^2 + 1, initial 0: the whole frame's.
pub(crate) fn crc16(data: &[u8]) -> u16 {
    data.iter().fold(0u16, |mut crc, &byte| {
        crc ^= u16::from(byte) << 8;
        for _ in 0..8 {
            crc = if crc & 0x8000 != 0 { (crc << 1) ^ 0x8005 } else { crc << 1 };
        }
        crc
    })
}

/// The frame header's sample rate code, or 0 ("as STREAMINFO says").
fn rate_code(rate: u32) -> u8 {
    match rate {
        88_200 => 1,
        176_400 => 2,
        192_000 => 3,
        8_000 => 4,
        16_000 => 5,
        22_050 => 6,
        24_000 => 7,
        32_000 => 8,
        44_100 => 9,
        48_000 => 10,
        96_000 => 11,
        _ => 0,
    }
}

/// `number` in FLAC's extended UTF-8 coding, up to 36 bits.
fn coded_number(number: u64, out: &mut Vec<u8>) {
    let number = number & SAMPLE_NUMBER_MASK;
    let continuation = match number {
        0..0x80 => {
            out.push(number as u8);
            return;
        }
        0x80..0x800 => 1,
        0x800..0x1_0000 => 2,
        0x1_0000..0x20_0000 => 3,
        0x20_0000..0x400_0000 => 4,
        0x400_0000..0x8000_0000 => 5,
        _ => 6,
    };
    // The lead byte: as many high ones as bytes, then the top bits.
    let lead_ones = 0xffu16 << (7 - continuation);
    let top = (number >> (6 * continuation)) as u16;
    out.push(((lead_ones | top) & 0xff) as u8);
    for i in (0..continuation).rev() {
        out.push(0x80 | ((number >> (6 * i)) & 0x3f) as u8);
    }
}

/// Writes FLAC frames for one track.
#[derive(Debug, Clone)]
pub(crate) struct Framer {
    channels: u16,
    rate: u32,
    /// The number of the next frame's first sample.
    next_sample: u64,
}

impl Framer {
    pub(crate) fn new(channels: u16, rate: u32) -> Result<Self, Mp4Error> {
        if channels == 0 || channels > MAX_CHANNELS {
            return Err(Mp4Error::Unsupported("FLAC with no channel or more than eight"));
        }
        if rate == 0 || rate > MAX_RATE {
            return Err(Mp4Error::Unsupported("FLAC at a rate over 20 bits"));
        }
        Ok(Self { channels, rate, next_sample: 0 })
    }

    /// One frame of `samples`, interleaved; returns it with its samples per
    /// channel. Nothing changes when it is refused.
    pub(crate) fn frame(&mut self, samples: &[i16]) -> Result<(Vec<u8>, u32), Mp4Error> {
        let channels = usize::from(self.channels);
        let block = samples.len() / channels;
        if block == 0 || block * channels != samples.len() {
            return Err(Mp4Error::BadFrame("PCM that is not whole sample frames"));
        }
        if block > MAX_BLOCK {
            return Err(Mp4Error::BadFrame("more than 65535 samples per channel in one frame"));
        }
        let mut out = Vec::with_capacity(16 + 2 * samples.len() + channels + 2);
        out.extend_from_slice(&[0xff, 0xf9]); // sync, variable block size
        let block_code = if block <= 256 { 6 } else { 7 };
        out.push((block_code << 4) | rate_code(self.rate));
        // Independent channels, 16 bits per sample.
        out.push(((self.channels as u8 - 1) << 4) | (0b100 << 1));
        coded_number(self.next_sample, &mut out);
        let block_minus_one = block - 1;
        if block_code == 6 {
            out.push(block_minus_one as u8);
        } else {
            out.extend_from_slice(&(block_minus_one as u16).to_be_bytes());
        }
        out.push(crc8(&out));
        for channel in 0..channels {
            out.push(0b0000_0010); // verbatim, no wasted bits
            for sample in samples.iter().skip(channel).step_by(channels) {
                out.extend_from_slice(&sample.to_be_bytes());
            }
        }
        let crc = crc16(&out);
        out.extend_from_slice(&crc.to_be_bytes());
        let block = block as u32;
        self.next_sample = (self.next_sample + u64::from(block)) & SAMPLE_NUMBER_MASK;
        Ok((out, block))
    }
}

/// The `dfLa` box: one STREAMINFO block, the last one. Variable block size,
/// 16 to 65535; frame sizes, total samples and MD5 unknown.
pub(crate) fn dfla(channels: u16, rate: u32) -> Result<Vec<u8>, Mp4Error> {
    Framer::new(channels, rate)?;
    let mut w = crate::boxes::BoxWriter::with_capacity(54);
    let open = w.start_full(b"dfLa", 0, 0);
    w.u8(0x80); // last metadata block, STREAMINFO
    w.u24(34);
    w.u16(16);
    w.u16(MAX_BLOCK as u16);
    w.u24(0);
    w.u24(0);
    // Rate (20 bits), channels - 1 (3), bits per sample - 1 (5), total samples (36).
    let packed = (u64::from(rate) << 44) | (u64::from(channels - 1) << 41) | (15 << 36);
    w.u64(packed);
    w.zeros(16);
    w.end(open)?;
    Ok(w.into_inner())
}

#[cfg(test)]
#[path = "flac_tests.rs"]
mod tests;
