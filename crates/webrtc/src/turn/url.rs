//! `stun:` and `turn:` URLs (RFC 7064, RFC 7065) as `webrtc.ice_servers`
//! lists them.

use std::fmt;
use std::net::{IpAddr, Ipv6Addr};

/// Longest URL read; the rest is refused before any parsing.
const MAX_URL: usize = 300;
/// Longest DNS name (RFC 1035).
const MAX_HOST: usize = 253;
pub(crate) const DEFAULT_PORT: u16 = 3478;

/// A server this client can use.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ServerUrl {
    pub(crate) kind: Kind,
    /// How a TURN server is reached (a STUN server always over UDP).
    pub(crate) transport: Transport,
    /// A DNS name or an IP literal (without brackets).
    pub(crate) host: String,
    pub(crate) port: u16,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Kind {
    /// A STUN server: a server-reflexive candidate.
    Stun,
    /// A TURN server: a relayed candidate.
    Turn,
}

/// The transport between this client and a TURN server; the relayed side is
/// UDP either way.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Transport {
    Udp,
    /// `?transport=tcp`: one TCP connection to the server (RFC 8656
    /// section 3.1), for a network that lets no UDP through.
    Tcp,
}

impl fmt::Display for ServerUrl {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let scheme = match self.kind {
            Kind::Stun => "stun",
            Kind::Turn => "turn",
        };
        if self.host.contains(':') {
            write!(f, "{scheme}:[{}]:{}", self.host, self.port)?;
        } else {
            write!(f, "{scheme}:{}:{}", self.host, self.port)?;
        }
        match self.transport {
            Transport::Udp => Ok(()),
            Transport::Tcp => f.write_str("?transport=tcp"),
        }
    }
}

/// Why a URL is not used.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub(crate) enum UrlError {
    #[error("not a stun:/turn: URL")]
    Malformed,
    /// `stuns:` and `turns:` need TLS (or DTLS) to the server.
    #[error("{0}: TLS to the server is not supported")]
    Secure(&'static str),
}

/// Parses one URL of an ICE server entry.
pub(crate) fn parse(url: &str) -> Result<ServerUrl, UrlError> {
    if url.len() > MAX_URL || url.chars().any(|c| c.is_control() || c.is_whitespace()) {
        return Err(UrlError::Malformed);
    }
    let (scheme, rest) = url.split_once(':').ok_or(UrlError::Malformed)?;
    let kind = match scheme.to_ascii_lowercase().as_str() {
        "stun" => Kind::Stun,
        "turn" => Kind::Turn,
        "stuns" => return Err(UrlError::Secure("stuns")),
        "turns" => return Err(UrlError::Secure("turns")),
        _ => return Err(UrlError::Malformed),
    };
    let (authority, query) = match rest.split_once('?') {
        Some((authority, query)) => (authority, Some(query)),
        None => (rest, None),
    };
    let transport = match (kind, query) {
        (_, None) => Transport::Udp,
        (Kind::Turn, Some(query)) => match query.to_ascii_lowercase().as_str() {
            "transport=udp" => Transport::Udp,
            "transport=tcp" => Transport::Tcp,
            _ => return Err(UrlError::Malformed),
        },
        // RFC 7064: a stun: URI has no query.
        (Kind::Stun, Some(_)) => return Err(UrlError::Malformed),
    };
    let (host, port) = split_host_port(authority)?;
    Ok(ServerUrl { kind, transport, host, port })
}

/// `host`, `host:port`, `[v6]`, `[v6]:port`. No user information (RFC 7065
/// keeps credentials out of the URL) and no path.
fn split_host_port(authority: &str) -> Result<(String, u16), UrlError> {
    let (host, port) = if let Some(bracketed) = authority.strip_prefix('[') {
        let (inside, after) = bracketed.split_once(']').ok_or(UrlError::Malformed)?;
        let ip: Ipv6Addr = inside.parse().map_err(|_| UrlError::Malformed)?;
        let port = match after {
            "" => None,
            after => Some(after.strip_prefix(':').ok_or(UrlError::Malformed)?),
        };
        (ip.to_string(), port)
    } else {
        let (host, port) = match authority.rsplit_once(':') {
            Some((host, port)) => (host, Some(port)),
            None => (authority, None),
        };
        if !valid_host(host) {
            return Err(UrlError::Malformed);
        }
        (host.to_ascii_lowercase(), port)
    };
    let port = match port {
        None => DEFAULT_PORT,
        Some(port) => match port.parse::<u16>() {
            Ok(number) if number != 0 && port.bytes().all(|b| b.is_ascii_digit()) => number,
            _ => return Err(UrlError::Malformed),
        },
    };
    Ok((host, port))
}

/// An IPv4 literal or a DNS name of letters, digits, hyphens and dots.
fn valid_host(host: &str) -> bool {
    if host.parse::<IpAddr>().is_ok_and(|ip| ip.is_ipv4()) {
        return true;
    }
    !host.is_empty()
        && host.len() <= MAX_HOST
        && host.split('.').all(|label| {
            !label.is_empty()
                && label.len() <= 63
                && !label.starts_with('-')
                && !label.ends_with('-')
                && label.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
        })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    fn ok(url: &str) -> (Kind, String, u16) {
        let parsed = parse(url).unwrap();
        (parsed.kind, parsed.host, parsed.port)
    }

    #[test]
    fn the_urls_openivr_writes() {
        // openIVR-cloud's defaults: internal/config/config.go.
        assert_eq!(ok("stun:relay.example.org:3478"), (Kind::Stun, "relay.example.org".into(), 3478));
        assert_eq!(ok("turn:relay.example.org:3478?transport=udp"), (Kind::Turn, "relay.example.org".into(), 3478));
        let tcp = parse("turn:relay.example.org:3478?transport=tcp").unwrap();
        assert_eq!((tcp.kind, tcp.transport, tcp.port), (Kind::Turn, Transport::Tcp, 3478));
        assert_eq!(tcp.to_string(), "turn:relay.example.org:3478?transport=tcp");
        assert_eq!(parse("turn:relay.example.org:3478?TRANSPORT=TCP"), Ok(tcp), "the query is not case sensitive");
        assert_ne!(
            parse("turn:relay.example.org:3478?transport=udp"),
            parse("turn:relay.example.org:3478?transport=tcp")
        );
        assert_eq!(parse("turns:relay.example.org:5349"), Err(UrlError::Secure("turns")));
    }

    #[test]
    fn hosts_ports_and_defaults() {
        assert_eq!(ok("turn:192.0.2.7"), (Kind::Turn, "192.0.2.7".into(), DEFAULT_PORT));
        assert_eq!(ok("TURN:Relay.Example:1234"), (Kind::Turn, "relay.example".into(), 1234));
        assert_eq!(ok("stun:[2001:db8::1]:19302"), (Kind::Stun, "2001:db8::1".into(), 19302));
        assert_eq!(ok("stun:[::1]"), (Kind::Stun, "::1".into(), DEFAULT_PORT));
        assert_eq!(parse("stun:[::1]:3478").unwrap().to_string(), "stun:[::1]:3478");
    }

    #[test]
    fn malformed_urls_are_refused() {
        for url in [
            "",
            "turn:",
            "turn::3478",
            "turn:host:0",
            "turn:host:65536",
            "turn:host:+5",
            "turn:user:pass@host",
            "turn:host/path",
            "turn:host?transport=sctp",
            "stun:host?transport=udp",
            "turn:-host",
            "turn:ho st",
            "turn:[::1",
            "turn:[nonsense]:1",
            "turn:2001:db8::1",
            "http://host",
            "turn:host\n",
        ] {
            assert_eq!(parse(url), Err(UrlError::Malformed), "{url:?}");
        }
        assert_eq!(parse(&format!("turn:{}", "a".repeat(400))), Err(UrlError::Malformed));
    }
}
