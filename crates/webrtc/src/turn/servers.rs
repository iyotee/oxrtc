//! The configured ICE servers, and what the sessions ask of them.

use std::fmt;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use oxrtc_core::CancellationToken;
use tokio::net::UdpSocket;
use tokio::sync::Notify;
use tracing::{info, warn};

use super::Credentials;
use super::driver::{Deliver, Outgoing, Probe, Relay, settled};
use super::url::{Kind, ServerUrl, UrlError, parse};
use crate::stun::Hmac;

/// TURN servers used at most; the rest are warned about and left out.
const MAX_RELAYS: usize = 4;
/// STUN servers used at most.
const MAX_PROBES: usize = 4;

/// One entry of `webrtc.ice_servers`, as go2rtc and openIVR write it:
/// `stun:`/`turn:` URLs, and the TURN user name and credential (openIVR's
/// relay hands out time-limited ones and restarts the media server with
/// each new pair). Never displayed with its credential.
#[derive(Clone, PartialEq, Eq, Default)]
pub struct IceServer {
    /// `stun:host[:port]`, `turn:host[:port][?transport=udp|tcp]`.
    pub urls: Vec<String>,
    /// The TURN user name.
    pub username: Option<String>,
    /// The TURN password.
    pub credential: Option<String>,
}

impl fmt::Debug for IceServer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("IceServer")
            .field("urls", &self.urls.len())
            .field("username", &self.username.as_ref().map(|_| "***"))
            .field("credential", &self.credential.as_ref().map(|_| "***"))
            .finish()
    }
}

/// The servers in use, and their tasks.
pub(crate) struct IceServers {
    relays: Vec<Arc<Relay>>,
    probes: Vec<Arc<Probe>>,
    changed: Arc<Notify>,
    started: tokio::time::Instant,
}

impl Default for IceServers {
    fn default() -> Self {
        let changed = Arc::default();
        Self { relays: Vec::new(), probes: Vec::new(), changed, started: tokio::time::Instant::now() }
    }
}

impl IceServers {
    /// The usable servers of a configuration. A URL that cannot be used is
    /// warned about by its position: a malformed one may hold anything,
    /// a credential included.
    pub(crate) fn new(config: &[IceServer]) -> Self {
        let mut servers = Self::default();
        for (entry, server) in config.iter().enumerate() {
            for (index, raw) in server.urls.iter().enumerate() {
                let url = match parse(raw) {
                    Ok(url) => url,
                    // A valid URL of a transport not supported (`turns:`, TLS
                    // to the server): said, not warned.
                    Err(reason @ UrlError::Secure(_)) => {
                        info!(entry, url = index, %reason, "webrtc.ice_servers: URL not used");
                        continue;
                    }
                    Err(reason) => {
                        warn!(entry, url = index, %reason, "webrtc.ice_servers: URL not used");
                        continue;
                    }
                };
                servers.add(url, server, entry);
            }
        }
        servers
    }

    fn add(&mut self, url: ServerUrl, server: &IceServer, entry: usize) {
        match url.kind {
            Kind::Stun if self.probes.len() < MAX_PROBES => {
                if !self.probes.iter().any(|p| p.url() == &url) {
                    self.probes.push(Arc::new(Probe::new(url)));
                }
            }
            Kind::Turn if self.relays.len() < MAX_RELAYS => {
                let (Some(username), Some(password)) = (&server.username, &server.credential) else {
                    warn!(entry, server = %url, "webrtc.ice_servers: a TURN server needs a username and a credential");
                    return;
                };
                let credentials = Credentials { username: username.clone(), password: password.clone() };
                if !self.relays.iter().any(|r| r.url() == &url && r.credentials() == &credentials) {
                    self.relays.push(Arc::new(Relay::new(url, credentials)));
                }
            }
            _ => warn!(entry, server = %url, "webrtc.ice_servers: too many servers of this kind; not used"),
        }
    }

    /// Whether any server is in use.
    pub(crate) fn is_empty(&self) -> bool {
        self.relays.is_empty() && self.probes.is_empty()
    }

    /// Starts every server's task; they stop with `cancel`.
    pub(crate) fn start(&self, shared: Arc<UdpSocket>, hmac: Hmac, deliver: Deliver, cancel: &CancellationToken) {
        for relay in &self.relays {
            tokio::spawn(relay.clone().run(hmac, deliver.clone(), self.changed.clone(), cancel.clone()));
        }
        for probe in &self.probes {
            tokio::spawn(probe.clone().run(shared.clone(), self.changed.clone(), cancel.clone()));
        }
    }

    /// Waits for every server's first answer or failure, until `limit`
    /// after startup: an offer answered in the first instants still gets the
    /// relayed candidate, and a server that never answers (its allocation
    /// fails after about 24 s) delays no offer after that window.
    pub(crate) async fn settle(&self, limit: Duration) {
        let deadline = self.started + limit;
        loop {
            let notified = self.changed.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if settled(&self.relays, &self.probes) {
                return;
            }
            if tokio::time::timeout_at(deadline, notified).await.is_err() {
                return;
            }
        }
    }

    /// The shared socket's mapped addresses (server-reflexive candidates).
    pub(crate) fn reflexive(&self) -> Vec<SocketAddr> {
        let mut found: Vec<SocketAddr> = Vec::new();
        for mapped in self.probes.iter().filter_map(|p| p.mapped()) {
            if !found.contains(&mapped) {
                found.push(mapped);
            }
        }
        found
    }

    /// The relayed addresses allocated now.
    pub(crate) fn relayed(&self) -> Vec<SocketAddr> {
        self.relays.iter().filter_map(|r| r.relayed()).collect()
    }

    /// The relay whose allocation is `relayed`.
    pub(crate) fn relay_for(&self, relayed: SocketAddr) -> Option<&Arc<Relay>> {
        self.relays.iter().find(|r| r.relayed() == Some(relayed))
    }

    /// A datagram on the shared socket from a STUN server: consumed when it
    /// answers a probe.
    pub(crate) fn probe_answer(&self, source: SocketAddr, data: &[u8]) -> bool {
        self.probes.iter().any(|p| p.handle(source, data))
    }

    /// Permissions on every relay for an offer's candidate IPs.
    pub(crate) fn permit(&self, ips: &[IpAddr]) -> Vec<Outgoing> {
        if ips.is_empty() {
            return Vec::new();
        }
        self.relays.iter().filter_map(|r| r.permit(ips)).collect()
    }
}
