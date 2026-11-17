//! One `MediaStream` per session: the stream and track ids of an answer.
//!
//! A browser builds one `MediaStream` per stream id it reads in the remote
//! description (`a=msid:<stream> <track>`, RFC 8830). str0m names each media
//! line of an answer on its own: ids drawn per line, or the ids of the
//! offer's line when it carries some. A session's picture and sound then
//! arrive as two streams, and the player many WHEP and go2rtc clients are,
//! `video.srcObject = event.streams[0]` at every track event, ends on the
//! sound's stream with no picture (openIVR's live view, 2026-11-17). go2rtc
//! and MediaMTX name one stream for both.
//!
//! str0m (0.25) has no way to choose these ids for an answer:
//! `Media::set_msid` is private and `SdpApi::add_media` takes them only for
//! an offer of its own. They are therefore rewritten in the answer's text,
//! which is the wire format a browser reads whatever str0m holds inside: the
//! one alternative, `a=msid` lines added to the offer before str0m reads it,
//! leans on str0m copying the offer's ids into its answer, which no document
//! promises, and edits the peer's description.
//!
//! The stream id is this server's, one per session ([`new_stream_id`]): the
//! streams of two cameras on one page are told apart, and an id an offer
//! sent is never handed back as the server's. The picture and the sound in
//! one stream are also what a browser synchronises (libwebrtc pairs the
//! audio and the video it plays together by their stream id).

use crate::resource::new_resource_id;

/// A fresh stream id: 128 random bits in hex, as a resource id is drawn, and
/// another draw (a page reads its stream's id; a resource id ends a session).
/// 32 characters of RFC 8830's `msid-id` (at most 64 token characters).
pub(crate) fn new_stream_id() -> Option<String> {
    new_resource_id()
}

/// The answer with every media line naming `stream`, and in it a track of
/// its own: `<stream>-<n>`, `n` being the line's position in the answer.
///
/// Rewritten: `a=msid:` and the `msid:` attribute of every `a=ssrc:` (a
/// picture's SSRC and the SSRC of its retransmissions name the same track),
/// and `a=msid-semantic:`, which lists `stream` alone. Every other line, and
/// every line ending, is kept byte for byte; an answer that names no stream
/// at all is returned as it is.
pub(crate) fn one_stream(answer: &str, stream: &str) -> String {
    if !answer.split_inclusive('\n').any(names_a_stream) {
        return answer.to_owned();
    }
    let mut out = String::with_capacity(answer.len() + 128);
    // The position of the media line being read; `None` in the session part.
    let mut media: Option<usize> = None;
    for line in answer.split_inclusive('\n') {
        let text = line.trim_end_matches(['\r', '\n']);
        let ending = line.get(text.len()..).unwrap_or_default();
        if text.starts_with("m=") {
            media = Some(media.map_or(0, |n| n + 1));
        }
        match rewritten(text, stream, media) {
            Some(new) => out.push_str(&new),
            None => out.push_str(text),
        }
        out.push_str(ending);
    }
    out
}

/// Whether `line` names a stream for a media line.
fn names_a_stream(line: &str) -> bool {
    line.starts_with("a=msid:") || ssrc_msid(line).is_some()
}

/// The SSRC of an `a=ssrc:<ssrc> msid:...` line.
fn ssrc_msid(line: &str) -> Option<&str> {
    let (ssrc, attribute) = line.strip_prefix("a=ssrc:")?.split_once(' ')?;
    attribute.starts_with("msid:").then_some(ssrc)
}

/// `text` as it is written with `stream`, when it is a line that names one.
/// A line of the session part names the session's streams; a line of media
/// line `media` names that line's stream and track.
fn rewritten(text: &str, stream: &str, media: Option<usize>) -> Option<String> {
    let Some(n) = media else {
        return text.starts_with("a=msid-semantic:").then(|| format!("a=msid-semantic: WMS {stream}"));
    };
    if text.starts_with("a=msid:") {
        return Some(format!("a=msid:{stream} {stream}-{n}"));
    }
    ssrc_msid(text).map(|ssrc| format!("a=ssrc:{ssrc} msid:{stream} {stream}-{n}"))
}

#[cfg(test)]
#[path = "msid_tests.rs"]
mod tests;
