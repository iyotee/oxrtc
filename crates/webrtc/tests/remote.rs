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
use oxrtc_core::{CancellationToken, Codec};
use oxrtc_webrtc::{IceServer, WebRtcConfig, WebRtcServer};
use str0m::media::MediaKind as RtcKind;
use tokio::net::UdpSocket;

const WAIT: Duration = Duration::from_secs(10);

async fn server_with(ice_servers: Vec<IceServer>, tcp: bool) -> WebRtcServer {
    let config = WebRtcConfig {
        listen: "127.0.0.1:0".parse().unwrap(),
        ice_servers,
        tcp,
        max_sessions: 4,
        max_sessions_per_ip: 4,
        ..WebRtcConfig::default()
    };
    WebRtcServer::bind(config, CancellationToken::new()).await.unwrap()
}

fn turn(server: &TurnServer, username: &str, password: &str) -> IceServer {
    IceServer {
        // As openIVR writes them: the STUN and TCP URLs are not usable here
        // (no STUN on this test server, no TURN over TCP) and are skipped.
        urls: vec![format!("turn:{}?transport=udp", server.addr), format!("turn:{}?transport=tcp", server.addr)],
        username: Some(username.into()),
        credential: Some(password.into()),
    }
}

/// Waits for a connection and `frames` video frames.
async fn assert_video(client: &mut Client, frames: usize) {
    assert_eq!(client.wait_for(WAIT, |e| *e == ClientEvent::Connected).await, Some(ClientEvent::Connected));
    let mut video = 0;
    let deadline = tokio::time::Instant::now() + WAIT;
    while video < frames && tokio::time::Instant::now() < deadline {
        if client
            .wait_for(Duration::from_secs(1), |e| matches!(e, ClientEvent::Media { kind: RtcKind::Video, .. }))
            .await
            .is_some()
        {
            video += 1;
        }
    }
    assert!(video >= frames, "{video} video frames of {frames}");
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
