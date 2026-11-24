//! `{"type":"mse","value":"<codecs>"}` on `/api/ws` (M5 F3): the stream as
//! fragmented MP4 over the socket, for a browser's Media Source Extensions
//! (go2rtc's player in `mse` mode, the WebRTC Camera card, a browser without
//! WebRTC).
//!
//! The value lists the codecs the browser's `MediaSource.isTypeSupported` said
//! yes to. It makes the subscription's [`Want`], so that the sources are
//! chosen by the same rule as for a WebRTC offer: the first source of the
//! stream that offers an accepted codec (a camera's own H.265 for a browser
//! that lists `hvc1` or `hev1`, the H.264 transcode after it otherwise). The
//! answer is `{"type":"mse","value":"video/mp4; codecs=\"...\""}` with the
//! codecs the stream really has, then binary frames: an init segment, then
//! one fragment per frame from a keyframe on.
//!
//! The subscription and its muxer are `crate::fmp4`'s, which
//! `stream.mp4` shares: its queue is the socket's, so a reader slower than
//! the stream loses frames and resumes at a keyframe.

use oxrtc_core::h265_receiver::HevcReceiver;
use oxrtc_core::{Codec, MediaWant, Want};
use oxrtc_mp4::HevcEntry;

pub use crate::fmp4::{LOSS_REPORT_EVERY, Wish};

/// Most names read from one codec list; the rest is not looked at.
pub const MAX_CODECS: usize = 32;

/// go2rtc's name for an MSE consumer, which `/api/streams` lists.
pub const CONSUMER_KIND: &str = "mse/fmp4";

/// Reads a codec list (RFC 6381 names, comma-separated, as go2rtc's player
/// writes them): `avc1`/`avc3` (H.264), `hvc1`/`hev1` (H.265), `mp4a.40`
/// (AAC), `opus`, `flac` (G.711 A-law and mu-law, and L16, written as FLAC).
/// A family picks the sources whatever its profile and level, as a WebRTC
/// offer's codecs do; an H.265 entry's profile, tier and level then say
/// whether the camera's own H.265 is sent or the stream's H.264 in its place
/// (ISO/IEC 14496-15 annex E: go2rtc's player lists `hvc1.1.6.L153.B0`, Main
/// at level 5.1; the rule WebRTC follows, D44; an entry that is not such a
/// string names no limit). An empty list asks what go2rtc gives a client
/// that sends none: H.264 or H.265, and AAC, unchecked. A list with nothing
/// known asks for nothing.
pub fn wish(list: &str) -> Wish {
    let mut listed = Vec::new();
    let mut receivers = Vec::new();
    let (mut hvc1, mut hev1, mut named) = (false, false, false);
    for name in list.split(',').map(str::trim).filter(|n| !n.is_empty()).take(MAX_CODECS) {
        named = true;
        let (family, rest) = name.split_once('.').unwrap_or((name, ""));
        let family = family.to_ascii_lowercase();
        match family.as_str() {
            "avc1" | "avc3" => listed.push(Codec::H264),
            "hvc1" | "hev1" => {
                if family == "hvc1" {
                    hvc1 = true;
                } else {
                    hev1 = true;
                }
                listed.push(Codec::H265);
                receivers.push(HevcReceiver::from_codec_string(name).unwrap_or(HevcReceiver::ANY));
            }
            "mp4a" if rest == "40" || rest.starts_with("40.") => listed.push(Codec::Aac),
            "opus" => listed.push(Codec::Opus),
            "flac" => listed.extend([Codec::Pcma, Codec::Pcmu, Codec::L16]),
            _ => {}
        }
    }
    if !named {
        return Wish {
            want: Want {
                video: MediaWant::OneOf(vec![Codec::H264, Codec::H265]),
                audio: MediaWant::OneOf(vec![Codec::Aac]),
            },
            hevc: HevcEntry::Hvc1,
            receivers: None,
        };
    }
    // The sources are then chosen by the order every egress shares, as for
    // a WebRTC offer.
    Wish {
        want: Want::preferred(&listed),
        hevc: if hev1 && !hvc1 { HevcEntry::Hev1 } else { HevcEntry::Hvc1 },
        receivers: Some(receivers),
    }
}

/// `mp4`'s wish (F2): the list's video alone, read as [`wish`] reads it
/// (go2rtc's `mp4.ParseCodecs` with no sound); an empty list H.264 or H.265,
/// go2rtc's default. Each keyframe is a file of its own, which a player shows
/// as a picture: a sound track would only lengthen every file.
pub fn mp4_wish(list: &str) -> Wish {
    let mut wish = wish(list);
    wish.want.audio = MediaWant::Skip;
    wish
}

#[cfg(test)]
#[path = "mse_tests.rs"]
mod tests;
