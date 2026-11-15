//! Per-address connection counting, shared by every listener.
//!
//! A listener's total (`max_connections`, `max_sessions`) stops a flood from
//! exhausting the host; it does not stop one client from taking every slot
//! and locking every other client out. [`PerIpLimit`] counts what each peer
//! address holds and refuses one more beyond its bound.
//!
//! Loopback is counted like any other address (`docs/DECISIONS.md`, D9): a
//! local process or an SSRF through the NVR backend reaches loopback too.
//!
//! Addresses are grouped the way a client can vary them: an IPv4 address (or
//! an IPv4-mapped IPv6 one) is its own group; a global or unique-local IPv6
//! address counts for its `/64`, because one host can pick any address in it.
//! A `/64` is a whole site, not one host: IPv6 clients on one LAN subnet share
//! a bound, as IPv4 hosts behind one NAT do. A link-local IPv6 address
//! (`fe80::/10`) is its own group: that prefix is the same on every link.
//!
//! The table is bounded by what it counts: an address has an entry only while
//! it holds a [`PerIpPermit`], and every permit sits behind a slot of its
//! listener's total.

use std::collections::HashMap;
use std::net::{IpAddr, Ipv6Addr};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

pub use crate::refusals::REPORT_EVERY;
use crate::refusals::Refusals;

/// Counts connections (or sessions) per peer address and refuses one beyond
/// [`max_per_ip`](Self::max_per_ip). Cloning shares the count.
#[derive(Clone)]
pub struct PerIpLimit {
    inner: Arc<Inner>,
}

struct Inner {
    max: usize,
    held: Mutex<HashMap<IpAddr, usize>>,
    refusals: Refusals,
}

impl Inner {
    fn held(&self) -> MutexGuard<'_, HashMap<IpAddr, usize>> {
        self.held.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl std::fmt::Debug for PerIpLimit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PerIpLimit")
            .field("max_per_ip", &self.inner.max)
            .field("addresses", &self.inner.held().len())
            .finish()
    }
}

/// The group an address is counted in.
pub fn group(ip: IpAddr) -> IpAddr {
    match ip.to_canonical() {
        IpAddr::V4(v4) => IpAddr::V4(v4),
        // `fe80::/64` is the same prefix on every link: grouping it would put
        // every link-local peer, on every interface, in one group.
        IpAddr::V6(v6) if v6.is_unicast_link_local() => IpAddr::V6(v6),
        IpAddr::V6(v6) => {
            let prefix = u128::from(v6) & !((1u128 << 64) - 1);
            IpAddr::V6(Ipv6Addr::from(prefix))
        }
    }
}

impl PerIpLimit {
    /// At most `max_per_ip` held at once per address group (at least 1).
    pub fn new(max_per_ip: usize) -> Self {
        Self {
            inner: Arc::new(Inner {
                max: max_per_ip.max(1),
                held: Mutex::new(HashMap::new()),
                refusals: Refusals::new(),
            }),
        }
    }

    /// Counts one refusal (a [`try_acquire`](Self::try_acquire) that gave
    /// `None`) and says whether to warn now: `Some(n)`, the refusals since the
    /// last warning, at most once per [`REPORT_EVERY`]; `None` otherwise.
    pub fn refused(&self) -> Option<u64> {
        self.inner.refusals.refused()
    }

    /// The bound.
    pub fn max_per_ip(&self) -> usize {
        self.inner.max
    }

    /// One more for `ip`, or `None` when its group already holds the bound.
    pub fn try_acquire(&self, ip: IpAddr) -> Option<PerIpPermit> {
        let key = group(ip);
        let mut held = self.inner.held();
        let count = held.entry(key).or_insert(0);
        if *count >= self.inner.max {
            return None;
        }
        *count += 1;
        Some(PerIpPermit { inner: self.inner.clone(), key })
    }

    /// How many `ip`'s group holds now.
    pub fn held_by(&self, ip: IpAddr) -> usize {
        self.inner.held().get(&group(ip)).copied().unwrap_or(0)
    }

    /// Address groups holding at least one permit.
    pub fn addresses(&self) -> usize {
        self.inner.held().len()
    }
}

/// One connection (or session) counted against its address; released on drop.
pub struct PerIpPermit {
    inner: Arc<Inner>,
    key: IpAddr,
}

impl std::fmt::Debug for PerIpPermit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PerIpPermit").field("group", &self.key).finish()
    }
}

impl Drop for PerIpPermit {
    fn drop(&mut self) {
        let mut held = self.inner.held();
        if let Some(count) = held.get_mut(&self.key) {
            *count = count.saturating_sub(1);
            if *count == 0 {
                held.remove(&self.key);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(text: &str) -> IpAddr {
        text.parse().unwrap()
    }

    #[test]
    fn each_address_gets_its_own_bound() {
        let limit = PerIpLimit::new(2);
        let a1 = limit.try_acquire(ip("127.0.0.1")).unwrap();
        let _a2 = limit.try_acquire(ip("127.0.0.1")).unwrap();
        assert!(limit.try_acquire(ip("127.0.0.1")).is_none(), "loopback is not exempt");
        let _b1 = limit.try_acquire(ip("192.168.1.20")).unwrap();
        assert_eq!(limit.held_by(ip("127.0.0.1")), 2);
        assert_eq!(limit.addresses(), 2);
        drop(a1);
        assert!(limit.try_acquire(ip("127.0.0.1")).is_some(), "a released permit is reusable");
    }

    #[test]
    fn entries_go_away_with_their_last_permit() {
        let limit = PerIpLimit::new(3);
        let permits: Vec<_> = (1..=50u8).filter_map(|n| limit.try_acquire(IpAddr::from([10, 0, 0, n]))).collect();
        assert_eq!(limit.addresses(), 50);
        drop(permits);
        assert_eq!(limit.addresses(), 0);
        assert_eq!(format!("{limit:?}"), "PerIpLimit { max_per_ip: 3, addresses: 0 }");
    }

    #[test]
    fn mapped_ipv4_and_one_ipv6_64_count_as_one_address() {
        let limit = PerIpLimit::new(1);
        let _v4 = limit.try_acquire(ip("192.0.2.7")).unwrap();
        assert!(limit.try_acquire(ip("::ffff:192.0.2.7")).is_none());
        let _v6 = limit.try_acquire(ip("2001:db8:1:2::1")).unwrap();
        assert!(limit.try_acquire(ip("2001:db8:1:2:ffff:ffff:ffff:ffff")).is_none(), "same /64");
        assert!(limit.try_acquire(ip("2001:db8:1:3::1")).is_some(), "another /64");
    }

    #[test]
    fn refusals_are_reported_once_per_interval() {
        // The throttle itself is tested in `refusals`; here, that clones of
        // a limit share one report.
        let limit = PerIpLimit::new(1);
        assert_eq!(limit.refused(), Some(1));
        assert_eq!(limit.clone().refused(), None);
    }

    #[test]
    fn link_local_ipv6_peers_count_one_by_one() {
        // fe80::/64 is the prefix of every link: two hosts on the LAN (or on
        // two different links) are not one client.
        let limit = PerIpLimit::new(1);
        let _a = limit.try_acquire(ip("fe80::1:2:3:4")).unwrap();
        assert!(limit.try_acquire(ip("fe80::1:2:3:4")).is_none(), "the same address is still bounded");
        assert!(limit.try_acquire(ip("fe80::aaaa:bbbb:cccc:dddd")).is_some(), "another host on the link");
        assert_eq!(group(ip("fe80::1:2:3:4")), ip("fe80::1:2:3:4"));
    }

    #[test]
    fn a_zero_bound_still_admits_one() {
        let limit = PerIpLimit::new(0);
        assert_eq!(limit.max_per_ip(), 1);
        let _one = limit.try_acquire(ip("127.0.0.1")).unwrap();
        assert!(limit.try_acquire(ip("127.0.0.1")).is_none());
    }

    #[test]
    fn clones_share_the_count() {
        let limit = PerIpLimit::new(1);
        let other = limit.clone();
        let _held = limit.try_acquire(ip("127.0.0.1")).unwrap();
        assert!(other.try_acquire(ip("127.0.0.1")).is_none());
    }
}
