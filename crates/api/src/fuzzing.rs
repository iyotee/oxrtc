//! Harnesses for the fuzz targets of `fuzz/` (feature `fuzzing`) and their
//! stable property tests (`fuzzing_tests.rs`). Each returns the invariant
//! that broke; a panic is a defect by itself.

use oxrtc_core::{Codec, MediaKind, MediaWant};

use crate::ws::message::{self, Dialect, Incoming, Outgoing};
use crate::ws::mse;

/// Any text frame through the parser of `/api/ws`'s messages: an offer
/// carries the value it was sent with, a candidate is a non-empty line with
/// no `a=` and no line end, an `mse` codec list makes a wish of known codecs
/// of the right kinds, each once, and every message the server writes from
/// what was read is JSON go2rtc's clients parse back to the same fields.
pub fn ws_message(data: &[u8]) -> Result<(), String> {
    let Ok(text) = std::str::from_utf8(data) else { return Ok(()) };
    let Ok(incoming) = message::parse(text) else { return Ok(()) };
    let sent: serde_json::Value = serde_json::from_str(text).map_err(|e| format!("parsed but not JSON: {e}"))?;
    match incoming {
        Incoming::Offer { sdp, dialect } => {
            let given = match dialect {
                Dialect::Sdp => sent.get("value").and_then(serde_json::Value::as_str),
                Dialect::Object => sent.get("value").and_then(|v| v.get("sdp")).and_then(serde_json::Value::as_str),
            };
            if given != Some(sdp.as_str()) {
                return Err(format!("the offer read is not the one sent: {sdp:?}"));
            }
            if sdp.trim().is_empty() {
                return Err("an empty offer taken".into());
            }
            let answer = Outgoing::Answer { sdp: sdp.clone(), dialect };
            let written: serde_json::Value =
                serde_json::from_str(&answer.to_json()).map_err(|e| format!("an answer that is not JSON: {e}"))?;
            let back = match dialect {
                Dialect::Sdp => written.get("value").and_then(serde_json::Value::as_str),
                Dialect::Object => written.get("value").and_then(|v| v.get("sdp")).and_then(serde_json::Value::as_str),
            };
            if back != Some(sdp.as_str()) {
                return Err("an answer that does not carry its SDP".into());
            }
            for line in message::answer_candidates(&sdp) {
                if !line.starts_with("candidate:") {
                    return Err(format!("an answer candidate {line:?}"));
                }
            }
        }
        Incoming::Candidate(Some(line)) => {
            if line.is_empty() || line.starts_with("a=") || line.contains(['\r', '\n']) || line.trim() != line {
                return Err(format!("a candidate line {line:?}"));
            }
            let written = Outgoing::Candidate(line.clone()).to_json();
            let back: serde_json::Value =
                serde_json::from_str(&written).map_err(|e| format!("a candidate that is not JSON: {e}"))?;
            if back.get("value").and_then(serde_json::Value::as_str) != Some(line.as_str()) {
                return Err("a candidate written otherwise than read".into());
            }
        }
        Incoming::BadOffer { dialect, reason } => {
            let error = Outgoing::Error { about: dialect.offer_type(), reason: reason.to_owned() }.to_json();
            let back: serde_json::Value =
                serde_json::from_str(&error).map_err(|e| format!("an error that is not JSON: {e}"))?;
            let value = back.get("value").and_then(serde_json::Value::as_str).unwrap_or_default();
            if !value.starts_with(dialect.offer_type()) {
                return Err(format!("an error about an offer that does not name it: {value:?}"));
            }
        }
        Incoming::Later(later) => {
            if sent.get("type").and_then(serde_json::Value::as_str) != Some(later.name()) {
                return Err(format!("{later:?} read from another type"));
            }
        }
        Incoming::Mse(codecs) => mse_wish(&codecs)?,
        Incoming::Candidate(None) | Incoming::Ignored => {}
    }
    Ok(())
}

/// A codec list's wish: each codec once, of its media's kind, the PCM family
/// whole (FLAC is all of it or none), and nothing asked that no name said.
fn mse_wish(codecs: &str) -> Result<(), String> {
    let wish = mse::wish(codecs);
    let named = codecs.split(',').any(|n| !n.trim().is_empty());
    for kind in [MediaKind::Video, MediaKind::Audio] {
        let MediaWant::OneOf(list) = wish.want.get(kind) else {
            if *wish.want.get(kind) == MediaWant::Any {
                return Err(format!("{codecs:?} asks for any {kind:?}"));
            }
            continue;
        };
        if list.is_empty() {
            return Err("an empty list of codecs is not a wish".into());
        }
        for (at, codec) in list.iter().enumerate() {
            if codec.kind() != kind || list.iter().skip(at + 1).any(|c| c == codec) {
                return Err(format!("{codecs:?} made {list:?} for {kind:?}"));
            }
        }
        let pcm = [Codec::Pcma, Codec::Pcmu, Codec::L16];
        if pcm.iter().any(|c| list.contains(c)) != pcm.iter().all(|c| list.contains(c)) {
            return Err(format!("{codecs:?} took part of the PCM family"));
        }
        let families: &[&str] = match kind {
            MediaKind::Video => &["avc1", "avc3", "hvc1", "hev1"],
            MediaKind::Audio => &["mp4a.40", "opus", "flac"],
        };
        let lower = codecs.to_ascii_lowercase();
        if named && !families.iter().any(|f| lower.contains(f)) {
            return Err(format!("{codecs:?} asks for {kind:?} no name said"));
        }
    }
    Ok(())
}

#[cfg(test)]
#[path = "fuzzing_tests.rs"]
mod tests;
