//! The configured ICE servers, what the sessions ask of them, and their
//! replacement while the server runs (D22).

use std::fmt;
use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use oxrtc_core::CancellationToken;
use tokio::net::UdpSocket;
use tokio::sync::Notify;
use tracing::{info, warn};

use super::Credentials;
use super::driver::{Deliver, Outgoing, Relay, lock};
use super::probe::Probe;
use super::url::{Kind, ServerUrl, UrlError, parse};
use crate::stun::Hmac;

/// TURN servers used at most; the rest are warned about and left out.
const MAX_RELAYS: usize = 4;
/// STUN servers used at most.
const MAX_PROBES: usize = 4;
/// Retired allocations kept at most (each holds a socket or a connection at
/// the server): past it the oldest end, and the sessions that had its address
/// with them. Credentials that rotate every few minutes never reach it.
pub(crate) const MAX_RETIRED: usize = 8;

/// One entry of `webrtc.ice_servers`, as go2rtc and openIVR write it:
/// `stun:`/`turn:` URLs, and the TURN user name and credential (openIVR's
/// relay hands out time-limited ones and replaces them in the running
/// server). Never displayed with its credential.
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

/// What a replacement kept of the servers it was given.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IceSummary {
    /// TURN servers in use (URLs that were not usable are not counted).
    pub turn: usize,
    /// STUN servers in use.
    pub stun: usize,
}

/// The usable servers of a configuration.
struct Wanted {
    relays: Vec<(ServerUrl, Credentials)>,
    probes: Vec<ServerUrl>,
}

impl Wanted {
    /// A URL that cannot be used is warned about by its position: a
    /// malformed one may hold anything, a credential included.
    fn of(config: &[IceServer]) -> Self {
        let mut wanted = Self { relays: Vec::new(), probes: Vec::new() };
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
                wanted.add(url, server, entry);
            }
        }
        wanted
    }

    fn add(&mut self, url: ServerUrl, server: &IceServer, entry: usize) {
        match url.kind {
            Kind::Stun if self.probes.len() < MAX_PROBES => {
                if !self.probes.contains(&url) {
                    self.probes.push(url);
                }
            }
            Kind::Turn if self.relays.len() < MAX_RELAYS => {
                let (Some(username), Some(password)) = (&server.username, &server.credential) else {
                    warn!(entry, server = %url, "webrtc.ice_servers: a TURN server needs a username and a credential");
                    return;
                };
                let credentials = Credentials { username: username.clone(), password: password.clone() };
                if !self.relays.iter().any(|(u, c)| *u == url && *c == credentials) {
                    self.relays.push((url, credentials));
                }
            }
            _ => warn!(entry, server = %url, "webrtc.ice_servers: too many servers of this kind; not used"),
        }
    }
}

#[derive(Default)]
struct State {
    /// Every allocation alive: the ones offered, and the retired ones that
    /// still serve a session.
    relays: Vec<Arc<Relay>>,
    probes: Vec<Arc<Probe>>,
}

/// What the tasks need, known once the server is bound.
struct Runtime {
    socket: Arc<UdpSocket>,
    hmac: Hmac,
    deliver: Deliver,
}

/// The servers in use, and their tasks.
pub(crate) struct IceServers {
    state: Mutex<State>,
    changed: Arc<Notify>,
    started: tokio::time::Instant,
    /// The server's: every task is a child of it.
    cancel: CancellationToken,
    runtime: OnceLock<Runtime>,
}

impl IceServers {
    /// The usable servers of a configuration; their tasks start with
    /// [`Self::start`] and stop with `cancel`.
    pub(crate) fn new(config: &[IceServer], cancel: &CancellationToken) -> Self {
        let wanted = Wanted::of(config);
        let state = State {
            relays: wanted
                .relays
                .into_iter()
                .map(|(url, credentials)| Arc::new(Relay::new(url, credentials, cancel.child_token())))
                .collect(),
            probes: wanted.probes.into_iter().map(|url| Arc::new(Probe::new(url, cancel.child_token()))).collect(),
        };
        Self {
            state: Mutex::new(state),
            changed: Arc::default(),
            started: tokio::time::Instant::now(),
            cancel: cancel.clone(),
            runtime: OnceLock::new(),
        }
    }

    /// Starts every server's task, and the ones a later [`Self::replace`] adds.
    pub(crate) fn start(&self, shared: Arc<UdpSocket>, hmac: Hmac, deliver: Deliver) {
        if self.runtime.set(Runtime { socket: shared, hmac, deliver }).is_err() {
            return;
        }
        let state = lock(&self.state);
        for relay in &state.relays {
            self.launch_relay(relay);
        }
        for probe in &state.probes {
            self.launch_probe(probe);
        }
    }

    fn launch_relay(&self, relay: &Arc<Relay>) {
        if let Some(runtime) = self.runtime.get() {
            tokio::spawn(relay.clone().run(runtime.hmac, runtime.deliver.clone(), self.changed.clone()));
        }
    }

    fn launch_probe(&self, probe: &Arc<Probe>) {
        if let Some(runtime) = self.runtime.get() {
            tokio::spawn(probe.clone().run(runtime.socket.clone(), self.changed.clone()));
        }
    }

    /// The relays a new session may be offered: not retired.
    fn offered(&self) -> Vec<Arc<Relay>> {
        lock(&self.state).relays.iter().filter(|r| r.retired_at().is_none()).cloned().collect()
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
            if self.settled() {
                return;
            }
            if tokio::time::timeout_at(deadline, notified).await.is_err() {
                return;
            }
        }
    }

    /// Whether every server has given its first answer or failure.
    fn settled(&self) -> bool {
        let state = lock(&self.state);
        state.relays.iter().filter(|r| r.retired_at().is_none()).all(|r| r.settled())
            && state.probes.iter().all(|p| p.settled())
    }

    /// The shared socket's mapped addresses (server-reflexive candidates).
    pub(crate) fn reflexive(&self) -> Vec<SocketAddr> {
        let mut found: Vec<SocketAddr> = Vec::new();
        for mapped in lock(&self.state).probes.iter().filter_map(|p| p.mapped()) {
            if !found.contains(&mapped) {
                found.push(mapped);
            }
        }
        found
    }

    /// The relayed addresses a new session is offered.
    pub(crate) fn relayed(&self) -> Vec<SocketAddr> {
        self.offered().iter().filter_map(|r| r.relayed()).collect()
    }

    /// The relay whose allocation is `relayed`, a retired one included: a
    /// session keeps the address it was offered.
    pub(crate) fn relay_for(&self, relayed: SocketAddr) -> Option<Arc<Relay>> {
        lock(&self.state).relays.iter().find(|r| r.relayed() == Some(relayed)).cloned()
    }

    /// A datagram on the shared socket from a STUN server: consumed when it
    /// answers a probe.
    pub(crate) fn probe_answer(&self, source: SocketAddr, data: &[u8]) -> bool {
        let probes: Vec<Arc<Probe>> = lock(&self.state).probes.clone();
        probes.iter().any(|p| p.handle(source, data))
    }

    /// Permissions on every offered relay for an offer's candidate IPs.
    pub(crate) fn permit(&self, ips: &[IpAddr]) -> Vec<Outgoing> {
        if ips.is_empty() {
            return Vec::new();
        }
        self.offered().iter().filter_map(|r| r.permit(ips)).collect()
    }

    /// Replaces the servers while the server runs. A STUN server or a TURN
    /// server with the credential it has is left as it is. A TURN server with
    /// another credential gets a new allocation beside the old one, which is
    /// retired once the new holds (a session keeps the address it was
    /// offered, so it keeps its allocation); one that is no longer wanted is
    /// retired at once.
    pub(crate) fn replace(&self, config: &[IceServer]) -> IceSummary {
        let wanted = Wanted::of(config);
        let summary = IceSummary { turn: wanted.relays.len(), stun: wanted.probes.len() };
        let now = Instant::now();
        let mut state = lock(&self.state);
        self.replace_probes(&mut state, wanted.probes);
        self.replace_relays(&mut state, wanted.relays, now);
        retire_beyond_limit(&mut state);
        drop(state);
        self.changed.notify_waiters();
        info!(turn = summary.turn, stun = summary.stun, "webrtc.ice_servers replaced");
        summary
    }

    fn replace_probes(&self, state: &mut State, wanted: Vec<ServerUrl>) {
        state.probes.retain(|probe| {
            let kept = wanted.contains(probe.url());
            if !kept {
                probe.stop();
            }
            kept
        });
        for url in wanted {
            if !state.probes.iter().any(|p| *p.url() == url) {
                let probe = Arc::new(Probe::new(url, self.cancel.child_token()));
                self.launch_probe(&probe);
                state.probes.push(probe);
            }
        }
    }

    fn replace_relays(&self, state: &mut State, wanted: Vec<(ServerUrl, Credentials)>, now: Instant) {
        state.relays.retain(|relay| !relay.is_stopped());
        let mut fresh = Vec::new();
        let mut kept = Vec::new();
        for (url, credentials) in wanted {
            let same = |r: &&Arc<Relay>| r.retired_at().is_none() && *r.url() == url && *r.credentials() == credentials;
            match state.relays.iter().find(same) {
                Some(relay) => kept.push(relay.clone()),
                None => fresh.push((url, credentials)),
            }
        }
        // What stays offered but is not wanted any more: an allocation that
        // never held is dropped, one that serves sessions is replaced.
        let mut superseded = Vec::new();
        for relay in &state.relays {
            if relay.retired_at().is_some() || kept.iter().any(|k| Arc::ptr_eq(k, relay)) {
                continue;
            }
            if relay.is_ready() {
                superseded.push(relay.clone());
            } else {
                relay.stop();
            }
        }
        state.relays.retain(|relay| !relay.is_stopped());
        let fresh_urls: Vec<ServerUrl> = fresh.iter().map(|(url, _)| url.clone()).collect();
        for (url, credentials) in fresh {
            let relay = Arc::new(Relay::new(url, credentials, self.cancel.child_token()));
            relay.replaces(superseded.iter().filter(|old| old.url() == relay.url()).cloned().collect());
            self.launch_relay(&relay);
            state.relays.push(relay);
        }
        // Those with no new allocation to take over from end up retired now.
        for old in &superseded {
            if !fresh_urls.contains(old.url()) {
                old.retire(now);
            }
        }
    }

    /// Ends the retired allocations nobody uses: `in_use` says whether a
    /// session still has a relayed address, and an allocation is kept at
    /// least `grace` after it was retired (a session offered its address
    /// needs that long to connect).
    pub(crate) fn reap(&self, now: Instant, grace: Duration, in_use: impl Fn(SocketAddr) -> bool) {
        let due: Vec<Arc<Relay>> = lock(&self.state)
            .relays
            .iter()
            .filter(|r| r.retired_at().is_some_and(|at| now.saturating_duration_since(at) >= grace))
            .cloned()
            .collect();
        for relay in due {
            if !relay.relayed().is_some_and(&in_use) {
                relay.stop();
            }
        }
        lock(&self.state).relays.retain(|relay| !relay.is_stopped());
    }

    #[cfg(test)]
    pub(crate) fn alive(&self) -> Vec<Arc<Relay>> {
        lock(&self.state).relays.clone()
    }
}

/// Ends the oldest retired allocations beyond [`MAX_RETIRED`].
fn retire_beyond_limit(state: &mut State) {
    let mut retired: Vec<(Instant, Arc<Relay>)> =
        state.relays.iter().filter_map(|r| Some((r.retired_at()?, r.clone()))).collect();
    if retired.len() <= MAX_RETIRED {
        return;
    }
    retired.sort_by_key(|(at, _)| *at);
    let excess = retired.len() - MAX_RETIRED;
    for (_, relay) in retired.into_iter().take(excess) {
        warn!(server = %relay.url(), "too many retired turn allocations; the oldest ends with its sessions");
        relay.stop();
    }
    state.relays.retain(|relay| !relay.is_stopped());
}

#[cfg(test)]
#[path = "servers_tests.rs"]
mod tests;
