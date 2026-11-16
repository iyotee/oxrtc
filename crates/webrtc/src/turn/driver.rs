//! `webrtc.ice_servers` at work: one task per TURN server keeps an
//! allocation on a socket or a TCP connection of its own (D13, D21), and
//! [`super::probe`] one per STUN server. What a peer sends through a relay is
//! handed to [`Deliver`], which treats it as a datagram that arrived on the
//! relayed address.
//!
//! A relay is one allocation made with one credential. When the credential
//! changes at runtime (D22) a new relay is made beside the old one, which is
//! retired once the new holds: it is no longer offered to a new session, and
//! keeps serving the sessions that were given its address.

use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use oxrtc_core::CancellationToken;
use tokio::sync::Notify;
use tracing::{debug, info, warn};

use super::allocation::{Event, Failure, Incoming};
use super::link::{self, Link};
use super::retry::Retry;
use super::url::{ServerUrl, Transport};
use super::{Allocation, Credentials};
use crate::stun::Hmac;

/// Hands a relayed datagram to the sessions: `(data, peer, relayed address)`.
pub(crate) type Deliver = Arc<dyn Fn(&[u8], SocketAddr, SocketAddr) + Send + Sync>;

/// How long a server name may take to resolve.
const RESOLVE_TIMEOUT: Duration = Duration::from_secs(5);
/// The longest a driver sleeps: a peer first used after a timer was set is
/// seen by then (its permission and channel are refreshed ahead by a minute).
pub(super) const TICK: Duration = Duration::from_secs(5);

const STARTING: u8 = 0;
const READY: u8 = 1;
const DOWN: u8 = 2;

pub(super) fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
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
    /// Ends the allocation (released at the server) and its task.
    stop: CancellationToken,
    /// When this relay stopped being offered to new sessions.
    retired: Mutex<Option<Instant>>,
    /// The relays this one replaces, retired once it holds an allocation.
    replaces: Mutex<Vec<Arc<Relay>>>,
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
    /// A relay that runs until `stop` is cancelled.
    pub(crate) fn new(url: ServerUrl, credentials: Credentials, stop: CancellationToken) -> Self {
        Self {
            url,
            credentials,
            active: Mutex::new(None),
            status: AtomicU8::new(STARTING),
            wake: Notify::new(),
            stop,
            retired: Mutex::new(None),
            replaces: Mutex::new(Vec::new()),
        }
    }

    /// Retires `old` once this relay holds an allocation.
    pub(crate) fn replaces(&self, old: Vec<Arc<Relay>>) {
        *lock(&self.replaces) = old;
    }

    /// Stops offering this relay to new sessions; the sessions that have its
    /// address keep it. Idempotent.
    pub(crate) fn retire(&self, now: Instant) {
        lock(&self.retired).get_or_insert(now);
    }

    pub(crate) fn retired_at(&self) -> Option<Instant> {
        *lock(&self.retired)
    }

    /// Ends the allocation: released at the server, the task ends.
    pub(crate) fn stop(&self) {
        self.stop.cancel();
    }

    pub(crate) fn is_stopped(&self) -> bool {
        self.stop.is_cancelled()
    }

    /// Whether an allocation is held (ready to be offered).
    pub(crate) fn is_ready(&self) -> bool {
        self.relayed().is_some()
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

    pub(super) fn settled(&self) -> bool {
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
    pub(crate) async fn run(self: Arc<Self>, hmac: Hmac, deliver: Deliver, changed: Arc<Notify>) {
        let mut retry = Retry::new();
        loop {
            let started = Instant::now();
            let outcome = self.allocate(hmac, &deliver, &changed).await;
            *lock(&self.active) = None;
            self.status.store(DOWN, Ordering::Release);
            changed.notify_waiters();
            let Some(failure) = outcome else { return };
            if self.retired_at().is_some() {
                // Its credential was replaced and may be expired: the
                // sessions that had this address lose it, none is made again.
                debug!(server = %self.url, reason = %failure, "a retired turn allocation ended");
                self.stop.cancel();
                return;
            }
            let wait = retry.after(&failure, started.elapsed());
            warn!(server = %self.url, reason = %failure, retry_in_s = wait.as_secs(), "turn allocation failed");
            tokio::select! {
                () = self.stop.cancelled() => return,
                () = tokio::time::sleep(wait) => {}
            }
        }
    }

    /// One allocation, from resolving the server to its end: `None` when
    /// stopped, else why it failed.
    async fn allocate(&self, hmac: Hmac, deliver: &Deliver, changed: &Notify) -> Option<Failure> {
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
                        self.supersede();
                        changed.notify_waiters();
                    }
                    Event::Failed(failure) => return Some(failure),
                }
            }
            let now = Instant::now();
            let wake_at = next.unwrap_or(now + TICK).min(now + TICK);
            tokio::select! {
                biased;
                () = self.stop.cancelled() => {
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

impl Relay {
    /// This relay holds an allocation: the ones it replaces are retired.
    fn supersede(&self) {
        let old = std::mem::take(&mut *lock(&self.replaces));
        let now = Instant::now();
        for relay in old {
            relay.retire(now);
            info!(server = %relay.url, "turn credentials replaced, the old allocation keeps its sessions");
        }
    }
}

fn drain(allocation: &mut Allocation) -> Vec<Vec<u8>> {
    std::iter::from_fn(|| allocation.poll_transmit()).collect()
}

/// The server's address, IPv4 first, or of `family` when given.
pub(super) async fn resolve(url: &ServerUrl, family: Option<bool>) -> Option<SocketAddr> {
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
