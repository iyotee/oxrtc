//! What the server's answers do to an allocation: the 401 challenge, a
//! stale nonce, a success (signed with the key, or not the server's), an
//! error about one peer or about the allocation itself.

use std::net::IpAddr;
use std::time::{Duration, Instant};

use super::{
    Allocation, Auth, Event, Failure, MAX_LIFETIME, MAX_STALE, MAX_TEXT, MIN_LIFETIME, REQUESTED_LIFETIME, Request,
    State, shown,
};
use crate::stun::{Message, attr, long_term_key};

impl Allocation {
    pub(super) fn succeeded(&mut self, now: Instant, kind: Request, message: &Message<'_>) {
        // The answer to a signed request is signed with the same key, or it
        // is not the server's (RFC 8489 section 9.2.5).
        if let Some(auth) = &self.auth
            && !message.verify_integrity(&auth.key, self.hmac)
        {
            return self.unanswered(now, kind);
        }
        self.stale = 0;
        match kind {
            Request::Allocate => {
                let Some(relayed) = message.xor_address(attr::XOR_RELAYED_ADDRESS).filter(|a| usable(a.ip())) else {
                    return self.fail(Failure::Error(0, "no usable relayed address".into()));
                };
                let mapped = message.xor_address(attr::XOR_MAPPED_ADDRESS);
                let lifetime = lifetime(message);
                self.state =
                    State::Allocated { relayed, expires: now + lifetime, refresh_at: now + refresh_after(lifetime) };
                self.events.push_back(Event::Allocated { relayed, mapped, lifetime });
            }
            Request::Refresh(0) => self.state = State::Closed,
            Request::Refresh(_) => {
                if let State::Allocated { relayed, .. } = self.state {
                    let lifetime = lifetime(message);
                    self.state = State::Allocated {
                        relayed,
                        expires: now + lifetime,
                        refresh_at: now + refresh_after(lifetime),
                    };
                }
            }
            Request::Permission(ip) => self.peers.permission_answered(ip, true, now),
            Request::Bind(number, _) => self.peers.channel_answered(number, true, now),
        }
    }

    /// An error answer to `kind`, which was sent as `sent`.
    pub(super) fn errored(&mut self, now: Instant, kind: Request, message: &Message<'_>, sent: &[u8]) {
        let (code, reason) = message.error_code().unwrap_or((0, ""));
        let realm = message.text(attr::REALM, MAX_TEXT);
        let nonce = message.text(attr::NONCE, MAX_TEXT);
        match code {
            // Signed with a nonce an earlier 438 already replaced: every
            // request in flight when the nonce lapsed is answered so, and
            // is sent again with the new one without counting against
            // MAX_STALE, which stops a server that never takes a nonce.
            438 if self.auth.as_ref().is_some_and(|auth| signed_with_another_nonce(sent, &auth.nonce)) => {
                self.request(now, kind);
            }
            // The challenge: the first answer to an unsigned request.
            401 if self.auth.is_none() => match (realm, nonce) {
                (Some(realm), Some(nonce)) => {
                    let key = long_term_key(&self.credentials.username, realm, &self.credentials.password);
                    self.auth = Some(Auth { realm: realm.to_owned(), nonce: nonce.to_owned(), key });
                    self.request(now, kind);
                }
                _ => self.fail(Failure::Error(401, "a challenge without realm or nonce".into())),
            },
            401 => match kind {
                Request::Allocate | Request::Refresh(_) => self.fail(Failure::Refused),
                other => self.refused(now, other),
            },
            438 if self.stale < MAX_STALE => {
                self.stale += 1;
                if let (Some(auth), Some(nonce)) = (&mut self.auth, nonce) {
                    auth.nonce = nonce.to_owned();
                    if let Some(realm) = realm
                        && realm != auth.realm
                    {
                        auth.realm = realm.to_owned();
                        auth.key = long_term_key(&self.credentials.username, realm, &self.credentials.password);
                    }
                }
                self.request(now, kind);
            }
            _ => match kind {
                Request::Allocate | Request::Refresh(1..) => self.fail(Failure::Error(code, shown(reason))),
                other => self.refused(now, other),
            },
        }
    }

    /// A request about one peer was refused: that peer only.
    fn refused(&mut self, now: Instant, kind: Request) {
        match kind {
            Request::Permission(ip) => self.peers.permission_answered(ip, false, now),
            Request::Bind(number, _) => self.peers.channel_answered(number, false, now),
            Request::Refresh(0) => self.state = State::Closed,
            Request::Allocate | Request::Refresh(_) => self.fail(Failure::Refused),
        }
    }

    /// A request never answered (or answered by someone without the key).
    pub(super) fn unanswered(&mut self, now: Instant, kind: Request) {
        match kind {
            Request::Allocate => self.fail(Failure::NoAnswer),
            Request::Refresh(0) => self.state = State::Closed,
            Request::Refresh(_) => self.fail(Failure::Lost("its refresh was not answered")),
            other => self.refused(now, other),
        }
    }
}

/// Whether the request `sent` carries a nonce other than `nonce`.
fn signed_with_another_nonce(sent: &[u8], nonce: &str) -> bool {
    Message::parse(sent).ok().and_then(|m| m.text(attr::NONCE, MAX_TEXT)).is_some_and(|used| used != nonce)
}

/// The lifetime an answer grants, bounded.
pub(super) fn lifetime(message: &Message<'_>) -> Duration {
    let seconds = message.u32(attr::LIFETIME).unwrap_or(REQUESTED_LIFETIME).clamp(MIN_LIFETIME, MAX_LIFETIME);
    Duration::from_secs(u64::from(seconds))
}

/// A refresh a minute before the lifetime lapses (at half of a short one).
pub(super) fn refresh_after(lifetime: Duration) -> Duration {
    lifetime.saturating_sub(Duration::from_secs(60)).max(lifetime / 2)
}

fn usable(ip: IpAddr) -> bool {
    !(ip.is_unspecified() || ip.is_multicast())
}
