use std::net::SocketAddr;
use std::sync::atomic::Ordering;
use std::time::Instant;

use str0m::RtcConfig;

use crate::registry::MAX_ADDRS_PER_SESSION;

use super::*;

/// A session the registry can route to, with no peer behind it.
fn session(id: u64) -> Arc<Session> {
    let rtc = RtcConfig::new().set_ice_lite(true).build(Instant::now());
    Arc::new(Session::new(id, rtc, CancellationToken::new(), Vec::new(), Vec::new()))
}

#[tokio::test]
async fn addresses_per_session_are_bounded() {
    let config = WebRtcConfig { listen: "127.0.0.1:0".parse().unwrap(), ..WebRtcConfig::default() };
    let server = WebRtcServer::bind(config, CancellationToken::new()).await.unwrap();
    let shared = &server.shared;
    let reservation = shared.reserve(None).unwrap();
    shared.register(reservation, session(7));
    let route = |port| (Via::Udp, SocketAddr::from(([127, 0, 0, 1], port)));
    for port in 1..=20u16 {
        shared.remember(route(port), 7);
    }
    assert_eq!(shared.addrs_of(7), MAX_ADDRS_PER_SESSION);
    // The newest routes are kept: the one the peer uses now always leads.
    let registry = shared.registry();
    assert_eq!(registry.route_owner(&route(20)), Some(7));
    assert_eq!(registry.route_owner(&route(13)), Some(7));
    assert_eq!(registry.route_owner(&route(12)), None);
    drop(registry);
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
    shared.register(c, session(9));
    assert!(shared.reserve(local(5)).is_err());
    shared.unregister(9);
    assert!(shared.reserve(local(6)).is_ok());
    drop(lan);
}

/// STUN's `PRIORITY`, which str0m requires of a binding request.
const PRIORITY: u16 = 0x0024;

/// A server with sessions `1..=count` registered.
async fn server_with(count: u64) -> (WebRtcServer, Vec<Arc<Session>>) {
    let config = WebRtcConfig { listen: "127.0.0.1:0".parse().unwrap(), ..WebRtcConfig::default() };
    let server = WebRtcServer::bind(config, CancellationToken::new()).await.unwrap();
    let sessions: Vec<Arc<Session>> = (1..=count).map(session).collect();
    for s in &sessions {
        server.shared.register(server.shared.reserve(None).unwrap(), s.clone());
    }
    (server, sessions)
}

/// The datagrams the sessions' `Rtc`s were handed to parse.
fn parsed(sessions: &[Arc<Session>]) -> Vec<usize> {
    sessions.iter().map(|s| s.offered.swap(0, Ordering::Relaxed)).collect()
}

/// A binding request to `ufrag`, signed with `password` when one is given.
fn binding_request(ufrag: &str, password: Option<&str>) -> Vec<u8> {
    use crate::stun::{Class, Encoder, attr, method};
    let hmac = str0m::crypto::from_feature_flags().sha1_hmac_provider;
    Encoder::new(Class::Request, method::BINDING, [9; 12])
        .attr(attr::USERNAME, format!("{ufrag}:peer").as_bytes())
        .u32(PRIORITY, 1_853_817_087)
        .finish(password.map(|p| (p.as_bytes(), hmac)), true)
}

fn password_of(session: &Session) -> String {
    session.peer().rtc.direct_api().local_ice_credentials().pass
}

const DTLS: [u8; 16] = [0x16, 0xfe, 0xfd, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 3, 1, 2];

#[tokio::test]
async fn an_unknown_datagram_costs_one_parse_whatever_the_session_count() {
    let stranger = SocketAddr::from(([203, 0, 113, 9], 40_000));
    for count in [1, 8, 48] {
        let (server, sessions) = server_with(count).await;
        let shared = &server.shared;
        // Media from an address no session knows: no session parses it
        // (each of them did, under its own lock).
        assert_eq!(shared.dispatch(&DTLS, stranger, Via::Udp), None);
        assert_eq!(parsed(&sessions).iter().sum::<usize>(), 0, "{count} sessions, media");
        // A binding request to a fragment no session has: none either.
        assert_eq!(shared.dispatch(&binding_request("nobodyhasthis", None), stranger, Via::Udp), None);
        assert_eq!(parsed(&sessions).iter().sum::<usize>(), 0, "{count} sessions, unknown fragment");
        // One naming the last session, unsigned: that session alone parses
        // it, and refuses it.
        let last = sessions.last().unwrap();
        assert_eq!(shared.dispatch(&binding_request(&last.ufrag, None), stranger, Via::Udp), None);
        let counts = parsed(&sessions);
        assert_eq!(counts.iter().sum::<usize>(), 1, "{count} sessions, named fragment: {counts:?}");
        assert_eq!(counts.last(), Some(&1));
        assert_eq!(shared.registry().route_owner(&(Via::Udp, stranger)), None, "a refused request teaches nothing");
    }
}

#[tokio::test]
async fn a_binding_request_reaches_its_session_and_teaches_it_the_route() {
    let (server, sessions) = server_with(3).await;
    let shared = &server.shared;
    let peer = SocketAddr::from(([192, 168, 1, 30], 50_000));
    let second = &sessions[1];
    let request = binding_request(&second.ufrag, Some(&password_of(second)));
    assert_eq!(shared.dispatch(&request, peer, Via::Udp), Some(2));
    assert_eq!(parsed(&sessions), [0, 1, 0]);
    assert_eq!(shared.registry().route_owner(&(Via::Udp, peer)), Some(2));
    // What follows on that route goes to that session alone.
    shared.dispatch(&DTLS, peer, Via::Udp);
    assert_eq!(parsed(&sessions), [0, 1, 0]);
    // The same address through a relay is another route.
    assert_eq!(shared.dispatch(&DTLS, peer, Via::Relay(SocketAddr::from(([198, 51, 100, 1], 3478)))), None);
    assert_eq!(parsed(&sessions), [0, 0, 0]);
}

#[tokio::test]
async fn a_route_another_session_had_is_taken_by_the_one_it_now_reaches() {
    let (server, sessions) = server_with(2).await;
    let shared = &server.shared;
    let peer = SocketAddr::from(([192, 168, 1, 31], 50_001));
    shared.remember((Via::Udp, peer), 1);
    let second = &sessions[1];
    let request = binding_request(&second.ufrag, Some(&password_of(second)));
    // Offered to the route's session first, which refuses it, then to the
    // session it names.
    assert_eq!(shared.dispatch(&request, peer, Via::Udp), Some(2));
    assert_eq!(parsed(&sessions), [1, 1]);
    assert_eq!(shared.registry().route_owner(&(Via::Udp, peer)), Some(2));
    assert_eq!(shared.addrs_of(1), 0);
    assert_eq!(shared.addrs_of(2), 1);
    // Gone with its session.
    shared.unregister(2);
    assert_eq!(shared.registry().route_owner(&(Via::Udp, peer)), None);
    assert_eq!(shared.dispatch(&request, peer, Via::Udp), None);
}
