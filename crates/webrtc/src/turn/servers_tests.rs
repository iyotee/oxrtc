//! What a replacement of the servers keeps, drops and retires, without any
//! network: nothing here has an allocation, so what is told apart is the
//! configuration (the allocations that held are `tests/remote.rs`'s).

use std::sync::Arc;
use std::time::{Duration, Instant};

use oxrtc_core::CancellationToken;

use super::{IceServer, IceServers, MAX_PROBES, MAX_RELAYS};

fn turn(host: &str, user: &str, password: &str) -> IceServer {
    IceServer {
        urls: vec![format!("turn:{host}:3478?transport=udp")],
        username: Some(user.into()),
        credential: Some(password.into()),
    }
}

fn stun(host: &str) -> IceServer {
    IceServer { urls: vec![format!("stun:{host}:3478")], username: None, credential: None }
}

fn servers(config: &[IceServer]) -> IceServers {
    IceServers::new(config, &CancellationToken::new())
}

#[test]
fn the_same_servers_keep_their_allocations() {
    let ice = servers(&[turn("a.test", "u", "p"), stun("s.test")]);
    let before = ice.alive();
    let summary = ice.replace(&[turn("a.test", "u", "p"), stun("s.test")]);
    assert_eq!((summary.turn, summary.stun), (1, 1));
    let after = ice.alive();
    assert_eq!(after.len(), 1);
    assert!(Arc::ptr_eq(&before[0], &after[0]), "the same allocation, not a new one");
    assert!(!after[0].is_stopped());
}

#[test]
fn a_new_credential_makes_a_new_allocation_and_drops_one_that_never_held() {
    let ice = servers(&[turn("a.test", "old", "p")]);
    let old = ice.alive().remove(0);
    ice.replace(&[turn("a.test", "new", "p")]);
    let alive = ice.alive();
    assert_eq!(alive.len(), 1, "the one that never held is not kept beside the new");
    assert!(old.is_stopped());
    assert!(!Arc::ptr_eq(&old, &alive[0]));
    assert_eq!(alive[0].credentials().username, "new");
}

#[test]
fn a_server_no_longer_listed_is_dropped_and_one_added_is_kept() {
    let ice = servers(&[turn("a.test", "u", "p")]);
    ice.replace(&[turn("b.test", "u", "p"), stun("s.test")]);
    let hosts: Vec<String> = ice.alive().iter().map(|r| r.url().host.clone()).collect();
    assert_eq!(hosts, vec!["b.test"]);
    let summary = ice.replace(&[]);
    assert_eq!((summary.turn, summary.stun), (0, 0));
    assert!(ice.alive().is_empty());
    assert!(ice.reflexive().is_empty());
}

#[test]
fn the_limits_hold_for_a_replacement_as_for_the_configuration() {
    let many: Vec<IceServer> = (0..10).map(|i| turn(&format!("t{i}.test"), "u", "p")).collect();
    let stuns: Vec<IceServer> = (0..10).map(|i| stun(&format!("s{i}.test"))).collect();
    let ice = servers(&[]);
    let summary = ice.replace(&[many, stuns].concat());
    assert_eq!((summary.turn, summary.stun), (MAX_RELAYS, MAX_PROBES));
    assert_eq!(ice.alive().len(), MAX_RELAYS);
}

#[test]
fn a_turn_server_without_credentials_and_a_tls_url_are_not_counted() {
    let ice = servers(&[]);
    let secure =
        IceServer { urls: vec!["turns:t.test:5349".into()], username: Some("u".into()), credential: Some("p".into()) };
    let bare = IceServer { urls: vec!["turn:t.test".into()], username: None, credential: None };
    let summary = ice.replace(&[secure, bare, turn("ok.test", "u", "p")]);
    assert_eq!((summary.turn, summary.stun), (1, 0));
}

#[test]
fn a_retired_allocation_ends_after_its_grace_when_no_session_has_its_address() {
    let ice = servers(&[turn("a.test", "u", "p")]);
    let relay = ice.alive().remove(0);
    let now = Instant::now();
    relay.retire(now);
    ice.reap(now + Duration::from_secs(1), Duration::from_secs(60), |_| false);
    assert!(!relay.is_stopped(), "kept for the grace, whatever uses it");
    assert_eq!(ice.alive().len(), 1);
    ice.reap(now + Duration::from_secs(61), Duration::from_secs(60), |_| false);
    assert!(relay.is_stopped());
    assert!(ice.alive().is_empty(), "and forgotten");
}

#[test]
fn an_allocation_still_offered_is_never_reaped() {
    let ice = servers(&[turn("a.test", "u", "p")]);
    ice.reap(Instant::now() + Duration::from_secs(3600), Duration::ZERO, |_| false);
    assert_eq!(ice.alive().len(), 1);
}

#[test]
fn a_retired_relay_is_not_offered_and_not_waited_for() {
    let ice = servers(&[turn("a.test", "u", "p")]);
    ice.alive()[0].retire(Instant::now());
    assert!(ice.relayed().is_empty());
    assert!(ice.permit(&["192.0.2.1".parse().unwrap()]).is_empty());
}
