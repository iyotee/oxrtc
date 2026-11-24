//! Harnesses for the fuzz targets of `fuzz/` (feature `fuzzing`) and their
//! stable property tests (`fuzzing_tests.rs`). Each returns the invariant
//! that broke; a panic is a defect by itself.

use oxrtc_core::h265_receiver::HevcReceiver;
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
        Incoming::Mp4(codecs) => {
            if sent.get("type").and_then(serde_json::Value::as_str) != Some("mp4") {
                return Err("mp4 read from another type".into());
            }
            let wish = crate::ws::mse::mp4_wish(&codecs);
            if wish.want.audio != oxrtc_core::MediaWant::Skip {
                return Err(format!("mp4 {codecs:?} asked for sound: {:?}", wish.want));
            }
        }
        Incoming::Mse(codecs) => mse_wish(&codecs)?,
        Incoming::Hls(codecs) => {
            if sent.get("type").and_then(serde_json::Value::as_str) != Some("hls") {
                return Err("hls read from another type".into());
            }
            mse_wish(&codecs)?;
        }
        Incoming::Mjpeg => {
            if sent.get("type").and_then(serde_json::Value::as_str) != Some("mjpeg") {
                return Err("mjpeg read from another type".into());
            }
        }
        Incoming::Candidate(None) | Incoming::Ignored => {}
    }
    Ok(())
}

/// A codec list's wish: each codec once, of its media's kind, the PCM family
/// whole (FLAC is all of it or none), and nothing asked that no name said.
fn mse_wish(codecs: &str) -> Result<(), String> {
    let wish = mse::wish(codecs);
    let named = codecs.split(',').any(|n| !n.trim().is_empty());
    // What the reader's H.265 decoder takes (D44): one receiver per `hvc1` or
    // `hev1` entry read, each the entry's own reading, a list that names
    // nothing unchecked.
    let entries: Vec<&str> = codecs
        .split(',')
        .map(str::trim)
        .filter(|n| !n.is_empty())
        .take(mse::MAX_CODECS)
        .filter(|n| {
            let family = n.split('.').next().unwrap_or_default().to_ascii_lowercase();
            family == "hvc1" || family == "hev1"
        })
        .collect();
    match &wish.receivers {
        None if named => return Err(format!("{codecs:?} names codecs and its H.265 is not checked")),
        Some(_) if !named => return Err(format!("{codecs:?} names nothing and its H.265 is checked")),
        Some(receivers) => {
            let read: Vec<HevcReceiver> =
                entries.iter().map(|e| HevcReceiver::from_codec_string(e).unwrap_or(HevcReceiver::ANY)).collect();
            if *receivers != read {
                return Err(format!("{codecs:?} made the receivers {receivers:?}"));
            }
        }
        None => {}
    }
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

/// Any query of `stream.mp4` or `frame.mp4`: the wish made of it asks only
/// for codecs the writer takes, each once and of its media's kind, never any
/// codec at all; `duration` is the whole seconds given; and every `filename`
/// makes a `Content-Disposition` whose quoted name holds no quote, backslash,
/// slash or control character, is at most [`crate::MAX_FILENAME_CHARS`]
/// long, and whose `filename*` decodes back to the name kept.
pub fn mp4_query(data: &[u8]) -> Result<(), String> {
    let Ok(query) = std::str::from_utf8(data) else { return Ok(()) };
    if let Ok(wish) = crate::mp4::stream_wish(query) {
        let writable = [Codec::H264, Codec::H265, Codec::Aac, Codec::Opus, Codec::Pcma, Codec::Pcmu, Codec::L16];
        for kind in [MediaKind::Video, MediaKind::Audio] {
            match wish.want.get(kind) {
                MediaWant::Any => return Err(format!("{query:?} asks for any {kind:?}")),
                MediaWant::Skip => {}
                MediaWant::OneOf(list) => {
                    for (at, codec) in list.iter().enumerate() {
                        let again = list.iter().skip(at + 1).any(|c| c == codec);
                        if codec.kind() != kind || again || !writable.contains(codec) {
                            return Err(format!("{query:?} made {list:?} for {kind:?}"));
                        }
                    }
                }
            }
        }
    }
    for (key, value) in form_urlencoded::parse(data) {
        match &*key {
            "duration" => {
                // Whole seconds held to u32::MAX, set aside past it (a `+`
                // before them taken, as Go's Atoi takes it); anything else no
                // limit, set aside; an empty value nothing at all.
                let number = value.strip_prefix('+').unwrap_or(&value);
                let digits = !number.is_empty() && number.bytes().all(|b| b.is_ascii_digit());
                let over = digits && !number.parse::<u64>().is_ok_and(|s| s <= u64::from(u32::MAX));
                let want = number.parse::<u64>().map_or(u64::from(u32::MAX), |s| s.min(u64::from(u32::MAX)));
                let (given, ignored) = crate::mp4::duration(Some(&value));
                let read = match given {
                    Some(given) => digits && given.as_secs() == want && want > 0,
                    None => !digits || want == 0,
                };
                let set_aside = ignored.is_some() == (over || (!digits && !value.is_empty()));
                if !read || !set_aside {
                    return Err(format!("duration {value:?} read as {given:?}, set aside {ignored:?}"));
                }
            }
            "filename" => disposition(&value)?,
            _ => {}
        }
    }
    Ok(())
}

fn disposition(name: &str) -> Result<(), String> {
    let header = crate::mp4::disposition(name);
    let text = header.to_str().map_err(|_| format!("{name:?}: a header that is not visible ASCII"))?;
    let quoted = text.strip_prefix("attachment; filename=\"").ok_or_else(|| format!("{name:?}: {text:?}"))?;
    let (plain, rest) = quoted.split_once('"').ok_or_else(|| format!("{name:?}: an unclosed name {text:?}"))?;
    if plain.chars().any(|c| c.is_control() || matches!(c, '\\' | '/')) {
        return Err(format!("{name:?}: the name {plain:?}"));
    }
    if plain.chars().count() > crate::MAX_FILENAME_CHARS {
        return Err(format!("{name:?}: a name of {} characters", plain.chars().count()));
    }
    let kept: String = name.chars().filter(|c| !c.is_control() && !matches!(c, '/' | '\\')).collect();
    match rest.strip_prefix("; filename*=UTF-8''") {
        None if rest.is_empty() => Ok(()),
        None => Err(format!("{name:?}: after the name {rest:?}")),
        Some(encoded) => {
            let back =
                String::from_utf8(percent_decoded(encoded)).map_err(|_| format!("{name:?}: filename* is not UTF-8"))?;
            if !kept.starts_with(&back) || back.chars().count() != plain.chars().count() {
                return Err(format!("{name:?}: filename* {back:?} for {plain:?}"));
            }
            Ok(())
        }
    }
}

/// `text` with its `%XX` escapes decoded and every other byte as it is
/// (RFC 8187's `value-chars`: `&` and `+` are themselves there, which a form
/// parser would cut at or turn into a space).
fn percent_decoded(text: &str) -> Vec<u8> {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut at = 0;
    while let Some(&byte) = bytes.get(at) {
        let hex = bytes.get(at + 1..at + 3).and_then(|h| std::str::from_utf8(h).ok());
        match hex.filter(|_| byte == b'%').and_then(|h| u8::from_str_radix(h, 16).ok()) {
            Some(decoded) => {
                out.push(decoded);
                at += 3;
            }
            None => {
                out.push(byte);
                at += 1;
            }
        }
    }
    out
}

#[cfg(test)]
#[path = "fuzzing_tests.rs"]
mod tests;

/// The names and sources `config_text` sets and removes.
const PATCHED: [(&str, &[&str]); 2] =
    [("cam", &["rtsp://u:p@a/x#timeout=5"]), ("camera.f7", &["rtsp://b/y", "ffmpeg:camera.f7#audio=opus"])];

/// The file's top-level map without `streams`, as parsed.
fn others(text: &str) -> Option<serde_norway::Mapping> {
    let mut root = match serde_norway::from_str(text).ok()? {
        serde_norway::Value::Mapping(root) => root,
        // An empty file (or one of comments) is an empty map.
        serde_norway::Value::Null => serde_norway::Mapping::new(),
        _ => return None,
    };
    root.remove("streams");
    Some(root)
}

/// Any text as a configuration file (`POST /api/config`'s body, the file
/// `PUT` and `DELETE /api/streams` edit, D48): the parser never panics, and
/// neither does go2rtc's merge of the text into itself; a stream set or
/// removed in the text (`config_patch`) changes nothing outside `streams:`
/// when both texts parse, and a set stream reads back with its sources.
pub fn config_text(data: &[u8]) -> Result<(), String> {
    let Ok(text) = std::str::from_utf8(data) else { return Ok(()) };
    let parsed = crate::config::parse(text);
    if let Ok(change) = serde_norway::from_str::<serde_norway::Value>(text) {
        let mut tree = change.clone();
        crate::config_file::merge(&mut tree, change);
    }
    for (name, sources) in PATCHED {
        let sources: Vec<String> = sources.iter().map(|s| (*s).to_owned()).collect();
        for set in [Some(sources.as_slice()), None] {
            let Some(out) = crate::config_patch::patch_stream(text, name, set) else { continue };
            let (Ok(_), Ok((after, _))) = (&parsed, crate::config::parse(&out)) else { continue };
            if others(text) != others(&out) {
                return Err(format!("setting {name} changed another section: {out:?}"));
            }
            let found = after.streams.iter().find(|(n, _)| n == name).map(|(_, s)| s);
            match set {
                Some(sources) if found.map(Vec::as_slice) != Some(sources) => {
                    return Err(format!("{name} does not read back as set: {out:?}"));
                }
                None if found.is_some() => return Err(format!("{name} still there once removed: {out:?}")),
                _ => {}
            }
        }
    }
    Ok(())
}
