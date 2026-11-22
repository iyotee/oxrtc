//! Long-lived answers counted in all and per address: `/api/ws` sockets, and
//! the `stream.mp4` and `frame.mp4` responses. A connection of the listener
//! carries one of them for as long as it lasts, so each kind has its own
//! bound under the listener's, which plain requests keep the rest of.

use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;

use oxrtc_core::{PerIpLimit, PerIpPermit, Refusals};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

/// The answers of one kind open, in all and per address.
pub(crate) struct Slots {
    /// The kind, as the log names it (`websocket`, `mp4 response`).
    what: &'static str,
    total: Arc<Semaphore>,
    max: usize,
    per_ip: PerIpLimit,
    full: Refusals,
}

/// One answer's two slots, held as long as it is open.
pub(crate) struct Slot {
    _total: OwnedSemaphorePermit,
    _address: PerIpPermit,
}

impl std::fmt::Debug for Slot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Slot").finish_non_exhaustive()
    }
}

impl Slots {
    /// At most `max` open (at least 1), `max_per_ip` from one address.
    pub(crate) fn new(what: &'static str, max: usize, max_per_ip: usize) -> Self {
        let max = max.max(1);
        Self {
            what,
            total: Arc::new(Semaphore::new(max)),
            max,
            per_ip: PerIpLimit::new(max_per_ip),
            full: Refusals::new(),
        }
    }

    /// A slot for an answer to `remote`, or `None` (warned about at most once
    /// per interval) when either bound is reached.
    pub(crate) fn try_open(&self, remote: Option<SocketAddr>) -> Option<Slot> {
        let what = self.what;
        let Ok(total) = self.total.clone().try_acquire_owned() else {
            if let Some(refused) = self.full.refused() {
                tracing::warn!(limit = self.max, refused, "api: {what} limit reached, refusing");
            }
            return None;
        };
        // An address the listener did not give (a test's router) counts as
        // the unspecified one.
        let ip = remote.map_or(IpAddr::from([0, 0, 0, 0]), |r| r.ip());
        let Some(address) = self.per_ip.try_acquire(ip) else {
            if let Some(refused) = self.per_ip.refused() {
                tracing::warn!(
                    limit = self.per_ip.max_per_ip(),
                    refused,
                    "api: per-address {what} limit reached, refusing"
                );
            }
            return None;
        };
        Some(Slot { _total: total, _address: address })
    }
}
