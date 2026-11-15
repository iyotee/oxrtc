#![allow(clippy::unwrap_used, clippy::indexing_slicing, clippy::panic)]

use super::*;
use crate::stun::{TransactionId, long_term_key};

const REALM: &str = "relay.test";
const NONCE: &str = "nonce-1";

fn hmac() -> Hmac {
    str0m::crypto::from_feature_flags().sha1_hmac_provider
}

fn credentials() -> Credentials {
    Credentials { username: "1700000000:site".into(), password: "secret==".into() }
}

fn key() -> [u8; 16] {
    long_term_key("1700000000:site", REALM, "secret==")
}

fn relayed() -> SocketAddr {
    "203.0.113.5:50000".parse().unwrap()
}

fn peer() -> SocketAddr {
    "198.51.100.7:61000".parse().unwrap()
}

/// Every datagram the allocation has to send, parsed back with its bytes.
fn sent(allocation: &mut Allocation) -> Vec<Vec<u8>> {
    std::iter::from_fn(|| allocation.poll_transmit()).collect()
}

fn tid(datagram: &[u8]) -> TransactionId {
    Message::parse(datagram).unwrap().transaction
}

fn answer(class: Class, method: u16, request: &[u8]) -> Encoder {
    Encoder::new(class, method, tid(request))
}

fn challenge(request: &[u8]) -> Vec<u8> {
    answer(Class::Error, method::ALLOCATE, request)
        .error(401, "Unauthorized")
        .attr(attr::REALM, REALM.as_bytes())
        .attr(attr::NONCE, NONCE.as_bytes())
        .finish(None, true)
}

fn allocated_answer(request: &[u8], lifetime: u32) -> Vec<u8> {
    answer(Class::Success, method::ALLOCATE, request)
        .xor_address(attr::XOR_RELAYED_ADDRESS, relayed())
        .xor_address(attr::XOR_MAPPED_ADDRESS, "198.51.100.1:3478".parse().unwrap())
        .u32(attr::LIFETIME, lifetime)
        .finish(Some((&key(), hmac())), true)
}

/// An allocation through the challenge, allocated at `now`.
fn allocated(now: Instant) -> Allocation {
    let mut allocation = Allocation::new(credentials(), hmac(), now);
    let first = sent(&mut allocation).remove(0);
    assert_eq!(allocation.handle(now, &challenge(&first)), Incoming::Control);
    let signed = sent(&mut allocation).remove(0);
    assert_eq!(allocation.handle(now, &allocated_answer(&signed, 600)), Incoming::Control);
    allocation
}

#[test]
fn allocates_after_the_challenge_with_the_long_term_key() {
    let now = Instant::now();
    let mut allocation = Allocation::new(credentials(), hmac(), now);
    let first = sent(&mut allocation);
    assert_eq!(first.len(), 1);
    let request = Message::parse(&first[0]).unwrap();
    assert_eq!((request.class, request.method), (Class::Request, method::ALLOCATE));
    assert_eq!(request.get(attr::REQUESTED_TRANSPORT), Some(&[17u8, 0, 0, 0][..]));
    assert!(request.get(attr::USERNAME).is_none(), "the first request is unsigned");
    assert!(request.fingerprint_ok() && request.has_fingerprint());

    allocation.handle(now, &challenge(&first[0]));
    let signed = sent(&mut allocation).remove(0);
    let request = Message::parse(&signed).unwrap();
    assert_eq!(request.text(attr::USERNAME, 64), Some("1700000000:site"));
    assert_eq!(request.text(attr::REALM, 64), Some(REALM));
    assert_eq!(request.text(attr::NONCE, 64), Some(NONCE));
    assert!(request.verify_integrity(&key(), hmac()), "signed with MD5(user:realm:password)");

    allocation.handle(now, &allocated_answer(&signed, 600));
    assert_eq!(allocation.relayed(), Some(relayed()));
    assert!(
        matches!(allocation.poll_event(), Some(Event::Allocated { lifetime, .. }) if lifetime == Duration::from_secs(600))
    );
}

#[test]
fn a_second_401_is_refused_credentials() {
    let now = Instant::now();
    let mut allocation = Allocation::new(credentials(), hmac(), now);
    let first = sent(&mut allocation).remove(0);
    allocation.handle(now, &challenge(&first));
    let signed = sent(&mut allocation).remove(0);
    allocation.handle(now, &challenge(&signed));
    assert_eq!(allocation.poll_event(), Some(Event::Failed(Failure::Refused)));
    assert!(!allocation.is_alive());
    assert!(sent(&mut allocation).is_empty(), "no retry with the same credentials");
    assert!(!format!("{:?} {}", credentials(), Failure::Refused).contains("secret"));
}

#[test]
fn a_stale_nonce_is_followed_a_few_times() {
    let now = Instant::now();
    let mut allocation = Allocation::new(credentials(), hmac(), now);
    let first = sent(&mut allocation).remove(0);
    allocation.handle(now, &challenge(&first));
    let mut request = sent(&mut allocation).remove(0);
    for round in 0..MAX_STALE {
        let nonce = format!("nonce-{}", round + 2);
        let stale = answer(Class::Error, method::ALLOCATE, &request)
            .error(438, "Stale Nonce")
            .attr(attr::REALM, REALM.as_bytes())
            .attr(attr::NONCE, nonce.as_bytes())
            .finish(None, true);
        allocation.handle(now, &stale);
        request = sent(&mut allocation).remove(0);
        assert_eq!(Message::parse(&request).unwrap().text(attr::NONCE, 64), Some(nonce.as_str()));
    }
    let stale = answer(Class::Error, method::ALLOCATE, &request).error(438, "Stale Nonce").finish(None, true);
    allocation.handle(now, &stale);
    assert_eq!(allocation.poll_event(), Some(Event::Failed(Failure::Error(438, "Stale Nonce".into()))));
}

#[test]
fn an_answer_without_the_key_is_not_the_servers() {
    let now = Instant::now();
    let mut allocation = Allocation::new(credentials(), hmac(), now);
    let first = sent(&mut allocation).remove(0);
    allocation.handle(now, &challenge(&first));
    let signed = sent(&mut allocation).remove(0);
    let forged = answer(Class::Success, method::ALLOCATE, &signed)
        .xor_address(attr::XOR_RELAYED_ADDRESS, "192.0.2.66:1".parse().unwrap())
        .finish(Some((b"wrong key", hmac())), true);
    allocation.handle(now, &forged);
    assert_eq!(allocation.relayed(), None);
    assert_eq!(allocation.poll_event(), Some(Event::Failed(Failure::NoAnswer)));
    // An answer to no request is dropped.
    let stray = Encoder::new(Class::Success, method::ALLOCATE, [9; 12]).finish(None, true);
    assert_eq!(allocated(now).handle(now, &stray), Incoming::Ignored);
}

#[test]
fn a_silent_server_fails_the_allocation() {
    let start = Instant::now();
    let mut allocation = Allocation::new(credentials(), hmac(), start);
    let mut sends = sent(&mut allocation).len();
    while let Some(at) = allocation.poll_timeout() {
        allocation.handle_timeout(at);
        sends += sent(&mut allocation).len();
    }
    assert_eq!(sends, 6);
    assert_eq!(allocation.poll_event(), Some(Event::Failed(Failure::NoAnswer)));
}

#[test]
fn the_allocation_is_refreshed_before_it_lapses() {
    let start = Instant::now();
    let mut allocation = allocated(start);
    allocation.poll_event();
    let at = allocation.poll_timeout().unwrap();
    assert_eq!(at, start + Duration::from_secs(540));
    allocation.handle_timeout(at);
    let refresh = sent(&mut allocation).remove(0);
    let message = Message::parse(&refresh).unwrap();
    assert_eq!(message.method, method::REFRESH);
    assert_eq!(message.u32(attr::LIFETIME), Some(600));
    assert!(message.verify_integrity(&key(), hmac()));
    let ok =
        answer(Class::Success, method::REFRESH, &refresh).u32(attr::LIFETIME, 600).finish(Some((&key(), hmac())), true);
    allocation.handle(at, &ok);
    assert_eq!(allocation.poll_timeout(), Some(at + Duration::from_secs(540)));
    // A refused refresh loses the allocation.
    allocation.handle_timeout(at + Duration::from_secs(540));
    let refresh = sent(&mut allocation).remove(0);
    let mismatch = answer(Class::Error, method::REFRESH, &refresh).error(437, "Allocation Mismatch").finish(None, true);
    allocation.handle(at, &mismatch);
    assert_eq!(allocation.poll_event(), Some(Event::Failed(Failure::Error(437, "Allocation Mismatch".into()))));
}

#[test]
fn sending_asks_for_a_permission_and_a_channel_then_uses_the_channel() {
    let now = Instant::now();
    let mut allocation = allocated(now);
    let first = allocation.send(now, peer(), b"media").unwrap();
    let indication = Message::parse(&first).unwrap();
    assert_eq!((indication.class, indication.method), (Class::Indication, method::SEND));
    assert_eq!(indication.xor_address(attr::XOR_PEER_ADDRESS), Some(peer()));
    assert_eq!(indication.get(attr::DATA), Some(&b"media"[..]));
    let requests = sent(&mut allocation);
    let methods: Vec<u16> = requests.iter().map(|r| Message::parse(r).unwrap().method).collect();
    assert_eq!(methods, [method::CREATE_PERMISSION, method::CHANNEL_BIND]);
    let bind = Message::parse(&requests[1]).unwrap();
    let number = bind.channel_number().unwrap();
    assert_eq!(bind.xor_address(attr::XOR_PEER_ADDRESS), Some(peer()));
    // A second packet before the binding is confirmed: still an indication, no new request.
    assert!(Message::parse(&allocation.send(now, peer(), b"more").unwrap()).is_ok());
    assert!(sent(&mut allocation).is_empty());
    for request in &requests {
        let method = Message::parse(request).unwrap().method;
        allocation.handle(now, &answer(Class::Success, method, request).finish(Some((&key(), hmac())), true));
    }
    let channel = allocation.send(now, peer(), b"media").unwrap();
    assert_eq!(ChannelData::parse(&channel), Some(ChannelData { channel: number, data: b"media" }));
    // What the peer sends back, both ways.
    let data = Encoder::new(Class::Indication, method::DATA, [5; 12])
        .xor_address(attr::XOR_PEER_ADDRESS, peer())
        .attr(attr::DATA, b"reply")
        .finish(None, false);
    assert_eq!(allocation.handle(now, &data), Incoming::Relayed { peer: peer(), data: b"reply" });
    let through_channel = channel_data(number, b"reply").unwrap();
    assert_eq!(allocation.handle(now, &through_channel), Incoming::Relayed { peer: peer(), data: b"reply" });
    assert_eq!(allocation.handle(now, &channel_data(number + 1, b"x").unwrap()), Incoming::Ignored);
}

#[test]
fn a_refused_permission_is_that_peers_only() {
    let now = Instant::now();
    let mut allocation = allocated(now);
    allocation.permit(now, "10.0.0.9".parse().unwrap());
    let request = sent(&mut allocation).remove(0);
    let forbidden =
        answer(Class::Error, method::CREATE_PERMISSION, &request).error(403, "Forbidden").finish(None, true);
    allocation.handle(now, &forbidden);
    assert!(allocation.is_alive());
    assert_eq!(allocation.poll_event().map(|e| matches!(e, Event::Allocated { .. })), Some(true));
    assert_eq!(allocation.poll_event(), None);
    allocation.permit(now, "10.0.0.9".parse().unwrap());
    assert!(sent(&mut allocation).is_empty(), "not asked again at once");
}

#[test]
fn nothing_is_relayed_before_the_allocation_and_close_releases_it() {
    let now = Instant::now();
    let mut allocation = Allocation::new(credentials(), hmac(), now);
    assert_eq!(allocation.send(now, peer(), b"x"), None);
    let data = Encoder::new(Class::Indication, method::DATA, [5; 12])
        .xor_address(attr::XOR_PEER_ADDRESS, peer())
        .attr(attr::DATA, b"x")
        .finish(None, false);
    assert_eq!(allocation.handle(now, &data), Incoming::Ignored);
    let mut allocation = allocated(now);
    allocation.close(now);
    let release = Message::parse(&sent(&mut allocation).remove(0)).unwrap().u32(attr::LIFETIME);
    assert_eq!(release, Some(0));
    assert!(!allocation.is_alive());
}

#[test]
fn garbage_from_the_server_is_ignored() {
    let now = Instant::now();
    let mut allocation = allocated(now);
    for junk in [&b""[..], &[0x40], &[0x00, 0x01, 0x00], &[0x17, 0xfe, 0xfd, 0, 1], &[0xff; 64]] {
        assert_eq!(allocation.handle(now, junk), Incoming::Ignored);
    }
    assert!(allocation.is_alive());
}
