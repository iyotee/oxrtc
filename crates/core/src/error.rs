//! Error types of the core.

use thiserror::Error;

/// A frame that is not well formed.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum FrameError {
    /// No bytes at all.
    #[error("empty frame")]
    Empty,
    /// Fewer than four bytes left where a length prefix should be.
    #[error("truncated length prefix")]
    TruncatedLength,
    /// A length prefix of zero.
    #[error("empty NAL unit")]
    EmptyNal,
    /// A length prefix larger than what follows it.
    #[error("NAL unit declares {declared} bytes, {available} remain")]
    TruncatedNal {
        /// Declared length.
        declared: usize,
        /// Bytes actually present.
        available: usize,
    },
}

/// Why a driver's publication was refused.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum PublishError {
    /// A frame was published before any track list.
    #[error("frame published before the track list")]
    NoTracks,
    /// The frame names a track the source did not announce.
    #[error("frame for unknown track {0}")]
    UnknownTrack(u8),
    /// The frame is larger than [`crate::Limits::max_frame_bytes`].
    #[error("frame of {size} bytes exceeds the limit of {limit}")]
    TooLarge {
        /// Frame size.
        size: usize,
        /// The limit.
        limit: usize,
    },
    /// The track list is empty or too long.
    #[error("invalid track list: {0}")]
    InvalidTracks(&'static str),
    /// The payload is malformed.
    #[error("malformed frame: {0}")]
    Malformed(#[from] FrameError),
}

/// A source description that cannot be used.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum SpecError {
    /// Empty string.
    #[error("empty source")]
    Empty,
    /// Longer than [`crate::spec::MAX_SPEC_LEN`].
    #[error("source longer than {0} bytes")]
    TooLong(usize),
    /// A control character (newline, NUL...) inside the source.
    #[error("control character in source")]
    ControlCharacter,
    /// No valid `scheme:` prefix.
    #[error("source has no valid scheme")]
    BadScheme,
    /// No driver registered for the scheme.
    #[error("unsupported source scheme {0:?}")]
    UnknownScheme(String),
    /// A stream that reads itself.
    #[error("a stream cannot read itself")]
    SelfReference,
    /// The driver refused the source.
    #[error("invalid source: {0}")]
    Invalid(String),
}

/// Why a stream could not be created or replaced.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum HubError {
    /// Stream names are 1 to 128 characters of `A-Z a-z 0-9 _ . -`.
    #[error("invalid stream name")]
    BadName,
    /// [`crate::Limits::max_streams`] reached.
    #[error("too many streams")]
    TooManyStreams,
    /// A stream needs at least one source.
    #[error("a stream needs at least one source")]
    NoSources,
    /// [`crate::Limits::max_sources_per_stream`] exceeded.
    #[error("too many sources")]
    TooManySources,
    /// One of the sources is unusable.
    #[error("source {index}: {error}")]
    Spec {
        /// Position of the source in the list.
        index: usize,
        /// What is wrong with it.
        error: SpecError,
    },
}

/// Why a subscription could not be made.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum SubscribeError {
    /// The [`crate::Want`] asks for no media.
    #[error("nothing wanted")]
    NothingWanted,
    /// No source offers a wanted codec.
    #[error("no source offers the wanted codecs")]
    NoMatchingSource,
    /// [`crate::Limits::max_consumers_per_stream`] reached.
    #[error("too many consumers")]
    TooManyConsumers,
    /// The stream was removed or replaced.
    #[error("stream closed")]
    Closed,
}

/// Why a subscription ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum SubEnd {
    /// The stream was removed or replaced.
    #[error("stream closed")]
    Closed,
    /// No source delivered a wanted track by the probe deadline (go2rtc
    /// answers an error at once when its sources fail to dial).
    #[error("no source delivered a wanted track")]
    NoTracks,
}

/// A query that cannot be turned into a [`crate::Want`].
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum WantError {
    /// A codec name nobody knows.
    #[error("unknown codec {0:?}")]
    UnknownCodec(String),
    /// A video codec asked for as audio, or the reverse.
    #[error("codec {codec} is not {kind}")]
    WrongKind {
        /// The codec.
        codec: &'static str,
        /// The media it was asked for.
        kind: &'static str,
    },
}
