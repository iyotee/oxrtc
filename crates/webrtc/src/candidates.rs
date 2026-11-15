//! The candidates an answer advertises (host, ICE-TCP, server-reflexive,
//! relayed), the peer addresses an offer lists, and the local address a
//! datagram is said to have arrived on.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

use str0m::Candidate;
use tracing::warn;

/// Most host candidates in one answer.
pub(crate) const MAX_CANDIDATES: usize = 16;
/// Most peer IPs read from one offer (each a TURN permission).
pub(crate) const MAX_OFFER_ADDRESSES: usize = 16;

/// Every local candidate of one answer.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct LocalCandidates {
    /// UDP host candidates, on the shared socket.
    pub(crate) host: Vec<SocketAddr>,
    /// ICE-TCP passive candidates: each host address at the TCP port.
    pub(crate) tcp: Vec<SocketAddr>,
    /// Server-reflexive candidates: a STUN server's view of the shared
    /// socket, and the host candidate it stands for.
    pub(crate) reflexive: Vec<(SocketAddr, SocketAddr)>,
    /// Relayed candidates: TURN allocations.
    pub(crate) relayed: Vec<SocketAddr>,
}

impl LocalCandidates {
    pub(crate) fn gather(
        host: Vec<SocketAddr>,
        tcp_port: Option<u16>,
        mapped: &[SocketAddr],
        relayed: Vec<SocketAddr>,
    ) -> Self {
        let mut tcp: Vec<SocketAddr> = Vec::new();
        if let Some(port) = tcp_port {
            for addr in host.iter().map(|h| SocketAddr::new(h.ip(), port)) {
                if !tcp.contains(&addr) {
                    tcp.push(addr);
                }
            }
        }
        let reflexive = mapped
            .iter()
            .filter(|m| !host.contains(m))
            .filter_map(|m| reflexive_base(&host, *m).map(|base| (*m, base)))
            .collect();
        Self { host, tcp, reflexive, relayed }
    }

    /// Whether every candidate is a host one: ICE-lite then suffices (and
    /// str0m's ICE-lite refuses any other kind).
    pub(crate) fn host_only(&self) -> bool {
        self.reflexive.is_empty() && self.relayed.is_empty()
    }
}

/// The host candidate a mapped address stands for: the first of its family
/// that is not loopback (the one a packet to a STUN server leaves by, on
/// any machine with one route out).
fn reflexive_base(host: &[SocketAddr], mapped: SocketAddr) -> Option<SocketAddr> {
    let family = |h: &&SocketAddr| h.is_ipv4() == mapped.is_ipv4();
    host.iter().filter(family).find(|h| !h.ip().is_loopback()).or_else(|| host.iter().find(family)).copied()
}

/// The IPs of the candidates an offer lists (a browser that waited for its
/// gathering lists its host, server-reflexive and relayed ones), at most
/// [`MAX_OFFER_ADDRESSES`]. A candidate str0m cannot read (an mDNS name) is
/// left out.
pub(crate) fn offer_addresses(sdp: &str) -> Vec<IpAddr> {
    let mut ips = Vec::new();
    for line in sdp.lines() {
        let Some(attribute) = line.trim().strip_prefix("a=") else { continue };
        if !attribute.starts_with("candidate:") {
            continue;
        }
        let Ok(candidate) = Candidate::from_sdp_string(attribute) else { continue };
        let ip = candidate.addr().ip();
        if !ips.contains(&ip) {
            ips.push(ip);
            if ips.len() == MAX_OFFER_ADDRESSES {
                break;
            }
        }
    }
    ips
}

/// The host candidates for a socket bound to `bound`: the bound address
/// itself when it is a specific IP; every interface address of the same
/// family (loopback last) when it is a wildcard; then the configured extras.
/// IPv4 comes first, duplicates are removed, at most [`MAX_CANDIDATES`].
pub(crate) fn host_candidates(bound: SocketAddr, extra: &[String]) -> Vec<SocketAddr> {
    let mut found = Vec::new();
    if bound.ip().is_unspecified() {
        let mut interfaces: Vec<IpAddr> = match if_addrs::get_if_addrs() {
            Ok(list) => list.into_iter().map(|i| i.ip()).collect(),
            Err(error) => {
                warn!(%error, "cannot list the network interfaces; advertising loopback only");
                Vec::new()
            }
        };
        interfaces.retain(|ip| ip.is_ipv4() == bound.is_ipv4() && usable(*ip) && !ip.is_loopback());
        found.extend(interfaces.into_iter().map(|ip| SocketAddr::new(ip, bound.port())));
        let loopback = if bound.is_ipv4() { IpAddr::V4(Ipv4Addr::LOCALHOST) } else { IpAddr::V6(Ipv6Addr::LOCALHOST) };
        found.push(SocketAddr::new(loopback, bound.port()));
    } else {
        found.push(bound);
    }
    for raw in extra {
        match parse_extra(raw, bound.port()) {
            Some(addr) if usable(addr.ip()) => found.push(addr),
            _ => warn!(candidate = %raw.chars().take(64).collect::<String>(), "ignoring an unusable candidate"),
        }
    }
    let mut unique: Vec<SocketAddr> = Vec::with_capacity(found.len());
    for addr in found {
        if !unique.contains(&addr) {
            unique.push(addr);
        }
    }
    unique.sort_by_key(|a| a.is_ipv6());
    unique.truncate(MAX_CANDIDATES);
    unique
}

/// `ip` or `ip:port` (`[v6]:port` too).
fn parse_extra(raw: &str, port: u16) -> Option<SocketAddr> {
    let raw = raw.trim();
    if let Ok(addr) = raw.parse::<SocketAddr>() {
        return Some(addr);
    }
    raw.trim_start_matches('[').trim_end_matches(']').parse::<IpAddr>().ok().map(|ip| SocketAddr::new(ip, port))
}

/// What ICE accepts as a host candidate.
fn usable(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v) => !(v.is_unspecified() || v.is_link_local() || v.is_broadcast() || v.is_multicast()),
        IpAddr::V6(v) => !(v.is_unspecified() || v.is_multicast() || v.is_unicast_link_local()),
    }
}

/// The local address a datagram from `source` is taken to have arrived on.
///
/// A socket bound to a specific address knows it. A wildcard socket does not
/// (that needs `IP_PKTINFO`), and ICE drops a request that did not arrive on
/// one of its candidates: the datagram is then attributed to the loopback
/// candidate for a loopback peer, else to the first other candidate of the
/// peer's family on the socket's port. Replies leave by the OS's route, so the
/// attribution only has to name *a* candidate the peer can reach.
pub(crate) fn arrival_address(bound: SocketAddr, candidates: &[SocketAddr], source: SocketAddr) -> SocketAddr {
    if !bound.ip().is_unspecified() {
        return bound;
    }
    let same_family = |c: &&SocketAddr| c.is_ipv4() == source.is_ipv4() && c.port() == bound.port();
    let pick = if source.ip().is_loopback() {
        candidates.iter().filter(same_family).find(|c| c.ip().is_loopback())
    } else {
        candidates.iter().filter(same_family).find(|c| !c.ip().is_loopback())
    };
    pick.or_else(|| candidates.iter().find(same_family)).copied().unwrap_or(bound)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn addr(s: &str) -> SocketAddr {
        s.parse().unwrap()
    }

    #[test]
    fn specific_bind_advertises_itself_and_extras() {
        let list = host_candidates(addr("127.0.0.1:5000"), &["192.168.1.42".into(), "203.0.113.9:40000".into()]);
        assert_eq!(list, vec![addr("127.0.0.1:5000"), addr("192.168.1.42:5000"), addr("203.0.113.9:40000")]);
    }

    #[test]
    fn wildcard_bind_lists_interfaces_with_loopback() {
        let list = host_candidates(addr("0.0.0.0:5000"), &[]);
        assert!(list.contains(&addr("127.0.0.1:5000")));
        assert!(list.iter().all(|a| a.is_ipv4() && a.port() == 5000 && !a.ip().is_unspecified()));
    }

    #[test]
    fn ipv4_first_duplicates_and_junk_removed() {
        let extra: Vec<String> = ["[2001:db8::1]:7000", "10.0.0.1", "10.0.0.1", "nonsense", "0.0.0.0", "169.254.1.1"]
            .map(String::from)
            .into();
        let list = host_candidates(addr("127.0.0.1:5000"), &extra);
        assert_eq!(list, vec![addr("127.0.0.1:5000"), addr("10.0.0.1:5000"), addr("[2001:db8::1]:7000")]);
    }

    #[test]
    fn candidate_count_is_bounded() {
        let extra: Vec<String> = (1..=40).map(|i| format!("10.0.0.{i}")).collect();
        assert_eq!(host_candidates(addr("127.0.0.1:5000"), &extra).len(), MAX_CANDIDATES);
    }

    #[test]
    fn local_candidates_add_tcp_reflexive_and_relayed() {
        let host = vec![addr("192.168.1.5:8555"), addr("127.0.0.1:8555")];
        let mapped = [addr("198.51.100.9:40001"), addr("192.168.1.5:8555")];
        let local = LocalCandidates::gather(host.clone(), Some(8555), &mapped, vec![addr("203.0.113.7:50000")]);
        assert_eq!(local.tcp, host, "every host address at the TCP port");
        // A mapping equal to a host candidate adds nothing (no NAT in between).
        assert_eq!(local.reflexive, vec![(addr("198.51.100.9:40001"), addr("192.168.1.5:8555"))]);
        assert!(!local.host_only());
        let plain = LocalCandidates::gather(host, None, &[], Vec::new());
        assert!(plain.host_only() && plain.tcp.is_empty());
    }

    #[test]
    fn an_offers_candidate_addresses_are_read_and_bounded() {
        let sdp = [
            "v=0",
            "a=candidate:1 1 udp 2122260223 192.168.1.20 50000 typ host",
            "a=candidate:2 1 udp 1686052607 198.51.100.3 50001 typ srflx raddr 192.168.1.20 rport 50000",
            "a=candidate:3 1 udp 2122260223 0a1b2c3d-1234.local 50000 typ host",
            "a=candidate:4 1 tcp 1518280447 192.168.1.20 9 typ host tcptype active",
            "a=mid:0",
        ]
        .join("\r\n");
        let ips: Vec<IpAddr> = ["192.168.1.20", "198.51.100.3"].map(|ip| ip.parse().unwrap()).into();
        assert_eq!(offer_addresses(&sdp), ips);
        let many: String = (1..=40).map(|i| format!("a=candidate:{i} 1 udp 1 10.0.0.{i} 5000 typ host\r\n")).collect();
        assert_eq!(offer_addresses(&many).len(), MAX_OFFER_ADDRESSES);
    }

    #[test]
    fn arrival_address_attribution() {
        let cands = [addr("192.168.1.5:5000"), addr("127.0.0.1:5000"), addr("203.0.113.9:40000")];
        let wild = addr("0.0.0.0:5000");
        assert_eq!(arrival_address(wild, &cands, addr("127.0.0.1:9")), addr("127.0.0.1:5000"));
        assert_eq!(arrival_address(wild, &cands, addr("192.168.1.77:9")), addr("192.168.1.5:5000"));
        assert_eq!(arrival_address(addr("10.1.1.1:5000"), &cands, addr("8.8.8.8:9")), addr("10.1.1.1:5000"));
        assert_eq!(arrival_address(wild, &[], addr("8.8.8.8:9")), wild);
    }
}
