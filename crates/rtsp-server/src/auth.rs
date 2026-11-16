//! Who may read from the RTSP server (ROADMAP R28, decision D24).
//!
//! go2rtc 1.9.15 challenges with `Basic realm="go2rtc"` on every request of a
//! peer that is not on loopback, and takes a loopback peer unchecked. oxrtc
//! challenges with **Digest** (MD5, `qop=auth`) and with **Basic** (what
//! go2rtc accepts, and the only thing some players send), as two
//! `WWW-Authenticate` headers: FFmpeg picks the stronger. Loopback peers
//! authenticate too (D7) unless `rtsp.local_auth: false` gives go2rtc's rule
//! back.
//!
//! Two kinds of credential are accepted, and they are never the same string:
//!
//! - **the operator's** (`rtsp.username`/`rtsp.password`): for people, players
//!   and the NVR's own readers;
//! - **a run's own**, issued with each publication token
//!   ([`Publishers::register`]): the FFmpeg child of an `ffmpeg:` source reads
//!   its input with it, so a process list or an FFmpeg error line never shows
//!   the operator's, and it dies with the child.
//!
//! A publication (`_publish/<token>`) is not a read and needs neither: the
//! token is its credential, and it is taken from loopback only.

use std::fmt;
use std::net::{IpAddr, SocketAddr};

use base64::Engine;
use md5::{Digest, Md5};
use rtsp_types::StatusCode;
use rtsp_types::headers::{AUTHORIZATION, WWW_AUTHENTICATE};
use subtle::ConstantTimeEq;

use crate::Shared;
use crate::conn::{Reply, Req, target};
use crate::publish::PUBLISH_PREFIX;
use crate::wire::reply;

/// The realm of the challenges.
const REALM: &str = "oxrtc";
/// Longest `Authorization` header looked at.
const MAX_AUTHORIZATION_LEN: usize = 1024;
/// Most `key=value` pairs read from a Digest header.
pub(crate) const MAX_DIGEST_PARAMS: usize = 16;

/// The operator's credential for readers.
#[derive(Clone, PartialEq, Eq)]
pub struct ReadAuth {
    username: String,
    password: String,
    /// Whether a peer on loopback must authenticate too.
    local: bool,
}

impl ReadAuth {
    /// Readers must give `username` and `password`; loopback ones too.
    pub fn new(username: impl Into<String>, password: impl Into<String>) -> Self {
        Self { username: username.into(), password: password.into(), local: true }
    }

    /// Whether a peer on loopback must authenticate (`true`, the default) or is
    /// let in as go2rtc does (`false`).
    #[must_use]
    pub fn local(mut self, local: bool) -> Self {
        self.local = local;
        self
    }
}

impl fmt::Debug for ReadAuth {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ReadAuth").field("username", &self.username).field("local", &self.local).finish_non_exhaustive()
    }
}

/// What a request has to do with authentication.
pub(crate) enum Verdict {
    /// Go ahead.
    Allowed,
    /// Answer with this and keep the connection: the client may try again.
    Challenge(Reply),
    /// Answer with this and close: the credential was wrong.
    Refused(Reply),
}

/// The challenge state of one connection: its Digest nonce.
#[derive(Default)]
pub(crate) struct Gate {
    nonce: Option<String>,
}

impl Gate {
    /// Decides on `request` from `peer`.
    pub(crate) fn check(&mut self, shared: &Shared, peer: SocketAddr, request: &Req) -> Verdict {
        let Some(auth) = &shared.auth else { return Verdict::Allowed };
        if !auth.local && is_loopback(peer.ip()) {
            return Verdict::Allowed;
        }
        if publishes(request) {
            return Verdict::Allowed;
        }
        let Some(value) = request.header(&AUTHORIZATION).map(|v| v.as_str()) else {
            return Verdict::Challenge(self.challenge(request, false));
        };
        let value = value.trim();
        if value.len() > MAX_AUTHORIZATION_LEN {
            return Verdict::Refused(self.challenge(request, false));
        }
        let (scheme, rest) = value.split_once(' ').unwrap_or((value, ""));
        let rest = rest.trim();
        let outcome = if scheme.eq_ignore_ascii_case("basic") {
            basic(shared, rest)
        } else if scheme.eq_ignore_ascii_case("digest") {
            self.digest(shared, request, rest)
        } else {
            Outcome::Wrong
        };
        match outcome {
            Outcome::Right => Verdict::Allowed,
            Outcome::OldNonce => Verdict::Challenge(self.challenge(request, true)),
            Outcome::Wrong => Verdict::Refused(self.challenge(request, false)),
        }
    }

    /// The `401`: Digest first, Basic second.
    fn challenge(&mut self, request: &Req, stale: bool) -> Reply {
        let nonce = self.nonce.get_or_insert_with(new_nonce);
        let stale = if stale { ", stale=true" } else { "" };
        reply(request, StatusCode::Unauthorized)
            .header(
                WWW_AUTHENTICATE,
                format!("Digest realm=\"{REALM}\", nonce=\"{nonce}\", algorithm=MD5, qop=\"auth\"{stale}"),
            )
            .header(WWW_AUTHENTICATE, format!("Basic realm=\"{REALM}\""))
            .build(Vec::new())
    }

    fn digest(&self, shared: &Shared, request: &Req, header: &str) -> Outcome {
        let Some(params) = parse_params(header) else { return Outcome::Wrong };
        let get = |key: &str| params.iter().find(|(k, _)| k.eq_ignore_ascii_case(key)).map(|(_, v)| v.as_str());
        let (Some(user), Some(nonce), Some(uri), Some(given)) =
            (get("username"), get("nonce"), get("uri"), get("response"))
        else {
            return Outcome::Wrong;
        };
        if get("realm").is_some_and(|realm| realm != REALM)
            || get("algorithm").is_some_and(|algorithm| !algorithm.eq_ignore_ascii_case("md5"))
        {
            return Outcome::Wrong;
        }
        let Some(password) = secret_of(shared, user) else { return Outcome::Wrong };
        let method: &str = request.method().into();
        let a1 = hex_md5(&format!("{user}:{REALM}:{password}"));
        let a2 = hex_md5(&format!("{method}:{uri}"));
        let expected = match (get("qop"), get("nc"), get("cnonce")) {
            (Some(qop), Some(nc), Some(cnonce)) if qop.eq_ignore_ascii_case("auth") => {
                hex_md5(&format!("{a1}:{nonce}:{nc}:{cnonce}:auth:{a2}"))
            }
            // RFC 2069: no `qop`.
            (None, None, None) => hex_md5(&format!("{a1}:{nonce}:{a2}")),
            _ => return Outcome::Wrong,
        };
        if !bool::from(expected.as_bytes().ct_eq(given.to_ascii_lowercase().as_bytes())) {
            return Outcome::Wrong;
        }
        // Right password, for a nonce this connection did not issue (a client
        // reusing the one of its last connection): ask again with a fresh answer.
        match &self.nonce {
            Some(ours) if bool::from(ours.as_bytes().ct_eq(nonce.as_bytes())) => Outcome::Right,
            _ => Outcome::OldNonce,
        }
    }
}

enum Outcome {
    Right,
    Wrong,
    OldNonce,
}

fn is_loopback(ip: IpAddr) -> bool {
    ip.to_canonical().is_loopback()
}

/// Whether the request is part of a publication, which its token authorizes.
fn publishes(request: &Req) -> bool {
    target(request).is_some_and(|(path, _)| path.starts_with(PUBLISH_PREFIX))
}

/// The password of `user`: the operator's, or a run's own.
fn secret_of(shared: &Shared, user: &str) -> Option<String> {
    if let Some(auth) = &shared.auth
        && bool::from(auth.username.as_bytes().ct_eq(user.as_bytes()))
    {
        return Some(auth.password.clone());
    }
    shared.publishers.read_password(user)
}

fn basic(shared: &Shared, credential: &str) -> Outcome {
    let Ok(decoded) = base64::engine::general_purpose::STANDARD.decode(credential) else { return Outcome::Wrong };
    let Some(colon) = decoded.iter().position(|&b| b == b':') else { return Outcome::Wrong };
    let (user, password) = decoded.split_at(colon);
    let password = password.get(1..).unwrap_or_default();
    let Ok(user) = std::str::from_utf8(user) else { return Outcome::Wrong };
    match secret_of(shared, user) {
        Some(secret) if bool::from(secret.as_bytes().ct_eq(password)) => Outcome::Right,
        _ => Outcome::Wrong,
    }
}

/// `key=value` and `key="value"` pairs, comma separated. `None` for a header
/// that does not read that way or holds too many.
pub(crate) fn parse_params(header: &str) -> Option<Vec<(String, String)>> {
    let mut params = Vec::new();
    let mut rest = header.trim();
    while !rest.is_empty() {
        let (key, after) = rest.split_once('=')?;
        let key = key.trim().trim_start_matches(',').trim();
        if key.is_empty() || key.contains(char::is_whitespace) {
            return None;
        }
        let after = after.trim_start();
        let (value, tail) = if let Some(quoted) = after.strip_prefix('"') {
            let mut value = String::new();
            let mut chars = quoted.char_indices();
            let end = loop {
                match chars.next()? {
                    (_, '\\') => value.push(chars.next()?.1),
                    (at, '"') => break at,
                    (_, c) => value.push(c),
                }
            };
            (value, quoted.get(end + 1..)?)
        } else {
            let end = after.find(',').unwrap_or(after.len());
            (after.get(..end)?.trim().to_owned(), after.get(end..)?)
        };
        params.push((key.to_owned(), value));
        if params.len() > MAX_DIGEST_PARAMS {
            return None;
        }
        rest = tail.trim_start().trim_start_matches(',').trim_start();
    }
    Some(params)
}

fn hex_md5(text: &str) -> String {
    use std::fmt::Write as _;
    let digest = Md5::digest(text.as_bytes());
    digest.iter().fold(String::with_capacity(32), |mut out, byte| {
        let _ = write!(out, "{byte:02x}");
        out
    })
}

fn new_nonce() -> String {
    let mut bytes = [0u8; 16];
    if getrandom::fill(&mut bytes).is_err() {
        // No system randomness: a nonce the process can still not repeat.
        bytes = fastrand::u128(..).to_be_bytes();
    }
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
#[path = "auth_tests.rs"]
mod tests;
