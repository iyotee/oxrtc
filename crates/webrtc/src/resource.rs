//! The WHEP session resource: what an answered offer is named by, and what a
//! client sends there afterwards (RFC 9725). `PATCH` with an
//! `application/trickle-ice-sdpfrag` body adds the remote candidates the
//! offer did not carry; `DELETE` ends the session.
//!
//! A resource id is 128 random bits from the operating system's source,
//! written in hex: a client that holds it may end the session or feed it
//! candidates, so it is not guessable and is never logged. The id of a
//! session that ended (or never existed) is `NotFound`, whatever it looked like.

use std::net::IpAddr;
use std::panic::AssertUnwindSafe;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use str0m::Candidate;
use tracing::{debug, warn};

use crate::registry::Session;
use crate::server::Shared;

/// Remote candidates one session takes by `PATCH`, in all. A browser trickles
/// a dozen at most; the bound keeps a client from growing the ICE agent's
/// pair list without end.
pub const MAX_TRICKLED_CANDIDATES: usize = 64;

/// Candidates one `PATCH` may carry.
pub const MAX_CANDIDATES_PER_PATCH: usize = 16;

/// How long a `DELETE` waits for the session to let go of its stream: the
/// session reacts at once, this only bounds a task stuck on a socket write.
const CLOSE_WAIT: Duration = Duration::from_secs(2);

/// An answered offer: the SDP answer, and the id of the session resource the
/// client reaches the session by.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Answered {
    /// The complete SDP answer.
    pub sdp: String,
    /// The opaque id of the session resource (32 hex characters).
    pub resource: String,
}

/// Why a request on a session resource was refused.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ResourceError {
    /// No such session (never was, or already ended).
    #[error("session not found")]
    NotFound,
    /// The body is not a usable SDP fragment, or asks for more than the
    /// bounds allow.
    #[error("bad candidate fragment: {0}")]
    BadFragment(String),
    /// The fragment carries another ICE user name fragment than the offer's:
    /// an ICE restart, which this server does not do.
    #[error("ice restart is not supported")]
    IceRestart,
}

/// A fresh resource id, `None` if the operating system has no random bytes.
pub(crate) fn new_resource_id() -> Option<String> {
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes).ok()?;
    Some(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

/// The first `a=ice-ufrag` of an offer: the peer's user name fragment, which
/// a trickled fragment must repeat.
pub(crate) fn offer_ufrag(sdp: &str) -> Option<String> {
    sdp.lines()
        .filter_map(|line| line.trim().strip_prefix("a=ice-ufrag:"))
        .map(|ufrag| ufrag.trim().to_owned())
        .find(|ufrag| !ufrag.is_empty())
}

/// What a trickle fragment holds that this server uses.
#[derive(Debug, Default)]
pub(crate) struct Fragment {
    /// `a=ice-ufrag`, when present.
    pub(crate) ufrag: Option<String>,
    /// The candidates str0m reads (an mDNS name is left out, as in an offer).
    pub(crate) candidates: Vec<Candidate>,
    /// `a=candidate` lines that could not be read.
    pub(crate) unread: usize,
}

/// Reads an `application/trickle-ice-sdpfrag` body: its `a=ice-ufrag` and its
/// `a=candidate` lines. Everything else (`a=ice-pwd`, `m=`, `a=mid`,
/// `a=end-of-candidates`) is accepted and ignored: the candidates belong to the
/// session whatever media line they sit under, since the answer bundles them.
/// More than [`MAX_CANDIDATES_PER_PATCH`] candidate lines is refused.
#[doc(hidden)]
pub(crate) fn parse_fragment(body: &str) -> Result<Fragment, ResourceError> {
    let mut fragment = Fragment::default();
    let mut lines = 0usize;
    for raw in body.lines() {
        let Some(attribute) = raw.trim().strip_prefix("a=") else { continue };
        if let Some(ufrag) = attribute.strip_prefix("ice-ufrag:") {
            if fragment.ufrag.is_none() && !ufrag.trim().is_empty() {
                fragment.ufrag = Some(ufrag.trim().to_owned());
            }
        } else if attribute.starts_with("candidate:") {
            lines += 1;
            if lines > MAX_CANDIDATES_PER_PATCH {
                return Err(ResourceError::BadFragment(format!("more than {MAX_CANDIDATES_PER_PATCH} candidates")));
            }
            match Candidate::from_sdp_string(attribute) {
                Ok(candidate) => fragment.candidates.push(candidate),
                Err(_) => fragment.unread += 1,
            }
        }
    }
    Ok(fragment)
}

impl Shared {
    /// The session a resource id names.
    fn resource(&self, id: &str) -> Option<Arc<Session>> {
        self.registry().resource(id)
    }

    /// Adds the candidates of `body` to the session `id` names. The addresses
    /// they name are returned, for the permissions a TURN relay asks for.
    pub(crate) fn trickle(&self, id: &str, body: &str) -> Result<Vec<IpAddr>, ResourceError> {
        let session = self.resource(id).ok_or(ResourceError::NotFound)?;
        let fragment = parse_fragment(body)?;
        if let (Some(sent), Some(offered)) = (&fragment.ufrag, &session.remote_ufrag)
            && sent != offered
        {
            return Err(ResourceError::IceRestart);
        }
        let wanted = fragment.candidates.len();
        let taken = session
            .trickled
            .try_update(Ordering::AcqRel, Ordering::Acquire, |held| {
                Some(held.saturating_add(wanted)).filter(|total| *total <= MAX_TRICKLED_CANDIDATES)
            })
            .is_ok();
        if !taken {
            return Err(ResourceError::BadFragment(format!(
                "more than {MAX_TRICKLED_CANDIDATES} candidates for one session"
            )));
        }
        let mut ips: Vec<IpAddr> = Vec::new();
        for candidate in &fragment.candidates {
            let ip = candidate.addr().ip();
            if !ips.contains(&ip) {
                ips.push(ip);
            }
        }
        {
            let mut peer = session.peer();
            // str0m runs on input from the network here as everywhere: a
            // panic ends this session, not the request's task.
            let added = std::panic::catch_unwind(AssertUnwindSafe(|| {
                for candidate in fragment.candidates {
                    peer.rtc.add_remote_candidate(candidate);
                }
            }));
            if added.is_err() {
                warn!(session = session.id, "str0m panicked on a trickled candidate, ending the session");
                drop(peer);
                session.cancel.cancel();
                return Err(ResourceError::BadFragment("candidate refused".into()));
            }
        }
        if fragment.unread > 0 {
            debug!(session = session.id, unread = fragment.unread, "trickled candidates left out");
        }
        session.wake.notify_one();
        Ok(ips)
    }

    /// Ends the session `id` names, and returns once it has let go of its
    /// stream (its consumer is gone from the stream's listing).
    pub(crate) async fn close_resource(&self, id: &str) -> Result<(), ResourceError> {
        let session = self.resource(id).ok_or(ResourceError::NotFound)?;
        session.cancel.cancel();
        let _ = tokio::time::timeout(CLOSE_WAIT, session.ended.cancelled()).await;
        Ok(())
    }
}

#[cfg(test)]
#[path = "resource_tests.rs"]
mod tests;
