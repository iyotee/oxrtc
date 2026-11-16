//! Viewers outside the LAN: a relayed candidate from a TURN server, a
//! server-reflexive one from a STUN server, and ICE-TCP - each proven by a
//! client that has no other way to reach the server.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing, clippy::panic)]

mod common;

use std::net::SocketAddr;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use common::paths::{candidates_of, connect_relay_only, connect_tcp_only};
use common::turn_server::TurnServer;
use common::*;
use oxrtc_core::{Codec, Hub, Stream};
use oxrtc_webrtc::{IceServer, WebRtcConfig, WebRtcServer};
use str0m::media::MediaKind as RtcKind;
use tokio::net::UdpSocket;

fn turn(server: &TurnServer, username: &str, password: &str) -> IceServer {
    IceServer {
        // The UDP URL of what openIVR writes; its TCP one is `turn_tcp.rs`'s.
        urls: vec![format!("turn:{}?transport=udp", server.addr)],
        username: Some(username.into()),
        credential: Some(password.into()),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_session_whose_only_path_is_the_relay_plays() {
    let turn_server = TurnServer::start("1700000000:site", "s3cret-credential").await;
    let (params, video) = synthetic_h264();
    let hub = hub(FakeFactory::default().with("cam", av_script(Codec::H264, params, video, false)));
    let stream = hub.upsert("cam", &["fake:cam"]).unwrap();
    let server = server_with(vec![turn(&turn_server, "1700000000:site", "s3cret-credential")], false).await;

    let (mut client, relayed) = connect_relay_only(&server, stream, browser(false)).await.unwrap();
    assert_ne!(relayed.port(), server.local_addr().port(), "the relayed address is the TURN server's");
    assert_video(&mut client, 10).await;

    let stats = &turn_server.stats;
    assert_eq!(stats.allocations.load(Ordering::Relaxed), 1, "one allocation, shared by the sessions");
    assert!(stats.permissions.lock().unwrap().contains(&client.local.ip()), "a permission for the viewer");
    // oxrtc is the TURN client, the viewer its peer. Ten frames of the
    // synthetic clip: a 3 kB keyframe and 400-byte deltas.
    let sent = stats.to_peers.load(Ordering::Relaxed);
    assert!(sent > 6_000, "the media went through the relay: {sent} bytes");
    assert!(stats.to_clients.load(Ordering::Relaxed) > 0, "and the viewer's checks and DTLS came back through it");
    assert!(stats.channels.load(Ordering::Relaxed) >= 1, "a channel was bound for the viewer");
    assert_eq!(stats.refused.load(Ordering::Relaxed), 0);
    drop(client);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn refused_turn_credentials_leave_the_host_candidates() {
    let turn_server = TurnServer::start("1700000000:site", "right").await;
    let (params, video) = synthetic_h264();
    let hub = hub(FakeFactory::default().with("cam", av_script(Codec::H264, params, video, false)));
    let stream = hub.upsert("cam", &["fake:cam"]).unwrap();
    let server = server_with(vec![turn(&turn_server, "1700000000:site", "wrong")], false).await;
    let started = Instant::now();
    let mut client = connect(&server, stream, browser(false)).await.unwrap();
    assert!(started.elapsed() < Duration::from_secs(3), "the answer does not wait on a refused relay");
    assert_video(&mut client, 5).await;
    assert!(turn_server.stats.refused.load(Ordering::Relaxed) >= 1);
    assert_eq!(turn_server.stats.allocations.load(Ordering::Relaxed), 0);
}

/// A STUN server that answers every binding request with `mapped`, as if a
/// NAT stood between it and the server.
async fn stun_server(mapped: SocketAddr) -> SocketAddr {
    let socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let addr = socket.local_addr().unwrap();
    tokio::spawn(async move {
        let mut buf = [0u8; 1500];
        while let Ok((n, from)) = socket.recv_from(&mut buf).await {
            if n < 20 || buf[0..2] != [0x00, 0x01] {
                continue;
            }
            let mut reply = vec![0x01, 0x01, 0x00, 0x0c];
            reply.extend_from_slice(&buf[4..20]);
            let SocketAddr::V4(v4) = mapped else { unreachable!() };
            reply.extend_from_slice(&[0x00, 0x20, 0x00, 0x08, 0x00, 0x01]);
            reply.extend_from_slice(&(v4.port() ^ 0x2112).to_be_bytes());
            reply.extend_from_slice(&(u32::from(*v4.ip()) ^ 0x2112_A442).to_be_bytes());
            let _ = socket.send_to(&reply, from).await;
        }
    });
    addr
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_stun_server_adds_a_server_reflexive_candidate() {
    let mapped: SocketAddr = "198.51.100.77:40000".parse().unwrap();
    let stun = stun_server(mapped).await;
    let (params, video) = synthetic_h264();
    let hub = hub(FakeFactory::default().with("cam", av_script(Codec::H264, params, video, false)));
    let stream = hub.upsert("cam", &["fake:cam"]).unwrap();
    let ice = IceServer { urls: vec![format!("stun:{stun}")], username: None, credential: None };
    let server = server_with(vec![ice], false).await;
    // The answer carries it, and a LAN viewer still plays over the host
    // candidate (the session runs full ICE now, not ICE-lite).
    let socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let mut client = connect_on(&server, stream.clone(), browser(false), socket).await.unwrap();
    assert_video(&mut client, 5).await;
    let mut rtc = browser(false).build(Instant::now());
    let mut api = rtc.sdp_api();
    api.add_media(RtcKind::Video, str0m::media::Direction::RecvOnly, None, None, None);
    let (offer, _pending) = api.apply().unwrap();
    let answer = server.answer(stream, &offer.to_sdp_string(), None).await.unwrap();
    assert_eq!(candidates_of(&answer, "srflx", "udp"), vec![mapped]);
    assert!(!answer.contains("a=ice-lite"), "a server-reflexive candidate needs full ICE");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_client_without_udp_plays_over_ice_tcp() {
    let (params, video) = synthetic_h264();
    let hub = hub(FakeFactory::default().with("cam", av_script(Codec::H264, params, video, false)));
    let stream = hub.upsert("cam", &["fake:cam"]).unwrap();
    let server = server_with(Vec::new(), true).await;
    assert!(server.tcp_addr().is_some());
    let mut client = connect_tcp_only(&server, stream, browser(false)).await.unwrap();
    assert_video(&mut client, 10).await;
    assert_eq!(server.session_count(), 1);
    client.stop(Control::Close);
    assert!(eventually(WAIT, || server.session_count() == 0).await, "the session ends with its connection");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ice_tcp_connections_are_bounded() {
    let server = server_with(Vec::new(), true).await;
    let addr = server.tcp_addr().unwrap();
    let mut held = Vec::new();
    for _ in 0..4 {
        held.push(tokio::net::TcpStream::connect(addr).await.unwrap());
    }
    // The fifth is closed at once: reading it ends.
    let mut extra = tokio::net::TcpStream::connect(addr).await.unwrap();
    let mut byte = [0u8; 1];
    let read = tokio::time::timeout(Duration::from_secs(3), tokio::io::AsyncReadExt::read(&mut extra, &mut byte)).await;
    assert!(matches!(read, Ok(Ok(0)) | Ok(Err(_))), "the connection beyond the limit is closed: {read:?}");
    // Junk on a held one closes nothing else and crashes nothing.
    tokio::io::AsyncWriteExt::write_all(&mut held[0], &[0, 4, 1, 2, 3, 4, 0xff, 0xff]).await.unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(server.tcp_addr().is_some());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_silent_turn_server_delays_no_offer_after_startup() {
    // Takes every datagram, answers none.
    let silent = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let ice = IceServer {
        urls: vec![format!("turn:{}", silent.local_addr().unwrap())],
        username: Some("u".into()),
        credential: Some("p".into()),
    };
    let (params, video) = synthetic_h264();
    let hub = hub(FakeFactory::default().with("cam", av_script(Codec::H264, params, video, false)));
    let stream = hub.upsert("cam", &["fake:cam"]).unwrap();
    let server = server_with(vec![ice], false).await;
    tokio::time::sleep(Duration::from_millis(2200)).await;
    let started = Instant::now();
    let mut client = connect(&server, stream, browser(false)).await.unwrap();
    assert!(started.elapsed() < Duration::from_secs(1), "answered in {:?}", started.elapsed());
    assert_video(&mut client, 5).await;
    drop(silent);
}

// ---------------------------------------------------------------------------
// Replacing the ICE servers while the server runs (D22): a rotation of the
// TURN credentials must not restart anything.

const OLD_USER: &str = "1700000000:site";
const NEW_USER: &str = "1700003600:site";

async fn server_rotating(ice: Vec<IceServer>, grace: Duration) -> WebRtcServer {
    let config = WebRtcConfig {
        listen: "127.0.0.1:0".parse().unwrap(),
        ice_servers: ice,
        tcp: false,
        ice_retire_grace: grace,
        max_sessions: 16,
        max_sessions_per_ip: 16,
        ..WebRtcConfig::default()
    };
    WebRtcServer::bind(config, oxrtc_core::CancellationToken::new()).await.unwrap()
}

fn camera() -> (std::sync::Arc<Hub>, std::sync::Arc<Stream>) {
    let (params, video) = synthetic_h264();
    let hub = hub(FakeFactory::default().with("cam", av_script(Codec::H264, params, video, false)));
    let stream = hub.upsert("cam", &["fake:cam"]).unwrap();
    (hub, stream)
}

/// Video frames received within `WAIT`, after the ones already queued.
async fn video_frames(client: &mut Client, frames: usize) -> usize {
    while client.events.try_recv().is_ok() {}
    let mut seen = 0;
    while seen < frames
        && client
            .wait_for(Duration::from_secs(2), |e| matches!(e, ClientEvent::Media { kind: RtcKind::Video, .. }))
            .await
            .is_some()
    {
        seen += 1;
    }
    seen
}

/// A relay-only session made once the answer no longer offers `old`: the new
/// allocation holds. Each attempt before it is closed.
async fn connect_after(
    server: &WebRtcServer,
    stream: &std::sync::Arc<Stream>,
    old: SocketAddr,
) -> (Client, SocketAddr) {
    for _ in 0..50 {
        let (mut client, relayed) = connect_relay_only(server, stream.clone(), browser(false)).await.unwrap();
        if relayed != old {
            return (client, relayed);
        }
        client.stop(Control::Close);
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("the answer still offers the old relayed address");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn new_credentials_serve_new_sessions_and_the_old_session_keeps_playing() {
    let turn_server = TurnServer::start(OLD_USER, "old-credential").await;
    let (_hub, stream) = camera();
    let server = server_rotating(vec![turn(&turn_server, OLD_USER, "old-credential")], Duration::from_secs(60)).await;
    let (mut before, old) = connect_relay_only(&server, stream.clone(), browser(false)).await.unwrap();
    assert_video(&mut before, 5).await;
    assert_eq!(turn_server.stats.allocations.load(Ordering::Relaxed), 1);

    // The rotation: the server takes the new credential and forgets the old one.
    turn_server.accept_also(NEW_USER, "new-credential");
    let summary = server.set_ice_servers(&[turn(&turn_server, NEW_USER, "new-credential")]);
    assert_eq!((summary.turn, summary.stun), (1, 0));
    turn_server.forget(OLD_USER);

    let (mut after, new) = connect_after(&server, &stream, old).await;
    assert_ne!(new, old, "a new allocation, so a new relayed address");
    assert_video(&mut after, 5).await;
    let made = turn_server.stats.allocated_as.lock().unwrap().clone();
    assert_eq!(made, vec![OLD_USER, NEW_USER], "the second allocation was made with the new credential: {made:?}");
    // What the old session was offered keeps carrying its media.
    assert_eq!(video_frames(&mut before, 20).await, 20, "the session made before the rotation keeps playing");
    assert_eq!(turn_server.stats.releases.load(Ordering::Relaxed), 0, "nothing is released while a session uses it");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_old_allocation_is_released_once_its_last_session_ends() {
    let turn_server = TurnServer::start(OLD_USER, "old-credential").await;
    let (_hub, stream) = camera();
    let server =
        server_rotating(vec![turn(&turn_server, OLD_USER, "old-credential")], Duration::from_millis(300)).await;
    let (mut before, old) = connect_relay_only(&server, stream.clone(), browser(false)).await.unwrap();
    assert_video(&mut before, 5).await;

    turn_server.accept_also(NEW_USER, "new-credential");
    server.set_ice_servers(&[turn(&turn_server, NEW_USER, "new-credential")]);
    let (mut after, _) = connect_after(&server, &stream, old).await;
    assert_video(&mut after, 5).await;

    // Past the grace, and still in use: kept.
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert_eq!(turn_server.stats.releases.load(Ordering::Relaxed), 0, "the old session still has its address");
    assert_eq!(video_frames(&mut before, 10).await, 10);

    before.stop(Control::Close);
    assert!(
        eventually(WAIT, || turn_server.stats.releases.load(Ordering::Relaxed) == 1).await,
        "the old allocation is released when nobody uses it"
    );
    assert_eq!(video_frames(&mut after, 10).await, 10, "the new allocation is not touched");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_same_servers_again_change_nothing() {
    let turn_server = TurnServer::start(OLD_USER, "old-credential").await;
    let server = server_rotating(vec![turn(&turn_server, OLD_USER, "old-credential")], Duration::from_secs(60)).await;
    assert!(eventually(WAIT, || turn_server.stats.allocations.load(Ordering::Relaxed) == 1).await);
    for _ in 0..3 {
        server.set_ice_servers(&[turn(&turn_server, OLD_USER, "old-credential")]);
    }
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(turn_server.stats.allocations.load(Ordering::Relaxed), 1, "the allocation is kept, not made again");
    assert_eq!(turn_server.stats.releases.load(Ordering::Relaxed), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn refused_new_credentials_leave_the_old_allocation_offered() {
    let turn_server = TurnServer::start(OLD_USER, "old-credential").await;
    let (_hub, stream) = camera();
    let server = server_rotating(vec![turn(&turn_server, OLD_USER, "old-credential")], Duration::from_secs(60)).await;
    let (mut first, old) = connect_relay_only(&server, stream.clone(), browser(false)).await.unwrap();
    assert_video(&mut first, 5).await;

    // A credential the server refuses: the allocation that holds stays the one offered.
    server.set_ice_servers(&[turn(&turn_server, NEW_USER, "not-known")]);
    assert!(eventually(WAIT, || turn_server.stats.refused.load(Ordering::Relaxed) >= 1).await);
    let (mut second, offered) = connect_relay_only(&server, stream.clone(), browser(false)).await.unwrap();
    assert_eq!(offered, old, "the old allocation is still what a new session is offered");
    assert_video(&mut second, 5).await;
    assert_eq!(video_frames(&mut first, 10).await, 10);

    // A good one a minute later takes over, and the refused one never held.
    turn_server.accept_also(NEW_USER, "new-credential");
    server.set_ice_servers(&[turn(&turn_server, NEW_USER, "new-credential")]);
    let (mut third, new) = connect_after(&server, &stream, old).await;
    assert_ne!(new, old);
    assert_video(&mut third, 5).await;
    assert_eq!(turn_server.stats.allocations.load(Ordering::Relaxed), 2, "the refused credential made no allocation");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn removing_the_servers_keeps_the_sessions_and_offers_nothing_new() {
    let turn_server = TurnServer::start(OLD_USER, "old-credential").await;
    let (_hub, stream) = camera();
    let server = server_rotating(vec![turn(&turn_server, OLD_USER, "old-credential")], Duration::from_secs(60)).await;
    let (mut before, _) = connect_relay_only(&server, stream.clone(), browser(false)).await.unwrap();
    assert_video(&mut before, 5).await;

    let summary = server.set_ice_servers(&[]);
    assert_eq!((summary.turn, summary.stun), (0, 0));
    let mut rtc = browser(false).build(Instant::now());
    let mut api = rtc.sdp_api();
    api.add_media(RtcKind::Video, str0m::media::Direction::RecvOnly, None, None, None);
    let (offer, _pending) = api.apply().unwrap();
    let answer = server.answer(stream, &offer.to_sdp_string(), None).await.unwrap();
    assert!(candidates_of(&answer, "relay", "udp").is_empty(), "no relayed candidate is offered any more");
    assert_eq!(video_frames(&mut before, 20).await, 20, "the session that has one keeps it");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_stun_server_added_at_runtime_gives_a_server_reflexive_candidate() {
    let mapped: SocketAddr = "198.51.100.78:40001".parse().unwrap();
    let stun = stun_server(mapped).await;
    let (_hub, stream) = camera();
    let server = server_rotating(Vec::new(), Duration::from_secs(60)).await;
    let ice = IceServer { urls: vec![format!("stun:{stun}")], username: None, credential: None };
    let summary = server.set_ice_servers(&[ice]);
    assert_eq!((summary.turn, summary.stun), (0, 1));
    let mut found = Vec::new();
    for _ in 0..50 {
        let mut rtc = browser(false).build(Instant::now());
        let mut api = rtc.sdp_api();
        api.add_media(RtcKind::Video, str0m::media::Direction::RecvOnly, None, None, None);
        let (offer, _pending) = api.apply().unwrap();
        let answer = server.answer(stream.clone(), &offer.to_sdp_string(), None).await.unwrap();
        found = candidates_of(&answer, "srflx", "udp");
        if !found.is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert_eq!(found, vec![mapped]);
    server.set_ice_servers(&[]);
    let mut rtc = browser(false).build(Instant::now());
    let mut api = rtc.sdp_api();
    api.add_media(RtcKind::Video, str0m::media::Direction::RecvOnly, None, None, None);
    let (offer, _pending) = api.apply().unwrap();
    let answer = server.answer(stream, &offer.to_sdp_string(), None).await.unwrap();
    assert!(candidates_of(&answer, "srflx", "udp").is_empty(), "a STUN server removed is no longer asked");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn retired_allocations_are_bounded() {
    // Twelve rotations, each waited for: no session ever uses an old allocation
    // and the grace is long, so only the bound ends them.
    let users: Vec<String> = (0..13).map(|i| format!("{}:site", 1_700_000_000 + i)).collect();
    let turn_server = TurnServer::start(&users[0], "credential-0").await;
    let server = server_rotating(vec![turn(&turn_server, &users[0], "credential-0")], Duration::from_secs(600)).await;
    assert!(eventually(WAIT, || turn_server.stats.allocations.load(Ordering::Relaxed) == 1).await);
    for (i, user) in users.iter().enumerate().skip(1) {
        let password = format!("credential-{i}");
        turn_server.accept_also(user, &password);
        server.set_ice_servers(&[turn(&turn_server, user, &password)]);
        assert!(eventually(WAIT, || turn_server.stats.allocations.load(Ordering::Relaxed) == i + 1).await);
        // The new allocation holds a moment after it is made: the old one is retired then.
        tokio::time::sleep(Duration::from_millis(150)).await;
    }
    // Retired at most eight at the time of each replacement: the ninth, tenth
    // and eleventh... of 12 rotations are the three that ended.
    assert_eq!(turn_server.stats.releases.load(Ordering::Relaxed), 3, "the oldest ended past the bound");
}
