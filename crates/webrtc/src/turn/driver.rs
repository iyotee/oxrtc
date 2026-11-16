//! `webrtc.ice_servers` at work: one task per TURN server keeps an
//! allocation on a socket or a TCP connection of its own (D13, D21), one
//! task per STUN server keeps the shared socket's mapped address. What a peer sends through a relay is
//! handed to [`Deliver`], which treats it as a datagram that arrived on the
//! relayed address.

use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use oxrtc_core::CancellationToken;
use tokio::net::UdpSocket;
use tokio::sync::Notify;
use tracing::{debug, info, warn};

use super::allocation::{Event, Failure, Incoming};
use super::link::{self, Link};
use super::retry::Retry;
use super::url::{ServerUrl, Transport};
use super::{Allocation, Credentials, Reflexive};
use crate::stun::Hmac;

/// Hands a relayed datagram to the sessions: `(data, peer, relayed address)`.
pub(crate) type Deliver = Arc<dyn Fn(&[u8], SocketAddr, SocketAddr) + Send + Sync>;

/// How long a server name may take to resolve.
const RESOLVE_TIMEOUT: Duration = Duration::from_secs(5);
/// The longest a driver sleeps: a peer first used after a timer was set is
/// seen by then (its permission and channel are refreshed ahead by a minute).
const TICK: Duration = Duration::from_secs(5);
/// A STUN server whose name did not resolve is tried again after this.
const PROBE_RETRY: Duration = Duration::from_secs(30);

const STARTING: u8 = 0;
const READY: u8 = 1;
const DOWN: u8 = 2;

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

struct Active {
    allocation: Allocation,
    link: Link,
}

/// One TURN server's allocation.
pub(crate) struct Relay {
    url: ServerUrl,
    credentials: Credentials,
    active: Mutex<Option<Active>>,
    status: AtomicU8,
    wake: Notify,
}

/// What a session sends through a relay: messages for its server.
pub(crate) struct Outgoing {
    link: Link,
    datagrams: Vec<Vec<u8>>,
}

impl Outgoing {
    pub(crate) async fn send(self) {
        for datagram in self.datagrams {
            self.link.send(datagram).await;
        }
    }
}

impl Relay {
    pub(crate) fn new(url: ServerUrl, credentials: Credentials) -> Self {
        Self { url, credentials, active: Mutex::new(None), status: AtomicU8::new(STARTING), wake: Notify::new() }
    }

    pub(crate) fn url(&self) -> &ServerUrl {
        &self.url
    }

    pub(crate) fn credentials(&self) -> &Credentials {
        &self.credentials
    }

    /// The relayed address, while allocated.
    pub(crate) fn relayed(&self) -> Option<SocketAddr> {
        lock(&self.active).as_ref().and_then(|a| a.allocation.relayed())
    }

    fn settled(&self) -> bool {
        self.status.load(Ordering::Acquire) != STARTING
    }

    /// `data` for `peer` through the relay, with any request it calls for.
    pub(crate) fn send(&self, peer: SocketAddr, data: &[u8]) -> Option<Outgoing> {
        let mut active = lock(&self.active);
        let active = active.as_mut()?;
        let now = Instant::now();
        let mut datagrams = vec![active.allocation.send(now, peer, data)?];
        let requests = drain(&mut active.allocation);
        if !requests.is_empty() {
            // The driver times their retransmissions.
            self.wake.notify_one();
        }
        datagrams.extend(requests);
        Some(Outgoing { link: active.link.clone(), datagrams })
    }

    /// Permissions for the IPs of an offer's candidates, so that the
    /// viewer's first checks are not dropped by the server.
    pub(crate) fn permit(&self, ips: &[IpAddr]) -> Option<Outgoing> {
        let mut active = lock(&self.active);
        let active = active.as_mut()?;
        let now = Instant::now();
        let relayed = active.allocation.relayed()?;
        for ip in ips.iter().filter(|ip| ip.is_ipv4() == relayed.is_ipv4()) {
            active.allocation.permit(now, *ip);
        }
        let datagrams = drain(&mut active.allocation);
        self.wake.notify_one();
        Some(Outgoing { link: active.link.clone(), datagrams })
    }

    /// Keeps an allocation until `cancel`, allocating again after a failure.
    pub(crate) async fn run(
        self: Arc<Self>,
        hmac: Hmac,
        deliver: Deliver,
        changed: Arc<Notify>,
        cancel: CancellationToken,
    ) {
        let mut retry = Retry::new();
        loop {
            let started = Instant::now();
            let outcome = self.allocate(hmac, &deliver, &changed, &cancel).await;
            *lock(&self.active) = None;
            self.status.store(DOWN, Ordering::Release);
            changed.notify_waiters();
            let Some(failure) = outcome else { return };
            let wait = retry.after(&failure, started.elapsed());
            warn!(server = %self.url, reason = %failure, retry_in_s = wait.as_secs(), "turn allocation failed");
            tokio::select! {
                () = cancel.cancelled() => return,
                () = tokio::time::sleep(wait) => {}
            }
        }
    }

    /// One allocation, from resolving the server to its end: `None` when
    /// stopped, else why it failed.
    async fn allocate(
        &self,
        hmac: Hmac,
        deliver: &Deliver,
        changed: &Notify,
        cancel: &CancellationToken,
    ) -> Option<Failure> {
        let server = match resolve(&self.url, None).await {
            Some(server) => server,
            None => return Some(Failure::Lost("the server name did not resolve")),
        };
        let (link, mut wire, closing) = match link::connect(&self.url, server).await {
            Ok(connected) => connected,
            Err(failure) => return Some(failure),
        };
        let now = Instant::now();
        let allocation = match self.url.transport {
            Transport::Udp => Allocation::new(self.credentials.clone(), hmac, now),
            Transport::Tcp => Allocation::reliable(self.credentials.clone(), hmac, now),
        };
        *lock(&self.active) = Some(Active { allocation, link: link.clone() });
        loop {
            let (datagrams, events, next) = {
                let mut active = lock(&self.active);
                let allocation = &mut active.as_mut()?.allocation;
                let events: Vec<Event> = std::iter::from_fn(|| allocation.poll_event()).collect();
                (drain(allocation), events, allocation.poll_timeout())
            };
            for datagram in datagrams {
                link.send(datagram).await;
            }
            for event in events {
                match event {
                    Event::Allocated { relayed, mapped, lifetime } => {
                        let mapped = mapped.map(|m| m.to_string()).unwrap_or_default();
                        info!(server = %self.url, %relayed, %mapped, lifetime_s = lifetime.as_secs(), "turn allocated");
                        self.status.store(READY, Ordering::Release);
                        changed.notify_waiters();
                    }
                    Event::Failed(failure) => return Some(failure),
                }
            }
            let now = Instant::now();
            let wake_at = next.unwrap_or(now + TICK).min(now + TICK);
            tokio::select! {
                biased;
                () = cancel.cancelled() => {
                    let release = {
                        let mut active = lock(&self.active);
                        let allocation = &mut active.as_mut()?.allocation;
                        allocation.close(Instant::now());
                        drain(allocation)
                    };
                    for datagram in release {
                        link.send_now(datagram);
                    }
                    closing.finish().await;
                    return None;
                }
                () = closing.lost() => return Some(Failure::Lost("the connection to the server closed")),
                received = wire.recv() => match received {
                    Ok(Some(data)) => {
                        let relayed = {
                            let mut active = lock(&self.active);
                            let allocation = &mut active.as_mut()?.allocation;
                            match (allocation.handle(Instant::now(), data), allocation.relayed()) {
                                (Incoming::Relayed { peer, data }, Some(relayed)) => {
                                    Some((data.to_vec(), peer, relayed))
                                }
                                _ => None,
                            }
                        };
                        if let Some((data, peer, relayed)) = relayed {
                            deliver(&data, peer, relayed);
                        }
                    }
                    Ok(None) => {}
                    Err(failure) => return Some(failure),
                },
                () = self.wake.notified() => {}
                () = tokio::time::sleep_until(tokio::time::Instant::from_std(wake_at)) => {
                    if let Some(active) = lock(&self.active).as_mut() {
                        active.allocation.handle_timeout(Instant::now());
                    }
                }
            }
        }
    }
}

fn drain(allocation: &mut Allocation) -> Vec<Vec<u8>> {
    std::iter::from_fn(|| allocation.poll_transmit()).collect()
}

/// The server's address, IPv4 first, or of `family` when given.
async fn resolve(url: &ServerUrl, family: Option<bool>) -> Option<SocketAddr> {
    let lookup = tokio::net::lookup_host((url.host.as_str(), url.port));
    let addrs: Vec<SocketAddr> = match tokio::time::timeout(RESOLVE_TIMEOUT, lookup).await {
        Ok(Ok(addrs)) => addrs.collect(),
        Ok(Err(error)) => {
            debug!(server = %url, %error, "cannot resolve the ICE server");
            return None;
        }
        Err(_) => return None,
    };
    match family {
        Some(v4) => addrs.into_iter().find(|a| a.is_ipv4() == v4),
        None => addrs.iter().find(|a| a.is_ipv4()).or_else(|| addrs.first()).copied(),
    }
}

/// One STUN server, asked through the shared socket.
pub(crate) struct Probe {
    url: ServerUrl,
    state: Mutex<(Reflexive, Option<SocketAddr>)>,
    wake: Notify,
}

impl Probe {
    pub(crate) fn new(url: ServerUrl) -> Self {
        Self { url, state: Mutex::new((Reflexive::new(Instant::now()), None)), wake: Notify::new() }
    }

    pub(crate) fn url(&self) -> &ServerUrl {
        &self.url
    }

    /// The shared socket's address as this server sees it.
    pub(crate) fn mapped(&self) -> Option<SocketAddr> {
        lock(&self.state).0.mapped()
    }

    fn settled(&self) -> bool {
        lock(&self.state).0.settled()
    }

    /// A datagram on the shared socket: `true` when it was this server's answer.
    pub(crate) fn handle(&self, source: SocketAddr, data: &[u8]) -> bool {
        let answered = {
            let mut state = lock(&self.state);
            state.1 == Some(source) && state.0.handle(Instant::now(), data)
        };
        if answered {
            self.wake.notify_one();
        }
        answered
    }

    pub(crate) async fn run(self: Arc<Self>, socket: Arc<UdpSocket>, changed: Arc<Notify>, cancel: CancellationToken) {
        let v4 = socket.local_addr().map(|a| a.is_ipv4()).unwrap_or(true);
        loop {
            let Some(server) = resolve(&self.url, Some(v4)).await else {
                tokio::select! {
                    () = cancel.cancelled() => return,
                    () = tokio::time::sleep(PROBE_RETRY) => continue,
                }
            };
            lock(&self.state).1 = Some(server);
            let mut known = None;
            loop {
                let (datagrams, next, mapped) = {
                    let mut state = lock(&self.state);
                    state.0.handle_timeout(Instant::now());
                    let datagrams: Vec<Vec<u8>> = std::iter::from_fn(|| state.0.poll_transmit()).collect();
                    (datagrams, state.0.poll_timeout(), state.0.mapped())
                };
                for datagram in datagrams {
                    let _ = socket.send_to(&datagram, server).await;
                }
                if mapped != known {
                    if let Some(mapped) = mapped {
                        info!(server = %self.url, %mapped, "stun mapped address");
                    }
                    known = mapped;
                    changed.notify_waiters();
                }
                let now = Instant::now();
                let wake_at = next.unwrap_or(now + TICK).min(now + TICK);
                tokio::select! {
                    () = cancel.cancelled() => return,
                    () = self.wake.notified() => {}
                    () = tokio::time::sleep_until(tokio::time::Instant::from_std(wake_at)) => {}
                }
            }
        }
    }
}

/// Whether every server has given its first answer or failure, for
/// [`crate::WebRtcServer::answer`] to wait on at startup.
pub(crate) fn settled(relays: &[Arc<Relay>], probes: &[Arc<Probe>]) -> bool {
    relays.iter().all(|r| r.settled()) && probes.iter().all(|p| p.settled())
}
