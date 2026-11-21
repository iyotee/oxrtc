//! The messages of go2rtc's WebSocket API: JSON text frames
//! `{"type": ..., "value": ...}` (go2rtc's `internal/api/ws/README.md`).
//!
//! Read: `webrtc/offer` (the value is the SDP offer), `webrtc` (the value is
//! `{"type": "offer", "sdp": ...}`, the dialect Home Assistant's client
//! library writes), `webrtc/candidate` (a candidate line, or an empty value
//! for the end of them), `mse` (the value is the codec list the browser's
//! MSE takes, F3), and the types of the egresses this server does not have
//! yet (`hls`, `mjpeg`, `mp4`). Any other type is read and ignored, as go2rtc
//! ignores a type it has no handler for.
//!
//! Written: `webrtc/answer` or `webrtc` (the answer, in the offer's own
//! dialect), `webrtc/candidate`, `mse` (the MIME type of the fragments that
//! follow as binary frames), and `{"type": "error", "value": "<type>:
//! <reason>"}`, go2rtc's form of a failure, the reason redacted.

use serde::Deserialize;
use serde_json::Value;

/// The two ways a client asks for a WebRTC session; the answer is written
/// the same way.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dialect {
    /// `webrtc/offer` with the SDP as its value (go2rtc's player).
    Sdp,
    /// `webrtc` with `{"type": "offer", "sdp": ...}` (Home Assistant).
    Object,
}

impl Dialect {
    /// The type the client's offer had, which an error about it names.
    pub fn offer_type(self) -> &'static str {
        match self {
            Self::Sdp => "webrtc/offer",
            Self::Object => "webrtc",
        }
    }
}

/// A type of a later item (F4 to F6): the one place their handlers plug in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Later {
    /// `hls`: a playlist (F6).
    Hls,
    /// `mjpeg`: JPEG pictures over this socket (F5).
    Mjpeg,
    /// `mp4`: progressive MP4 over this socket (F4).
    Mp4,
}

impl Later {
    /// The message type.
    pub fn name(self) -> &'static str {
        match self {
            Self::Hls => "hls",
            Self::Mjpeg => "mjpeg",
            Self::Mp4 => "mp4",
        }
    }
}

/// What a text frame asks for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Incoming {
    /// An SDP offer.
    Offer {
        /// The offer, as the client wrote it.
        sdp: String,
        /// How it was asked, and so how it is answered.
        dialect: Dialect,
    },
    /// An offer message without an offer in it.
    BadOffer {
        /// How it was asked.
        dialect: Dialect,
        /// Why it is not one.
        reason: &'static str,
    },
    /// A remote candidate line (`candidate:...`, without `a=`); `None` is an
    /// empty value, which ends the client's candidates and asks nothing.
    Candidate(Option<String>),
    /// `mse`: fragmented MP4 over this socket, in codecs of this list
    /// (`avc1.640029,hvc1.1.6.L153.B0,mp4a.40.2,flac,opus`: what the
    /// browser's `MediaSource.isTypeSupported` said yes to); empty when the
    /// value is none or not a string, as go2rtc reads it.
    Mse(String),
    /// A type a later item serves.
    Later(Later),
    /// Any other type: ignored.
    Ignored,
}

/// A text frame that is not a message.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum MessageError {
    /// Not JSON, or JSON without a string `type`.
    #[error("not a message")]
    NotMessage,
}

#[derive(Deserialize)]
struct Raw {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    value: Value,
}

/// Reads one text frame.
pub fn parse(text: &str) -> Result<Incoming, MessageError> {
    let raw: Raw = serde_json::from_str(text).map_err(|_| MessageError::NotMessage)?;
    Ok(match raw.kind.as_str() {
        "webrtc/offer" => match raw.value {
            Value::String(sdp) if !sdp.trim().is_empty() => Incoming::Offer { sdp, dialect: Dialect::Sdp },
            _ => Incoming::BadOffer { dialect: Dialect::Sdp, reason: "the value is not an SDP offer" },
        },
        "webrtc" => object_offer(&raw.value),
        // go2rtc reads a candidate that is not a string as an empty one.
        "webrtc/candidate" => Incoming::Candidate(raw.value.as_str().and_then(candidate_line)),
        "mse" => Incoming::Mse(raw.value.as_str().map(str::to_owned).unwrap_or_default()),
        "hls" => Incoming::Later(Later::Hls),
        "mjpeg" => Incoming::Later(Later::Mjpeg),
        "mp4" => Incoming::Later(Later::Mp4),
        _ => Incoming::Ignored,
    })
}

/// `webrtc`'s value: `{"type": "offer", "sdp": "..."}` (its `ice_servers`
/// are not used: the server's own are, see D39).
fn object_offer(value: &Value) -> Incoming {
    let bad = |reason| Incoming::BadOffer { dialect: Dialect::Object, reason };
    let Some(object) = value.as_object() else { return bad("the value is not an offer") };
    if object.get("type").and_then(Value::as_str) != Some("offer") {
        return bad("the value is not an offer");
    }
    match object.get("sdp").and_then(Value::as_str) {
        Some(sdp) if !sdp.trim().is_empty() => Incoming::Offer { sdp: sdp.to_owned(), dialect: Dialect::Object },
        _ => bad("the offer has no SDP"),
    }
}

/// A candidate value as a line `candidate:...`: an `a=` before it and the
/// line's own end are taken off, and nothing after its first line is read (it
/// is handed on as one line of a trickle fragment, which must not gain lines
/// of the client's choosing); an empty one is the end of candidates.
fn candidate_line(value: &str) -> Option<String> {
    let line = value.trim_start().split(['\r', '\n']).next().unwrap_or_default().trim();
    let line = line.strip_prefix("a=").unwrap_or(line).trim();
    (!line.is_empty()).then(|| line.to_owned())
}

/// What the server writes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outgoing {
    /// The SDP answer, in the offer's dialect.
    Answer {
        /// The complete answer.
        sdp: String,
        /// How the offer was asked.
        dialect: Dialect,
    },
    /// One of the server's candidates, as the answer also carries it.
    Candidate(String),
    /// The MIME type, codecs included, of the init segment and fragments
    /// that follow (`video/mp4; codecs="avc1.64001f,mp4a.40.2"`), which
    /// go2rtc's player hands to `MediaSource.addSourceBuffer`.
    Mse(String),
    /// A failure of the message of type `about`.
    Error {
        /// The type of the message that failed.
        about: &'static str,
        /// Why.
        reason: String,
    },
}

impl Outgoing {
    /// The text frame. The reason of an error is redacted: a backend's
    /// message may quote a source's URL.
    pub fn to_json(&self) -> String {
        let value = match self {
            Self::Answer { sdp, dialect: Dialect::Sdp } => {
                serde_json::json!({ "type": "webrtc/answer", "value": sdp })
            }
            Self::Answer { sdp, dialect: Dialect::Object } => {
                serde_json::json!({ "type": "webrtc", "value": { "type": "answer", "sdp": sdp } })
            }
            Self::Candidate(line) => serde_json::json!({ "type": "webrtc/candidate", "value": line }),
            Self::Mse(mime) => serde_json::json!({ "type": "mse", "value": mime }),
            Self::Error { about, reason } => serde_json::json!({
                "type": "error",
                "value": oxrtc_core::spec::redact(&format!("{about}: {reason}")),
            }),
        };
        value.to_string()
    }
}

/// The candidate lines of an SDP (`a=candidate:...`), as `candidate:...`,
/// each once.
pub fn answer_candidates(sdp: &str) -> Vec<String> {
    let mut lines: Vec<String> = Vec::new();
    for line in sdp.lines() {
        let Some(candidate) = line.trim().strip_prefix("a=") else { continue };
        if candidate.starts_with("candidate:") && !lines.iter().any(|l| l == candidate) {
            lines.push(candidate.to_owned());
        }
    }
    lines
}

#[cfg(test)]
#[path = "message_tests.rs"]
mod tests;
