//! One track of a writer: its sample entry, codec string and clock, and how
//! a frame of it becomes a sample.

use bytes::Bytes;
use oxrtc_core::frame::validate_avcc;
use oxrtc_core::{Codec, CodecParams, MediaKind, TrackInfo};

use crate::boxes::{audio_entry, visual_entry};
use crate::flac::{Framer, Pcm};
use crate::{Mp4Error, aac, flac, h264, h265, opus};

/// How a frame of the track becomes a sample.
#[derive(Debug, Clone)]
pub(crate) enum Packing {
    /// Length-prefixed NAL units, as the core carries them and MP4 stores them.
    Nal,
    /// AAC access units, each lasting `frame` ticks.
    Aac { frame: u32 },
    /// Opus packets, lasting what their table of contents says.
    Opus,
    /// PCM decoded and written as FLAC frames.
    Flac { pcm: Pcm, framer: Framer, channels: u16 },
}

/// A track's fixed description and its packing state.
#[derive(Debug, Clone)]
pub(crate) struct Track {
    pub kind: MediaKind,
    pub timescale: u32,
    pub width: u16,
    pub height: u16,
    /// The complete sample entry box, for `stsd`.
    pub entry: Vec<u8>,
    pub codec: String,
    pub packing: Packing,
}

/// Whether a track of this codec can be written.
pub fn supported(track: &TrackInfo) -> bool {
    track.codec != Codec::Jpeg
}

/// The RFC 6381 codec string a track would have (`avc1.64001f`, `hvc1.1.6.L120.90`,
/// `mp4a.40.2`, `opus`, `flac`), for MSE's `isTypeSupported` and HLS's `CODECS`.
pub fn codec_string(track: &TrackInfo) -> Result<String, Mp4Error> {
    Track::new(track, 1, crate::HevcEntry::Hvc1).map(|t| t.codec)
}

fn dimension(value: u32) -> Result<u16, Mp4Error> {
    u16::try_from(value).map_err(|_| Mp4Error::invalid("picture size", "over 65535 pixels"))
}

impl Track {
    pub(crate) fn new(info: &TrackInfo, id: u32, hevc: crate::HevcEntry) -> Result<Self, Mp4Error> {
        if info.codec.kind() != info.kind {
            return Err(Mp4Error::invalid("track", "a codec of the other kind"));
        }
        let es_id = u16::try_from(id).unwrap_or(0);
        let audio = |kind: &[u8; 4], rate: u32, config: Vec<u8>, codec: &str, packing: Packing| {
            Ok(Self {
                kind: MediaKind::Audio,
                timescale: rate,
                width: 0,
                height: 0,
                entry: audio_entry(kind, info.channels, rate, &config)?,
                codec: codec.to_owned(),
                packing,
            })
        };
        if info.clock_rate == 0 {
            return Err(Mp4Error::invalid("track", "a clock rate of 0"));
        }
        match (info.codec, &info.params) {
            (Codec::H264, CodecParams::H264 { sps, pps }) => {
                let parsed = h264::parse_sps(sps)?;
                let config = h264::avcc(sps, pps, &parsed)?;
                let (width, height) = (dimension(parsed.width)?, dimension(parsed.height)?);
                Ok(Self {
                    kind: MediaKind::Video,
                    timescale: info.clock_rate,
                    width,
                    height,
                    entry: visual_entry(b"avc1", width, height, &config)?,
                    codec: h264::codec_string(&parsed),
                    packing: Packing::Nal,
                })
            }
            (Codec::H265, CodecParams::H265 { vps, sps, pps }) => {
                let parsed = h265::parse_sps(sps)?;
                let config = h265::hvcc(vps, sps, pps, &parsed)?;
                let (width, height) = (dimension(parsed.width)?, dimension(parsed.height)?);
                let (tag, kind) = match hevc {
                    crate::HevcEntry::Hvc1 => ("hvc1", b"hvc1"),
                    crate::HevcEntry::Hev1 => ("hev1", b"hev1"),
                };
                Ok(Self {
                    kind: MediaKind::Video,
                    timescale: info.clock_rate,
                    width,
                    height,
                    entry: visual_entry(kind, width, height, &config)?,
                    codec: h265::codec_string(tag, &parsed.ptl),
                    packing: Packing::Nal,
                })
            }
            (Codec::H264, _) => Err(Mp4Error::MissingConfig("H.264 SPS and PPS")),
            (Codec::H265, _) => Err(Mp4Error::MissingConfig("H.265 VPS, SPS and PPS")),
            (Codec::Aac, CodecParams::Aac { config }) => {
                let parsed = aac::parse(config)?;
                let packing = Packing::Aac { frame: aac::frame_duration(&parsed, info.clock_rate) };
                audio(b"mp4a", info.clock_rate, aac::esds(config, es_id)?, &aac::codec_string(&parsed), packing)
            }
            (Codec::Aac, _) => Err(Mp4Error::MissingConfig("AAC AudioSpecificConfig")),
            (Codec::Opus, _) if info.clock_rate != opus::RATE => {
                Err(Mp4Error::Unsupported("Opus at a clock other than 48 kHz"))
            }
            (Codec::Opus, _) => audio(b"Opus", opus::RATE, opus::dops(info.channels)?, "opus", Packing::Opus),
            (Codec::Pcma | Codec::Pcmu | Codec::L16, _) => {
                let pcm = Pcm::of(info.codec).ok_or(Mp4Error::Unsupported("this PCM coding"))?;
                let framer = Framer::new(info.channels, info.clock_rate)?;
                let config = flac::dfla(info.channels, info.clock_rate)?;
                audio(b"fLaC", info.clock_rate, config, "flac", Packing::Flac { pcm, framer, channels: info.channels })
            }
            (Codec::Jpeg, _) => Err(Mp4Error::Unsupported("Motion JPEG")),
        }
    }

    /// The sample a frame becomes, and its length when its content says it.
    /// A refused frame leaves the track as it was.
    pub(crate) fn pack(&mut self, data: &Bytes) -> Result<(Bytes, Option<u32>), Mp4Error> {
        if data.len() > crate::MAX_SAMPLE_BYTES {
            return Err(Mp4Error::TooLarge(data.len()));
        }
        match &mut self.packing {
            Packing::Nal => {
                validate_avcc(data).map_err(|_| Mp4Error::BadFrame("NAL units whose length prefixes do not add up"))?;
                Ok((data.clone(), None))
            }
            Packing::Aac { frame } => {
                if data.is_empty() {
                    return Err(Mp4Error::BadFrame("an empty AAC access unit"));
                }
                Ok((data.clone(), Some(*frame)))
            }
            Packing::Opus => {
                let samples = opus::packet_samples(data)?;
                Ok((data.clone(), Some(samples)))
            }
            Packing::Flac { pcm, framer, channels } => {
                let mut samples = Vec::with_capacity(data.len());
                pcm.decode(data, *channels, &mut samples)?;
                let (frame, block) = framer.frame(&samples)?;
                Ok((Bytes::from(frame), Some(block)))
            }
        }
    }
}

#[cfg(test)]
#[path = "track_tests.rs"]
pub(crate) mod tests;
