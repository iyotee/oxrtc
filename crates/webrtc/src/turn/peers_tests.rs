#![allow(clippy::unwrap_used)]

use std::net::Ipv4Addr;

use super::*;

fn ip(n: u32) -> IpAddr {
    IpAddr::V4(Ipv4Addr::from(0x0A00_0000 + n))
}

fn peer(n: u16) -> SocketAddr {
    SocketAddr::new(ip(1), 40_000 + n)
}

#[test]
fn a_permission_is_asked_once_and_refreshed_while_used() {
    let start = Instant::now();
    let mut peers = Peers::default();
    assert!(peers.want_permission(ip(1), start));
    assert!(!peers.want_permission(ip(1), start), "already asked for");
    peers.permission_answered(ip(1), true, start);
    assert!(!peers.want_permission(ip(1), start), "installed");
    assert!(peers.permissions_due(start).is_empty());
    assert_eq!(peers.next_due(), Some(start + PERMISSION_LIFETIME), "unused since: its lapse");
    // Used after it was installed: refreshed ahead of the lapse.
    peers.want_permission(ip(1), start + Duration::from_secs(10));
    let refresh = peers.next_due().unwrap();
    assert_eq!(refresh, start + PERMISSION_LIFETIME - REFRESH_AHEAD);
    assert_eq!(peers.permissions_due(refresh), vec![ip(1)]);
    assert!(peers.permissions_due(refresh).is_empty(), "asked for once");
}

#[test]
fn an_unused_permission_lapses_and_is_forgotten() {
    let start = Instant::now();
    let mut peers = Peers::default();
    peers.want_permission(ip(1), start);
    peers.permission_answered(ip(1), true, start);
    // Never used since: no refresh, and the timer is its lapse, not a busy loop.
    let due = start + PERMISSION_LIFETIME - REFRESH_AHEAD;
    assert!(peers.permissions_due(due).is_empty());
    assert_eq!(peers.next_due(), Some(start + PERMISSION_LIFETIME));
    assert!(peers.permissions_due(start + PERMISSION_LIFETIME).is_empty());
    assert_eq!(peers.counts().0, 0);
    assert_eq!(peers.next_due(), None);
}

#[test]
fn a_refused_permission_waits_before_it_is_asked_again() {
    let start = Instant::now();
    let mut peers = Peers::default();
    peers.want_permission(ip(1), start);
    peers.permission_answered(ip(1), false, start);
    assert_eq!(peers.next_due(), None, "no timer for a refusal");
    assert!(!peers.want_permission(ip(1), start + Duration::from_secs(1)));
    assert!(peers.want_permission(ip(1), start + REFUSED_FOR));
}

#[test]
fn permissions_are_bounded_and_the_least_used_goes() {
    let start = Instant::now();
    let mut peers = Peers::default();
    for n in 0..MAX_PERMISSIONS as u32 {
        assert!(peers.want_permission(ip(n), start + Duration::from_millis(u64::from(n))));
    }
    // Every one is being asked for: there is no room.
    assert!(!peers.want_permission(ip(9999), start));
    for n in 0..MAX_PERMISSIONS as u32 {
        peers.permission_answered(ip(n), true, start);
    }
    assert!(peers.want_permission(ip(9999), start + Duration::from_secs(1)));
    assert_eq!(peers.counts().0, MAX_PERMISSIONS);
    // ip(0) was the least recently used.
    assert!(peers.want_permission(ip(0), start + Duration::from_secs(2)), "it was evicted");
}

#[test]
fn channels_are_bound_once_bounded_and_numbered_in_turn() {
    let start = Instant::now();
    let mut peers = Peers::default();
    let first = peers.want_channel(peer(1), start).unwrap();
    assert_eq!(first, 0x4000);
    assert_eq!(peers.want_channel(peer(1), start), None, "being bound");
    assert_eq!(peers.channel(peer(1), start), None, "not confirmed yet");
    assert_eq!(peers.peer_of(first), Some(peer(1)));
    peers.channel_answered(first, true, start);
    assert_eq!(peers.channel(peer(1), start), Some(first));
    for n in 2..=MAX_CHANNELS as u16 {
        assert!(peers.want_channel(peer(n), start).is_some());
    }
    assert_eq!(peers.want_channel(peer(999), start), None, "bounded");
    assert_eq!(peers.counts().1, MAX_CHANNELS);
}

#[test]
fn a_used_channel_is_refreshed_and_an_unused_one_unbound() {
    let start = Instant::now();
    let mut peers = Peers::default();
    let used = peers.want_channel(peer(1), start).unwrap();
    let idle = peers.want_channel(peer(2), start).unwrap();
    peers.channel_answered(used, true, start);
    peers.channel_answered(idle, true, start);
    peers.channel(peer(1), start + Duration::from_secs(5));
    let due = start + CHANNEL_LIFETIME - REFRESH_AHEAD;
    assert_eq!(peers.channels_due(due), vec![(used, peer(1))]);
    assert!(peers.channels_due(start + CHANNEL_LIFETIME).is_empty());
    assert_eq!(peers.peer_of(idle), None, "the unused one is unbound");
    // Its number is not handed out again at once.
    assert_ne!(peers.want_channel(peer(3), start + CHANNEL_LIFETIME), Some(idle));
}

#[test]
fn a_refused_channel_is_forgotten_after_a_while() {
    let start = Instant::now();
    let mut peers = Peers::default();
    let number = peers.want_channel(peer(1), start).unwrap();
    peers.channel_answered(number, false, start);
    assert_eq!(peers.next_due(), Some(start + REFUSED_FOR));
    assert!(peers.channels_due(start + REFUSED_FOR).is_empty());
    assert_eq!(peers.counts().1, 0);
    assert!(peers.want_channel(peer(1), start + REFUSED_FOR).is_some());
}

#[test]
fn a_bound_channel_installs_its_peers_permission() {
    let start = Instant::now();
    let mut peers = Peers::default();
    peers.want_permission(peer(1).ip(), start);
    let number = peers.want_channel(peer(1), start).unwrap();
    peers.channel_answered(number, true, start);
    assert!(!peers.want_permission(peer(1).ip(), start), "installed by the binding");
}
