//! Translations between retina's vocabulary and the core's: codecs, track
//! descriptions, errors.

use std::num::NonZeroU32;

use oxrtc_core::{Codec, CodecParams, MediaKind, SourceError, TrackInfo};
use oxrtc_depacketize::{DepacketizerSpec, TrackDepacketizer};
use retina::client::{Stream, StreamDirection};

/// The core codec for an SDP media type and a (lower-case) encoding name, or
/// `None` when oxrtc cannot carry it (ONVIF metadata, G.726, G.723...).
pub(crate) fn map_codec(media: &str, encoding_name: &str) -> Option<Codec> {
    let codec = match (media, encoding_name.to_ascii_lowercase().as_str()) {
        ("video", "h264") => Codec::H264,
        ("video", "h265") => Codec::H265,
        ("video" | "image", "jpeg") => Codec::Jpeg,
        ("audio", "pcma") => Codec::Pcma,
        ("audio", "pcmu") => Codec::Pcmu,
        ("audio", "mpeg4-generic") => Codec::Aac,
        ("audio", "opus") => Codec::Opus,
        ("audio", "l16") => Codec::L16,
        _ => return None,
    };
    Some(codec)
}

/// The codec of a stream oxrtc can receive and carry.
pub(crate) fn stream_codec(stream: &Stream) -> Option<Codec> {
    if stream.direction() != StreamDirection::Receiving {
        return None;
    }
    map_codec(stream.media(), stream.encoding_name())
}

/// What turns the RTP of a stream into frames, cut at `max_frame` bytes for
/// an access unit in progress (R11): retina's own depacketizer, fed the raw
/// packets, and the same code as the publications' (`oxrtc-depacketize`). A
/// stream retina cannot depacketize is the camera's to be refused for.
pub(crate) fn depacketizer_of(
    stream: Option<&Stream>,
    codec: Codec,
    max_frame: usize,
) -> Result<TrackDepacketizer, SourceError> {
    let stream = stream.ok_or_else(|| SourceError::Protocol("a stream that is not in the presentation".into()))?;
    let clock_rate = NonZeroU32::new(stream.clock_rate_hz())
        .ok_or_else(|| SourceError::Protocol(format!("{} stream with a zero clock rate", stream.encoding_name())))?;
    let spec = DepacketizerSpec {
        media: stream.media().to_owned(),
        encoding: stream.encoding_name().to_owned(),
        clock_rate,
        channels: stream.channels(),
        fmtp: stream.format_specific_params().map(str::to_owned),
    };
    TrackDepacketizer::new(spec, codec, max_frame).map_err(|reason| classify(None, &reason))
}

/// Describes a set-up stream as a core track, with the codec configuration
/// its depacketizer knows.
pub(crate) fn track_of(codec: Codec, stream: &Stream, params: CodecParams) -> TrackInfo {
    match codec.kind() {
        MediaKind::Video => TrackInfo {
            kind: MediaKind::Video,
            codec,
            clock_rate: stream.clock_rate_hz(),
            channels: 1,
            params: CodecParams::None,
        }
        .with_params(params),
        MediaKind::Audio => {
            let channels = stream.channels().map_or(1, std::num::NonZeroU16::get);
            TrackInfo::audio(codec, stream.clock_rate_hz(), channels).with_params(params)
        }
    }
}

/// The first line of an error message: retina appends connection and message
/// contexts on further lines, which belong in a debug log, not in a state.
fn headline(text: &str) -> &str {
    text.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or("unknown error")
}

/// Classifies an RTSP failure from its status code (when it came from a
/// response) and its message.
pub(crate) fn classify(status: Option<u16>, text: &str) -> SourceError {
    let line = headline(text);
    if status == Some(401) {
        return SourceError::Auth;
    }
    let unreachable = ["Unable to connect", "Error reading from RTSP peer", "Error writing to RTSP peer", "Timeout"];
    if unreachable.iter().any(|p| line.starts_with(p)) {
        return SourceError::Unreachable(line.to_owned());
    }
    if let Some(what) = line.strip_prefix("Unsupported: ") {
        return SourceError::Unsupported(what.to_owned());
    }
    SourceError::Protocol(line.to_owned())
}

/// Classifies a retina error.
pub(crate) fn classify_error(error: &retina::Error) -> SourceError {
    classify(error.status_code(), &error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_every_carried_encoding() {
        for (media, name, codec) in [
            ("video", "h264", Codec::H264),
            ("video", "H264", Codec::H264),
            ("video", "h265", Codec::H265),
            ("video", "jpeg", Codec::Jpeg),
            ("image", "jpeg", Codec::Jpeg),
            ("audio", "pcma", Codec::Pcma),
            ("audio", "pcmu", Codec::Pcmu),
            ("audio", "mpeg4-generic", Codec::Aac),
            ("audio", "opus", Codec::Opus),
            ("audio", "l16", Codec::L16),
        ] {
            assert_eq!(map_codec(media, name), Some(codec), "{media}/{name}");
        }
    }

    #[test]
    fn skips_what_cannot_be_carried() {
        for (media, name) in [
            ("application", "vnd.onvif.metadata"),
            ("application", "vnd.onvif.metadata+gzip"),
            ("audio", "g726-32"),
            ("audio", "g723"),
            ("audio", "h264"),
            ("video", "pcma"),
            ("video", "mp4v-es"),
            ("text", "t140"),
        ] {
            assert_eq!(map_codec(media, name), None, "{media}/{name}");
        }
    }

    #[test]
    fn classifies_failures() {
        assert_eq!(classify(Some(401), "401 response to DESCRIBE CSeq=2: Unauthorized"), SourceError::Auth);
        assert_eq!(
            classify(None, "Unable to connect to RTSP server: connection refused\n\nconn: x"),
            SourceError::Unreachable("Unable to connect to RTSP server: connection refused".into())
        );
        assert!(matches!(
            classify(None, "Error reading from RTSP peer: reset\n\nconn: x"),
            SourceError::Unreachable(_)
        ));
        assert!(matches!(classify(None, "Error writing to RTSP peer: broken pipe"), SourceError::Unreachable(_)));
        assert_eq!(
            classify(None, "Unsupported: AAC object type 5"),
            SourceError::Unsupported("AAC object type 5".into())
        );
        assert_eq!(
            classify(Some(404), "404 response to DESCRIBE CSeq=1: Not Found\n\nconn: a\nmsg: b"),
            SourceError::Protocol("404 response to DESCRIBE CSeq=1: Not Found".into())
        );
        assert_eq!(classify(None, "\n\n"), SourceError::Protocol("unknown error".into()));
    }

    #[test]
    fn only_unsupported_features_are_final() {
        assert!(classify(Some(401), "x").is_retryable());
        assert!(classify(None, "Unable to connect to RTSP server: x").is_retryable());
        assert!(!classify(None, "Unsupported: x").is_retryable());
    }
}
