//! go2rtc's listen addresses: `":8555"` (every interface), `"127.0.0.1:1984"`,
//! `"[::1]:8554"`, `"localhost:1984"`.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};

/// Parses the `listen` value of section `key` (`api.listen`...). An empty
/// value, which disables a listener in go2rtc, is refused: every listener is
/// needed here (the transcoders publish into the RTSP server, openIVR only
/// talks to the API, WebRTC is the viewers' path).
pub fn parse(key: &str, value: &str) -> Result<SocketAddr, String> {
    let value = value.trim();
    let bad = || format!("{key}: {value:?} is not an address (\":port\", \"ip:port\" or \"localhost:port\")");
    if value.is_empty() {
        return Err(format!("{key} is empty; oxrtc needs this listener"));
    }
    if let Ok(addr) = value.parse::<SocketAddr>() {
        return Ok(addr);
    }
    let (host, port) = value.rsplit_once(':').ok_or_else(bad)?;
    let port: u16 = port.parse().map_err(|_| bad())?;
    let ip = match host {
        "" => IpAddr::V4(Ipv4Addr::UNSPECIFIED),
        h if h.eq_ignore_ascii_case("localhost") => IpAddr::V4(Ipv4Addr::LOCALHOST),
        _ => return Err(bad()),
    };
    Ok(SocketAddr::new(ip, port))
}

/// The address a local client reaches `bound` at: the loopback of its family
/// when it is unspecified.
pub fn reachable(bound: SocketAddr) -> SocketAddr {
    let ip = match bound.ip() {
        IpAddr::V4(ip) if ip.is_unspecified() => IpAddr::V4(Ipv4Addr::LOCALHOST),
        IpAddr::V6(ip) if ip.is_unspecified() => IpAddr::V6(std::net::Ipv6Addr::LOCALHOST),
        ip => ip,
    };
    SocketAddr::new(ip, bound.port())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn go2rtc_spellings() {
        assert_eq!(parse("webrtc.listen", ":8555"), Ok(SocketAddr::from(([0, 0, 0, 0], 8555))));
        assert_eq!(parse("api.listen", "127.0.0.1:1984"), Ok(SocketAddr::from(([127, 0, 0, 1], 1984))));
        assert_eq!(parse("api.listen", " localhost:0 "), Ok(SocketAddr::from(([127, 0, 0, 1], 0))));
        assert_eq!(parse("rtsp.listen", "[::1]:8554"), Ok("[::1]:8554".parse().unwrap()));
    }

    #[test]
    fn refusals_name_the_key() {
        for bad in ["", "8554", "host:1", ":99999", ":x", "1.2.3.4", "[::1]"] {
            let error = parse("rtsp.listen", bad).unwrap_err();
            assert!(error.starts_with("rtsp.listen"), "{bad:?}: {error}");
        }
    }

    #[test]
    fn unspecified_is_reached_on_loopback() {
        assert_eq!(reachable(SocketAddr::from(([0, 0, 0, 0], 5))), SocketAddr::from(([127, 0, 0, 1], 5)));
        assert_eq!(reachable("[::]:5".parse().unwrap()), "[::1]:5".parse().unwrap());
        assert_eq!(reachable(SocketAddr::from(([10, 0, 0, 2], 5))), SocketAddr::from(([10, 0, 0, 2], 5)));
    }
}
