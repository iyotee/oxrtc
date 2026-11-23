//! The refusal of an offer that shares no usable codec with the stream, in
//! go2rtc's words (`streams: codecs not matched: <the stream's> => <the
//! offer's>`), so that an operator reads from the message alone what the
//! viewer asked for and what the stream has (D43). One place for WHEP and
//! `/api/ws`, which answer an offer by the same call.

use oxrtc_core::{Stream, TrackInfo};

use crate::WhepError;
use crate::offer::OfferedCodecs;

/// Codec names a refusal lists per side, at most.
const MAX_LISTED: usize = 16;

/// The refusal for `offered`, the stream's side read from `tracks` (what a
/// subscription delivered), or else from what `stream`'s sources showed on
/// their current connections.
pub(crate) fn not_matched(stream: &Stream, tracks: Option<&[TrackInfo]>, offered: &OfferedCodecs) -> WhepError {
    let known: Vec<TrackInfo> = match tracks {
        Some(tracks) => tracks.to_vec(),
        None => stream.snapshot().sources.into_iter().flat_map(|source| source.tracks).collect(),
    };
    WhepError::NoMatchingCodec(text(&known, &offered.named))
}

/// `streams: codecs not matched: video:H264, audio:OPUS => video:VP8`.
pub(crate) fn text(stream: &[TrackInfo], offered: &[String]) -> String {
    let mut have: Vec<String> = Vec::new();
    for track in stream {
        let name = format!("{}:{}", track.kind.as_str(), track.codec.as_str().to_ascii_uppercase());
        if have.len() < MAX_LISTED && !have.contains(&name) {
            have.push(name);
        }
    }
    let have = if have.is_empty() { "none known yet".to_owned() } else { have.join(", ") };
    let asked: Vec<&str> = offered.iter().take(MAX_LISTED).map(String::as_str).collect();
    let asked = if asked.is_empty() { "nothing".to_owned() } else { asked.join(", ") };
    format!("streams: codecs not matched: {have} => {asked}")
}

#[cfg(test)]
mod tests {
    use oxrtc_core::Codec;

    use super::*;

    #[test]
    fn names_the_stream_s_codecs_then_the_offer_s_as_go2rtc() {
        let tracks = [TrackInfo::video(Codec::H264), TrackInfo::audio(Codec::Opus, 48_000, 2)];
        let offered = ["video:VP8".to_owned(), "audio:OPUS".to_owned()];
        assert_eq!(
            text(&tracks, &offered),
            "streams: codecs not matched: video:H264, audio:OPUS => video:VP8, audio:OPUS"
        );
    }

    #[test]
    fn says_when_a_side_has_nothing() {
        assert_eq!(text(&[], &["video:AV1".to_owned()]), "streams: codecs not matched: none known yet => video:AV1");
        assert_eq!(text(&[TrackInfo::video(Codec::H265)], &[]), "streams: codecs not matched: video:H265 => nothing");
    }

    #[test]
    fn a_long_list_is_cut_and_said_once_per_codec() {
        let tracks = vec![TrackInfo::video(Codec::H264); 40];
        let offered: Vec<String> = (0..40).map(|i| format!("video:X{i}")).collect();
        let said = text(&tracks, &offered);
        assert_eq!(said.matches("video:H264").count(), 1);
        assert_eq!(said.matches("video:X").count(), MAX_LISTED);
    }
}
