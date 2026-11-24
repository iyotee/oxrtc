//! HLS (M5 F6, D47): `GET /api/stream.m3u8` as go2rtc serves it - a master
//! playlist naming one media playlist of a session of the client's own -
//! then the session's routes under `/api/hls/`: the media playlist (with
//! Low-Latency HLS's parts, preload hint and blocking reload), the fMP4 init
//! segment, segments and parts (`.m4s`, or `.ts` for MPEG-TS), and
//! `/api/stream.mpd`, an MPEG-DASH manifest over the same fMP4 segments.
//!
//! fMP4 when the query names codecs (`mp4`, `video`, `audio`, read as
//! `stream.mp4` reads them), MPEG-TS otherwise (H.264 or H.265, AAC), as
//! go2rtc chooses. A session is go2rtc's: one subscription per client,
//! ended after [`session::IDLE`] (or three of its longest segments) without
//! a request. Its id (128 random bits) is the capability its routes are
//! read with: `stream.m3u8` is authenticated as every route, the session's
//! routes need its id and nothing else, so that a player which cannot send
//! a header (a `<video>` element, a Chromecast) plays what an authenticated
//! page opened for it; the id is never logged.

pub(crate) mod playlist;
mod routes;
pub(crate) mod session;
pub(crate) mod window;

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use oxrtc_core::{PerIpLimit, Refusals};

pub(crate) use routes::{router, stream_m3u8, stream_mpd};
use session::Session;

use crate::slots::Slots;

/// The bounds of HLS.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HlsLimits {
    /// Sessions at once.
    pub max_sessions: usize,
    /// Sessions at once from one address.
    pub max_sessions_per_ip: usize,
    /// Requests waiting at once (a blocking reload, a part not complete yet).
    pub max_waiting: usize,
    /// Requests waiting at once from one address.
    pub max_waiting_per_ip: usize,
    /// The longest a request waits, whatever the target duration.
    pub max_wait: Duration,
    /// A session no request has touched for this long ends (go2rtc's
    /// `keepalive`, 5 s), or for three of its longest segments if longer.
    pub idle: Duration,
}

/// Sessions at once by default.
pub const MAX_SESSIONS: usize = crate::MAX_CONNECTIONS / 4;
/// Sessions at once from one address by default.
pub const MAX_SESSIONS_PER_IP: usize = 16;

impl Default for HlsLimits {
    fn default() -> Self {
        Self {
            max_sessions: MAX_SESSIONS,
            max_sessions_per_ip: MAX_SESSIONS_PER_IP,
            max_waiting: crate::MAX_CONNECTIONS / 2,
            max_waiting_per_ip: 32,
            max_wait: Duration::from_secs(30),
            idle: session::IDLE,
        }
    }
}

/// The sessions, and their bounds.
pub(crate) struct Hls {
    pub limits: HlsLimits,
    sessions: Mutex<HashMap<String, Arc<Session>>>,
    per_ip: PerIpLimit,
    full: Refusals,
    pub waiting: Slots,
}

impl std::fmt::Debug for Hls {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Hls").field("limits", &self.limits).field("sessions", &self.len()).finish()
    }
}

/// A new session id: 16 random bytes, base 62 (22 characters).
fn new_id() -> Option<String> {
    const ALPHABET: &[u8] = b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz";
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes).ok()?;
    let mut value = u128::from_be_bytes(bytes);
    let mut out = String::with_capacity(22);
    for _ in 0..22 {
        let digit = usize::try_from(value % 62).unwrap_or(0);
        out.push(char::from(*ALPHABET.get(digit).unwrap_or(&b'0')));
        value /= 62;
    }
    Some(out)
}

impl Hls {
    pub(crate) fn new(limits: HlsLimits) -> Self {
        Self {
            limits,
            sessions: Mutex::new(HashMap::new()),
            per_ip: PerIpLimit::new(limits.max_sessions_per_ip),
            full: Refusals::new(),
            waiting: Slots::new("hls request", limits.max_waiting, limits.max_waiting_per_ip),
        }
    }

    fn map(&self) -> std::sync::MutexGuard<'_, HashMap<String, Arc<Session>>> {
        self.sessions.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Sessions open.
    pub(crate) fn len(&self) -> usize {
        self.map().len()
    }

    /// The session `id` names, touched.
    pub(crate) fn get(&self, id: &str) -> Option<Arc<Session>> {
        let session = self.map().get(id).cloned()?;
        session.touch();
        Some(session)
    }

    /// A new session for `remote`, or why not.
    pub(crate) fn open(
        &self,
        remote: Option<SocketAddr>,
        make: impl FnOnce(String, oxrtc_core::PerIpPermit) -> Session,
    ) -> Result<Arc<Session>, &'static str> {
        let mut map = self.map();
        map.retain(|_, s| !s.ended());
        if map.len() >= self.limits.max_sessions {
            if let Some(refused) = self.full.refused() {
                tracing::warn!(limit = self.limits.max_sessions, refused, "api: hls session limit reached, refusing");
            }
            return Err("too many hls sessions");
        }
        let ip = remote.map_or(IpAddr::from([0, 0, 0, 0]), |r| r.ip());
        let Some(permit) = self.per_ip.try_acquire(ip) else {
            if let Some(refused) = self.per_ip.refused() {
                tracing::warn!(
                    limit = self.per_ip.max_per_ip(),
                    refused,
                    "api: per-address hls session limit reached, refusing"
                );
            }
            return Err("too many hls sessions");
        };
        let id = new_id().ok_or("no random bytes for a session id")?;
        let session = Arc::new(make(id.clone(), permit));
        map.insert(id, session.clone());
        Ok(session)
    }

    /// Forgets a session that ended.
    pub(crate) fn remove(&self, id: &str) {
        if let Some(session) = self.map().remove(id) {
            session.end();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_are_22_characters_of_base_62_and_differ() {
        let a = new_id().unwrap();
        let b = new_id().unwrap();
        assert_eq!(a.len(), 22);
        assert!(a.bytes().all(|c| c.is_ascii_alphanumeric()));
        assert_ne!(a, b);
    }
}
