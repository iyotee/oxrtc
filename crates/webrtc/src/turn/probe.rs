//! One STUN server, asked through the shared socket for the address it sees.

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use std::time::Instant;

use oxrtc_core::CancellationToken;
use tokio::net::UdpSocket;
use tokio::sync::Notify;
use tracing::info;

use super::Reflexive;
use super::driver::{TICK, lock, resolve};
use super::url::ServerUrl;

/// A STUN server whose name did not resolve is tried again after this.
const PROBE_RETRY: Duration = Duration::from_secs(30);

/// One STUN server, asked through the shared socket.
pub(crate) struct Probe {
    url: ServerUrl,
    state: Mutex<(Reflexive, Option<SocketAddr>)>,
    wake: Notify,
    /// Ends the probe's task.
    stop: CancellationToken,
}

impl Probe {
    /// A probe that runs until `stop` is cancelled.
    pub(crate) fn new(url: ServerUrl, stop: CancellationToken) -> Self {
        Self { url, state: Mutex::new((Reflexive::new(Instant::now()), None)), wake: Notify::new(), stop }
    }

    pub(crate) fn stop(&self) {
        self.stop.cancel();
    }

    pub(crate) fn url(&self) -> &ServerUrl {
        &self.url
    }

    /// The shared socket's address as this server sees it.
    pub(crate) fn mapped(&self) -> Option<SocketAddr> {
        lock(&self.state).0.mapped()
    }

    pub(super) fn settled(&self) -> bool {
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

    pub(crate) async fn run(self: Arc<Self>, socket: Arc<UdpSocket>, changed: Arc<Notify>) {
        let v4 = socket.local_addr().map(|a| a.is_ipv4()).unwrap_or(true);
        loop {
            let Some(server) = resolve(&self.url, Some(v4)).await else {
                tokio::select! {
                    () = self.stop.cancelled() => return,
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
                    () = self.stop.cancelled() => return,
                    () = self.wake.notified() => {}
                    () = tokio::time::sleep_until(tokio::time::Instant::from_std(wake_at)) => {}
                }
            }
        }
    }
}
