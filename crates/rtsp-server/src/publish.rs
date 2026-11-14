//! One-time publication tokens: how a transcoder gets a path to `ANNOUNCE` on.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use oxrtc_core::{CancellationToken, ProducerSink};

/// Most tokens registered at once. A registration beyond it yields a handle
/// whose URL is refused (`404`), and a warning.
pub(crate) const MAX_PUBLISHERS: usize = 1024;

/// The path prefix of publication URLs.
pub(crate) const PUBLISH_PREFIX: &str = "_publish/";

struct Entry {
    sink: ProducerSink,
    /// Cancelled when the handle is dropped; parent of every session token.
    closed: CancellationToken,
    /// The session currently publishing, if any.
    active: Option<CancellationToken>,
}

struct Inner {
    base: String,
    entries: Mutex<HashMap<String, Entry>>,
}

/// The registry of publication tokens of one server. Cloning is cheap; every
/// clone sees the same registry.
#[derive(Clone)]
pub struct Publishers {
    inner: Arc<Inner>,
}

impl std::fmt::Debug for Publishers {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Publishers").field("registered", &self.len()).finish()
    }
}

/// What a publishing session gets from its token.
pub(crate) struct Claim {
    pub(crate) sink: ProducerSink,
    /// Cancelled when the handle is dropped or a newer session claims the token.
    pub(crate) session: CancellationToken,
}

impl Publishers {
    pub(crate) fn new(local: SocketAddr) -> Self {
        // An unspecified address is reached on the loopback of its own family:
        // a socket bound to `[::]` may be IPv6-only (the Windows default).
        let ip = match local.ip() {
            IpAddr::V4(ip) if ip.is_unspecified() => IpAddr::V4(Ipv4Addr::LOCALHOST),
            IpAddr::V6(ip) if ip.is_unspecified() => IpAddr::V6(Ipv6Addr::LOCALHOST),
            ip => ip,
        };
        let base = format!("rtsp://{}", SocketAddr::new(ip, local.port()));
        Self { inner: Arc::new(Inner { base, entries: Mutex::new(HashMap::new()) }) }
    }

    fn lock(&self) -> MutexGuard<'_, HashMap<String, Entry>> {
        self.inner.entries.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Registers `sink` under a fresh random token. Media `ANNOUNCE`d and
    /// `RECORD`ed on [`PublishHandle::url`] is published into it. Dropping the
    /// handle deregisters the token and ends any session publishing on it.
    pub fn register(&self, sink: ProducerSink) -> PublishHandle {
        let token = random_token();
        let url = format!("{}/{PUBLISH_PREFIX}{}", self.inner.base, token.as_deref().unwrap_or("unavailable"));
        let mut registered = false;
        if let Some(token) = &token {
            let mut entries = self.lock();
            if entries.len() >= MAX_PUBLISHERS {
                tracing::warn!(limit = MAX_PUBLISHERS, "rtsp: too many publication tokens, refusing one");
            } else {
                entries.insert(token.clone(), Entry { sink, closed: CancellationToken::new(), active: None });
                registered = true;
            }
        } else {
            tracing::warn!("rtsp: no system randomness for a publication token");
        }
        PublishHandle { token: token.filter(|_| registered), url, publishers: self.clone() }
    }

    /// Takes the token for a new publishing session, ending the previous one.
    pub(crate) fn claim(&self, token: &str) -> Option<Claim> {
        let mut entries = self.lock();
        let entry = entries.get_mut(token)?;
        let session = entry.closed.child_token();
        if let Some(previous) = entry.active.replace(session.clone()) {
            previous.cancel();
        }
        Some(Claim { sink: entry.sink.clone(), session })
    }

    /// Number of registered tokens.
    pub(crate) fn len(&self) -> usize {
        self.lock().len()
    }

    fn deregister(&self, token: &str) {
        let entry = self.lock().remove(token);
        if let Some(entry) = entry {
            entry.closed.cancel();
        }
    }
}

/// A registered publication token. Dropping it deregisters the token and ends
/// any session publishing on it.
pub struct PublishHandle {
    token: Option<String>,
    url: String,
    publishers: Publishers,
}

impl std::fmt::Debug for PublishHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The token is the only credential of the path: never printed.
        f.debug_struct("PublishHandle").field("registered", &self.token.is_some()).finish_non_exhaustive()
    }
}

impl PublishHandle {
    /// `rtsp://127.0.0.1:<port>/_publish/<128-bit random token, hex>`: where
    /// the transcoder publishes (`-f rtsp -rtsp_transport tcp <url>`).
    pub fn url(&self) -> String {
        self.url.clone()
    }
}

impl Drop for PublishHandle {
    fn drop(&mut self) {
        if let Some(token) = &self.token {
            self.publishers.deregister(token);
        }
    }
}

/// 128 bits from the operating system, hex encoded.
fn random_token() -> Option<String> {
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes).ok()?;
    let mut hex = String::with_capacity(32);
    for byte in bytes {
        let _ = write!(hex, "{byte:02x}");
    }
    Some(hex)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_are_128_bit_hex_and_distinct() {
        let a = random_token().unwrap();
        let b = random_token().unwrap();
        assert_eq!(a.len(), 32);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(a, b);
    }

    #[test]
    fn url_uses_loopback_for_an_unspecified_listen_address() {
        let p = Publishers::new(SocketAddr::from(([0, 0, 0, 0], 4242)));
        assert_eq!(p.inner.base, "rtsp://127.0.0.1:4242");
        let p = Publishers::new(SocketAddr::from(([127, 0, 0, 2], 4242)));
        assert_eq!(p.inner.base, "rtsp://127.0.0.2:4242");
        let p = Publishers::new(SocketAddr::from((Ipv6Addr::UNSPECIFIED, 4242)));
        assert_eq!(p.inner.base, "rtsp://[::1]:4242");
    }
}
