//! `PUT /api/webrtc/ice` (oxrtc only): new ICE servers for the running
//! WebRTC egress, so that a TURN credential that rotates restarts nothing.

use axum::body::Bytes;
use axum::extract::State;
use axum::http::StatusCode;
use serde::Deserialize;

use crate::ApiState;
use crate::config::IceServer;
use crate::routes::{Reply, json, text};

/// Entries accepted in one call (openIVR writes one).
const MAX_ENTRIES: usize = 16;
/// URLs accepted in one entry (openIVR writes three).
const MAX_URLS: usize = 16;
/// Longest URL, user name or credential accepted, in bytes.
const MAX_FIELD_BYTES: usize = 1024;

/// One entry, as `webrtc.ice_servers` writes it.
#[derive(Deserialize)]
struct Entry {
    #[serde(default)]
    urls: Vec<String>,
    username: Option<String>,
    credential: Option<String>,
}

/// The entries of a body, or why it is refused. The reason never quotes the
/// body: it may hold a credential, and serde's own messages quote values.
fn parse(body: &[u8]) -> Result<Vec<IceServer>, &'static str> {
    let entries: Vec<Entry> =
        serde_json::from_slice(body).map_err(|_| "the body must be a JSON list of {urls, username, credential}")?;
    if entries.len() > MAX_ENTRIES {
        return Err("too many entries");
    }
    let mut servers = Vec::with_capacity(entries.len());
    for entry in entries {
        let mut fields =
            entry.urls.iter().map(String::as_str).chain(entry.username.as_deref()).chain(entry.credential.as_deref());
        if entry.urls.len() > MAX_URLS || fields.any(|f| f.len() > MAX_FIELD_BYTES) {
            return Err("an entry is too large");
        }
        servers.push(IceServer { urls: entry.urls, username: entry.username, credential: entry.credential });
    }
    Ok(servers)
}

pub(crate) async fn put_ice_servers(State(state): State<ApiState>, body: Bytes) -> Reply {
    let answerer = state.answerer.clone().ok_or_else(|| text(StatusCode::NOT_IMPLEMENTED, "webrtc is not enabled"))?;
    let servers = parse(&body).map_err(|reason| text(StatusCode::BAD_REQUEST, reason))?;
    let summary = answerer.set_ice_servers(servers);
    // Counts only: the entries hold a credential.
    tracing::info!(turn = summary.turn, stun = summary.stun, "api: ice servers replaced");
    Ok(json(&serde_json::json!({ "turn": summary.turn, "stun": summary.stun })))
}
