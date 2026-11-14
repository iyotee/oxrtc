//! Translations between retina's vocabulary and the core's: codecs, track
//! descriptions, errors.

use bytes::Bytes;
use oxrtc_core::{Codec, CodecParams, MediaKind, SourceError, TrackInfo};
use retina::client::{Stream, StreamDirection};
use retina::codec::{ParametersRef, VideoParametersCodec};

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

/// The codec configuration retina knows for a stream so far.
pub(crate) fn params_of(codec: Codec, params: Option<ParametersRef<'_>>) -> CodecParams {
    match params {
        Some(ParametersRef::Video(video)) => match video.codec_params() {
            VideoParametersCodec::H264 { sps, pps } if codec == Codec::H264 => {
                CodecParams::H264 { sps: sps.clone(), pps: pps.clone() }
            }
            VideoParametersCodec::H265 { vps, sps, pps } if codec == Codec::H265 => {
                CodecParams::H265 { vps: vps.clone(), sps: sps.clone(), pps: pps.clone() }
            }
            _ => CodecParams::None,
        },
        Some(ParametersRef::Audio(audio)) if codec == Codec::Aac && !audio.extra_data().is_empty() => {
            CodecParams::Aac { config: Bytes::copy_from_slice(audio.extra_data()) }
        }
        _ => CodecParams::None,
    }
}

/// Describes a set-up stream as a core track.
pub(crate) fn track_of(codec: Codec, stream: &Stream) -> TrackInfo {
    let params = params_of(codec, stream.parameters());
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
    fn unknown_params_stay_none() {
        assert_eq!(params_of(Codec::H264, None), CodecParams::None);
        assert_eq!(params_of(Codec::Pcma, None), CodecParams::None);
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
