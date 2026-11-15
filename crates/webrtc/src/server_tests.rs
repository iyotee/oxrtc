use std::net::SocketAddr;
use std::sync::Mutex;

use str0m::RtcConfig;

use crate::registry::MAX_ADDRS_PER_SESSION;

use super::*;

#[tokio::test]
async fn addresses_per_session_are_bounded() {
    let config = WebRtcConfig { listen: "127.0.0.1:0".parse().unwrap(), ..WebRtcConfig::default() };
    let server = WebRtcServer::bind(config, CancellationToken::new()).await.unwrap();
    let shared = &server.shared;
    let rtc = RtcConfig::new().set_ice_lite(true).build(Instant::now());
    let session = Arc::new(Session {
        id: 7,
        peer: Mutex::new(Peer { rtc, last_rx: Instant::now() }),
        wake: Notify::new(),
        cancel: CancellationToken::new(),
        candidates: Vec::new(),
        tcp_candidates: Vec::new(),
    });
    let reservation = shared.reserve(None).unwrap();
    shared.register(reservation, session);
    for port in 1..=20u16 {
        shared.remember((Via::Udp, SocketAddr::from(([127, 0, 0, 1], port))), 7);
    }
    assert_eq!(shared.addrs_of(7), MAX_ADDRS_PER_SESSION);
    shared.unregister(7);
    assert_eq!(shared.addrs_of(7), 0);
    assert_eq!(server.session_count(), 0);
}

#[tokio::test]
async fn reservations_count_against_the_limit_and_are_released() {
    let config = WebRtcConfig { listen: "127.0.0.1:0".parse().unwrap(), max_sessions: 2, ..WebRtcConfig::default() };
    let server = WebRtcServer::bind(config, CancellationToken::new()).await.unwrap();
    let a = server.shared.reserve(None).unwrap();
    let _b = server.shared.reserve(None).unwrap();
    assert!(matches!(server.shared.reserve(None), Err(WhepError::TooManySessions)));
    drop(a);
    assert!(server.shared.reserve(None).is_ok());
}

#[tokio::test]
async fn each_address_has_its_own_share_of_the_sessions() {
    let config = WebRtcConfig {
        listen: "127.0.0.1:0".parse().unwrap(),
        max_sessions: 8,
        max_sessions_per_ip: 2,
        ..WebRtcConfig::default()
    };
    let server = WebRtcServer::bind(config, CancellationToken::new()).await.unwrap();
    let shared = &server.shared;
    let local = |port| Some(SocketAddr::from(([127, 0, 0, 1], port)));
    let a = shared.reserve(local(1)).unwrap();
    let _b = shared.reserve(local(2)).unwrap();
    assert!(matches!(shared.reserve(local(3)), Err(WhepError::TooManySessions)), "loopback is not exempt");
    let lan = shared.reserve(Some(SocketAddr::from(([192, 168, 1, 30], 4000)))).unwrap();
    // A refused address leaves the total untouched.
    assert_eq!(shared.registry().reserved, 3);
    drop(a);
    let c = shared.reserve(local(4)).unwrap();
    // A registered session keeps its address's share until it ends.
    let rtc = RtcConfig::new().set_ice_lite(true).build(Instant::now());
    let session = Arc::new(Session {
        id: 9,
        peer: Mutex::new(Peer { rtc, last_rx: Instant::now() }),
        wake: Notify::new(),
        cancel: CancellationToken::new(),
        candidates: Vec::new(),
        tcp_candidates: Vec::new(),
    });
    shared.register(c, session);
    assert!(shared.reserve(local(5)).is_err());
    shared.unregister(9);
    assert!(shared.reserve(local(6)).is_ok());
    drop(lan);
}
