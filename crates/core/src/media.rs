//! Media vocabulary: kinds, codecs, codec parameters and track descriptions.

use bytes::Bytes;

/// What a track carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum MediaKind {
    /// Pictures.
    Video,
    /// Sound.
    Audio,
}

impl MediaKind {
    /// Both kinds, video first: the order tracks are listed in.
    pub const ALL: [MediaKind; 2] = [MediaKind::Video, MediaKind::Audio];

    /// Lower-case name, as used in queries and SDP (`video`, `audio`).
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Video => "video",
            Self::Audio => "audio",
        }
    }
}

/// A codec rust2rtc can carry. Carrying is not decoding: the core never looks
/// inside a frame beyond the length prefixes of NAL-based codecs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Codec {
    /// H.264 / AVC.
    H264,
    /// H.265 / HEVC.
    H265,
    /// Motion JPEG.
    Jpeg,
    /// Opus.
    Opus,
    /// G.711 A-law.
    Pcma,
    /// G.711 mu-law.
    Pcmu,
    /// AAC (MPEG-4 audio).
    Aac,
    /// Linear 16-bit PCM, big endian.
    L16,
}

impl Codec {
    /// Every codec, in declaration order.
    pub const ALL: [Codec; 8] =
        [Codec::H264, Codec::H265, Codec::Jpeg, Codec::Opus, Codec::Pcma, Codec::Pcmu, Codec::Aac, Codec::L16];

    /// The kind of track this codec belongs to.
    pub fn kind(self) -> MediaKind {
        match self {
            Self::H264 | Self::H265 | Self::Jpeg => MediaKind::Video,
            Self::Opus | Self::Pcma | Self::Pcmu | Self::Aac | Self::L16 => MediaKind::Audio,
        }
    }

    /// Canonical lower-case name.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::H264 => "h264",
            Self::H265 => "h265",
            Self::Jpeg => "jpeg",
            Self::Opus => "opus",
            Self::Pcma => "pcma",
            Self::Pcmu => "pcmu",
            Self::Aac => "aac",
            Self::L16 => "l16",
        }
    }

    /// Parses a codec name, accepting the usual aliases (`hevc`, `alaw`,
    /// `mjpeg`...). Case-insensitive; anything longer than 16 bytes is refused
    /// without being looked at.
    pub fn parse(name: &str) -> Option<Self> {
        if name.len() > 16 {
            return None;
        }
        let codec = match name.to_ascii_lowercase().as_str() {
            "h264" | "avc" => Self::H264,
            "h265" | "hevc" => Self::H265,
            "jpeg" | "mjpeg" => Self::Jpeg,
            "opus" => Self::Opus,
            "pcma" | "alaw" => Self::Pcma,
            "pcmu" | "mulaw" | "ulaw" => Self::Pcmu,
            "aac" | "mpeg4-generic" => Self::Aac,
            "l16" | "pcm" => Self::L16,
            _ => return None,
        };
        Some(codec)
    }

    /// Whether frames of this codec are length-prefixed NAL units.
    pub fn is_nal_based(self) -> bool {
        matches!(self, Self::H264 | Self::H265)
    }
}

/// Out-of-band codec configuration, when the source announced it.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum CodecParams {
    /// Nothing announced (or nothing needed).
    #[default]
    None,
    /// H.264 parameter sets, without start codes or length prefixes.
    H264 {
        /// Sequence parameter set.
        sps: Bytes,
        /// Picture parameter set.
        pps: Bytes,
    },
    /// H.265 parameter sets, without start codes or length prefixes.
    H265 {
        /// Video parameter set.
        vps: Bytes,
        /// Sequence parameter set.
        sps: Bytes,
        /// Picture parameter set.
        pps: Bytes,
    },
    /// AAC `AudioSpecificConfig`.
    Aac {
        /// The raw config bytes.
        config: Bytes,
    },
}

/// One track of a source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrackInfo {
    /// Video or audio.
    pub kind: MediaKind,
    /// Its codec.
    pub codec: Codec,
    /// RTP clock rate, in Hz: the unit of [`crate::Frame::pts`].
    pub clock_rate: u32,
    /// Audio channels (1 for video).
    pub channels: u16,
    /// Codec configuration, if known.
    pub params: CodecParams,
}

impl TrackInfo {
    /// A video track at the standard 90 kHz clock.
    pub fn video(codec: Codec) -> Self {
        Self { kind: MediaKind::Video, codec, clock_rate: 90_000, channels: 1, params: CodecParams::None }
    }

    /// An audio track.
    pub fn audio(codec: Codec, clock_rate: u32, channels: u16) -> Self {
        Self { kind: MediaKind::Audio, codec, clock_rate, channels, params: CodecParams::None }
    }

    /// The same track with its codec configuration.
    pub fn with_params(mut self, params: CodecParams) -> Self {
        self.params = params;
        self
    }
}
