//! A track's frames as the elementary stream MPEG-TS carries: H.264 and
//! H.265 in Annex B (ITU-T H.264 and H.265, annex B) behind an access unit
//! delimiter, AAC behind ADTS headers (ISO/IEC 14496-3, 1.A.2), Opus behind
//! its control header (the Opus project's "Encapsulation of Opus in MPEG-2
//! Transport Stream", 6.1).

use bytes::Bytes;
use oxrtc_core::aac::AudioConfig;
use oxrtc_core::frame::{NalUnits, h264_nal_type, h265_nal_type, validate_avcc};
use oxrtc_core::{Codec, CodecParams, TrackInfo};

use crate::TsError;

const START_CODE: [u8; 4] = [0, 0, 0, 1];
/// H.264's access unit delimiter, any picture type.
const H264_AUD: [u8; 2] = [0x09, 0xF0];
/// H.265's access unit delimiter (type 35, layer 0, temporal id 1), any picture type.
const H265_AUD: [u8; 3] = [0x46, 0x01, 0x50];
/// Largest access unit an ADTS header's 13-bit length can count.
const MAX_ADTS_FRAME: usize = 0x1FFF - 7;

/// How a track's frames are written.
#[derive(Debug, Clone)]
pub(crate) enum Es {
    /// H.264, the parameter sets the track description gives.
    H264 { sets: Vec<Bytes> },
    /// H.265, likewise.
    H265 { sets: Vec<Bytes> },
    /// AAC: the six bytes of the ADTS header every frame shares (its length
    /// apart).
    Aac { profile: u8, rate_index: u8, channels: u8 },
    /// Opus.
    Opus,
}

impl Es {
    /// The elementary stream of `track`, or why MPEG-TS does not carry it.
    pub(crate) fn of(track: &TrackInfo) -> Result<Self, TsError> {
        if track.codec.kind() != track.kind {
            return Err(TsError::Unsupported("a codec of the other kind"));
        }
        match (track.codec, &track.params) {
            (Codec::H264, CodecParams::H264 { sps, pps }) => Ok(Self::H264 { sets: vec![sps.clone(), pps.clone()] }),
            (Codec::H264, _) => Ok(Self::H264 { sets: Vec::new() }),
            (Codec::H265, CodecParams::H265 { vps, sps, pps }) => {
                Ok(Self::H265 { sets: vec![vps.clone(), sps.clone(), pps.clone()] })
            }
            (Codec::H265, _) => Ok(Self::H265 { sets: Vec::new() }),
            (Codec::Aac, CodecParams::Aac { config }) => {
                let parsed =
                    AudioConfig::parse(config).map_err(|_| TsError::Unsupported("an unreadable AAC config"))?;
                let profile = match parsed.core_object_type {
                    1..=4 => parsed.core_object_type - 1,
                    _ => return Err(TsError::Unsupported("an AAC object type ADTS cannot name")),
                };
                let rate_index = parsed.core_rate_index.ok_or(TsError::Unsupported("an AAC rate ADTS cannot name"))?;
                let channels = match parsed.channel_configuration {
                    1..=7 => parsed.channel_configuration,
                    _ => return Err(TsError::Unsupported("an AAC channel layout ADTS cannot name")),
                };
                Ok(Self::Aac { profile, rate_index, channels })
            }
            (Codec::Aac, _) => Err(TsError::Unsupported("AAC with no AudioSpecificConfig")),
            (Codec::Opus, _) if track.clock_rate != 48_000 => {
                Err(TsError::Unsupported("Opus at a clock other than 48 kHz"))
            }
            (Codec::Opus, _) if !(1..=2).contains(&track.channels) => {
                Err(TsError::Unsupported("Opus beyond two channels"))
            }
            (Codec::Opus, _) => Ok(Self::Opus),
            (Codec::Pcma | Codec::Pcmu | Codec::L16, _) => Err(TsError::Unsupported("PCM in MPEG-TS")),
            (Codec::Jpeg, _) => Err(TsError::Unsupported("Motion JPEG in MPEG-TS")),
        }
    }

    /// The stream type of the program map table, and the stream's descriptors.
    pub(crate) fn stream_type(&self, channels: u16) -> (u8, Vec<u8>) {
        match self {
            Self::H264 { .. } => (0x1B, Vec::new()),
            Self::H265 { .. } => (0x24, Vec::new()),
            Self::Aac { .. } => (0x0F, Vec::new()),
            // registration_descriptor 'Opus', then the DVB extension
            // descriptor of Opus (tag 0x80) with its channel configuration.
            Self::Opus => (0x06, vec![0x05, 4, b'O', b'p', b'u', b's', 0x7F, 2, 0x80, channels.min(2) as u8]),
        }
    }

    /// Whether frames are pictures.
    pub(crate) fn video(&self) -> bool {
        matches!(self, Self::H264 { .. } | Self::H265 { .. })
    }

    /// The bytes of one frame.
    pub(crate) fn write(&self, data: &[u8], keyframe: bool) -> Result<Vec<u8>, TsError> {
        if data.len() > crate::MAX_FRAME_BYTES {
            return Err(TsError::TooLarge(data.len()));
        }
        match self {
            Self::H264 { sets } => annex_b(data, keyframe, sets, &H264_AUD, h264_nal_type, 9, &[7]),
            Self::H265 { sets } => annex_b(data, keyframe, sets, &H265_AUD, h265_nal_type, 35, &[32, 33]),
            Self::Aac { profile, rate_index, channels } => {
                if data.is_empty() {
                    return Err(TsError::BadFrame("an empty AAC access unit"));
                }
                if data.len() > MAX_ADTS_FRAME {
                    return Err(TsError::TooLarge(data.len()));
                }
                let length = data.len() + 7;
                let mut out = Vec::with_capacity(length);
                out.extend_from_slice(&[
                    0xFF,
                    // MPEG-4, layer 0, no CRC.
                    0xF1,
                    (profile << 6) | (rate_index << 2) | (channels >> 2),
                    ((channels & 3) << 6) | ((length >> 11) & 0x03) as u8,
                    ((length >> 3) & 0xFF) as u8,
                    // The length's last three bits, a buffer fullness of 0x7FF (variable).
                    (((length & 7) as u8) << 5) | 0x1F,
                    0xFC,
                ]);
                out.extend_from_slice(data);
                Ok(out)
            }
            Self::Opus => {
                if data.is_empty() {
                    return Err(TsError::BadFrame("an empty Opus packet"));
                }
                // control_header_prefix 0x3FF, no trim, no extension.
                let mut out = vec![0x7F, 0xE0];
                out.resize(2 + data.len() / 255, 0xFF);
                out.push((data.len() % 255) as u8);
                out.extend_from_slice(data);
                Ok(out)
            }
        }
    }
}

/// An H.264 or H.265 frame in Annex B: a delimiter first unless it has one,
/// the track's parameter sets before a keyframe that carries none of them.
fn annex_b(
    data: &[u8],
    keyframe: bool,
    sets: &[Bytes],
    delimiter: &[u8],
    kind: impl Fn(&[u8]) -> Option<u8>,
    delimiter_type: u8,
    set_types: &[u8],
) -> Result<Vec<u8>, TsError> {
    validate_avcc(data).map_err(|_| TsError::BadFrame("NAL units whose length prefixes do not add up"))?;
    let mut out = Vec::with_capacity(data.len() + 64);
    let mut units = NalUnits::new(data).peekable();
    if units.peek().and_then(|nal| kind(nal)) != Some(delimiter_type) {
        out.extend_from_slice(&START_CODE);
        out.extend_from_slice(delimiter);
    }
    let has_sets = NalUnits::new(data).any(|nal| kind(nal).is_some_and(|t| set_types.contains(&t)));
    let mut sets_written = !keyframe || has_sets;
    for nal in units {
        // After the frame's own delimiter, if it has one, before anything else.
        if !sets_written && kind(nal) != Some(delimiter_type) {
            for set in sets.iter().filter(|s| !s.is_empty()) {
                out.extend_from_slice(&START_CODE);
                out.extend_from_slice(set);
            }
            sets_written = true;
        }
        out.extend_from_slice(&START_CODE);
        out.extend_from_slice(nal);
    }
    Ok(out)
}

#[cfg(test)]
#[path = "es_tests.rs"]
mod tests;
