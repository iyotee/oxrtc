//! TURN over TCP (`turn:...?transport=tcp`, RFC 8656 section 3.1), as
//! openIVR's relay entry lists it beside the UDP one: for a viewer whose
//! network lets no UDP through, the only path to the camera is a relay the
//! server reaches over TCP. The relayed side is UDP.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing, clippy::panic)]

mod common;

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use common::paths::{candidates_of, connect_relay_only};
use common::turn_server::TurnServer;
use common::*;
use oxrtc_core::{CancellationToken, Codec, Stream};
use oxrtc_webrtc::{IceServer, WebRtcConfig, WebRtcServer};
use str0m::media::MediaKind as RtcKind;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, UdpSocket};

const USERNAME: &str = "1700000000:site";
const PASSWORD: &str = "s3cret-credential";

fn entry(urls: Vec<String>, password: &str) -> IceServer {
    IceServer { urls, username: Some(USERNAME.into()), credential: Some(password.into()) }
}

fn stream() -> Arc<Stream> {
    let (params, video) = synthetic_h264();
    hub(FakeFactory::default().with("cam", av_script(Codec::H264, params, video, false)))
        .upsert("cam", &["fake:cam"])
        .unwrap()
}

/// What openIVR writes where the UDP path is closed: its UDP URL answers
/// nothing, its TCP one reaches the relay.
async fn udp_blocked(server: &TurnServer, password: &str) -> (IceServer, UdpSocket) {
    let blackhole = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let urls = vec![
        format!("turn:{}?transport=udp", blackhole.local_addr().unwrap()),
        format!("turn:{}?transport=tcp", server.tcp_addr),
    ];
    (entry(urls, password), blackhole)
}

/// The answer a browser offering video and audio gets.
async fn answer(server: &WebRtcServer, stream: Arc<Stream>) -> String {
    let mut rtc = browser(false).build(Instant::now());
    let mut api = rtc.sdp_api();
    api.add_media(RtcKind::Video, str0m::media::Direction::RecvOnly, None, None, None);
    let (offer, _pending) = api.apply().unwrap();
    server.answer(stream, &offer.to_sdp_string(), None).await.unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_session_whose_only_path_is_the_tcp_relay_plays() {
    let turn = TurnServer::start(USERNAME, PASSWORD).await;
    let (ice, _blackhole) = udp_blocked(&turn, PASSWORD).await;
    let server = server_with(vec![ice], false).await;

    let (mut client, relayed) = connect_relay_only(&server, stream(), browser(false)).await.unwrap();
    assert_ne!(relayed.port(), server.local_addr().port(), "the relayed address is the TURN server's");
    assert_video(&mut client, 10).await;

    let stats = &turn.stats;
    assert_eq!(stats.tcp_connections.load(Ordering::Relaxed), 1, "one connection, shared by the sessions");
    assert_eq!(stats.allocations.load(Ordering::Relaxed), 1, "one allocation, and none over UDP");
    assert_eq!(stats.tcp_allocations.load(Ordering::Relaxed), 1);
    assert!(stats.permissions.lock().unwrap().contains(&client.local.ip()), "a permission for the viewer");
    // The same ten frames as over UDP, through a stream that stays aligned
    // only if each ChannelData message is padded to four bytes.
    let sent = stats.to_peers.load(Ordering::Relaxed);
    assert!(sent > 6_000, "the media went through the relay: {sent} bytes");
    assert!(stats.to_clients.load(Ordering::Relaxed) > 0, "and the viewer's checks and DTLS came back through it");
    assert!(stats.channels.load(Ordering::Relaxed) >= 1, "a channel was bound for the viewer");
    assert_eq!(stats.refused.load(Ordering::Relaxed), 0);
    drop(client);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn openivrs_entry_allocates_over_udp_and_over_tcp() {
    // As openIVR writes it: the same server over both transports.
    let turn = TurnServer::start(USERNAME, PASSWORD).await;
    let urls = vec![format!("turn:{}?transport=udp", turn.addr), format!("turn:{}?transport=tcp", turn.tcp_addr)];
    let server = server_with(vec![entry(urls, PASSWORD)], false).await;
    let answer = answer(&server, stream()).await;
    let relayed = candidates_of(&answer, "relay", "udp");
    assert_eq!(relayed.len(), 2, "a relayed candidate per transport: {relayed:?}");
    assert_ne!(relayed[0], relayed[1]);
    assert_eq!(turn.stats.allocations.load(Ordering::Relaxed), 2);
    assert_eq!(turn.stats.tcp_allocations.load(Ordering::Relaxed), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_connection_the_network_resets_is_made_again() {
    let turn = TurnServer::start(USERNAME, PASSWORD).await;
    let (ice, _blackhole) = udp_blocked(&turn, PASSWORD).await;
    let server = server_with(vec![ice], false).await;
    let first = candidates_of(&answer(&server, stream()).await, "relay", "udp");
    assert_eq!(first.len(), 1);

    // A NAT that forgot the connection: the allocation is lost with it, and
    // made again on a new connection after the core's backoff (about a second).
    turn.reset_tcp_connections();
    assert!(
        eventually(Duration::from_secs(10), || turn.stats.tcp_allocations.load(Ordering::Relaxed) == 2).await,
        "{} allocations after the reset",
        turn.stats.tcp_allocations.load(Ordering::Relaxed)
    );
    assert_eq!(turn.stats.tcp_connections.load(Ordering::Relaxed), 2);
    // And a viewer whose only path is the new relay plays.
    let (mut client, relayed) = connect_relay_only(&server, stream(), browser(false)).await.unwrap();
    assert_ne!(Some(relayed), first.first().copied(), "a new allocation has a new address");
    assert_video(&mut client, 5).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_allocation_is_released_over_tcp_when_the_server_stops() {
    let turn = TurnServer::start(USERNAME, PASSWORD).await;
    let (ice, _blackhole) = udp_blocked(&turn, PASSWORD).await;
    let config = WebRtcConfig {
        listen: "127.0.0.1:0".parse().unwrap(),
        ice_servers: vec![ice],
        tcp: false,
        ..WebRtcConfig::default()
    };
    let cancel = CancellationToken::new();
    let server = WebRtcServer::bind(config, cancel.clone()).await.unwrap();
    answer(&server, stream()).await;
    assert_eq!(turn.stats.tcp_allocations.load(Ordering::Relaxed), 1);
    cancel.cancel();
    assert!(
        eventually(Duration::from_secs(3), || turn.stats.releases.load(Ordering::Relaxed) == 1).await,
        "the Refresh with a lifetime of 0 reached the server before the connection closed"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn refused_credentials_over_tcp_leave_the_host_candidates() {
    let turn = TurnServer::start(USERNAME, "right").await;
    let (ice, _blackhole) = udp_blocked(&turn, "wrong").await;
    let server = server_with(vec![ice], false).await;
    let started = Instant::now();
    let mut client = connect(&server, stream(), browser(false)).await.unwrap();
    assert!(started.elapsed() < Duration::from_secs(3), "the answer does not wait on a refused relay");
    assert_video(&mut client, 5).await;
    assert!(turn.stats.refused.load(Ordering::Relaxed) >= 1);
    assert_eq!(turn.stats.allocations.load(Ordering::Relaxed), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_tcp_server_that_never_answers_delays_no_offer_after_startup() {
    // Takes the connection and every byte, answers none.
    let silent = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("turn:{}?transport=tcp", silent.local_addr().unwrap());
    let held = tokio::spawn(async move {
        let (mut stream, _) = silent.accept().await.unwrap();
        let mut buf = [0u8; 512];
        while stream.read(&mut buf).await.is_ok_and(|n| n > 0) {}
    });
    let server = server_with(vec![entry(vec![url], PASSWORD)], false).await;
    tokio::time::sleep(Duration::from_millis(2200)).await;
    let started = Instant::now();
    let mut client = connect(&server, stream(), browser(false)).await.unwrap();
    assert!(started.elapsed() < Duration::from_secs(1), "answered in {:?}", started.elapsed());
    assert_video(&mut client, 5).await;
    held.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_server_that_does_not_speak_turn_is_hung_up_on_and_tried_again() {
    // An HTTP error page where the TURN answer should be.
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("turn:{}?transport=tcp", listener.local_addr().unwrap());
    let accepted = Arc::new(AtomicUsize::new(0));
    let hung_up = Arc::new(AtomicUsize::new(0));
    let (counted, ended) = (accepted.clone(), hung_up.clone());
    let task = tokio::spawn(async move {
        loop {
            let (mut stream, _) = listener.accept().await.unwrap();
            counted.fetch_add(1, Ordering::Relaxed);
            let _ = stream.write_all(b"HTTP/1.1 400 Bad Request\r\n\r\n").await;
            let mut buf = [0u8; 512];
            // Reads what the client sent, then its end of the connection.
            while stream.read(&mut buf).await.is_ok_and(|n| n > 0) {}
            ended.fetch_add(1, Ordering::Relaxed);
        }
    });
    let _server = server_with(vec![entry(vec![url], PASSWORD)], false).await;
    assert!(
        eventually(Duration::from_secs(8), || accepted.load(Ordering::Relaxed) >= 2).await,
        "tried again after about a second: {} connections",
        accepted.load(Ordering::Relaxed)
    );
    assert!(hung_up.load(Ordering::Relaxed) >= 1, "the client closed the connection it could not read");
    task.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_peer_datagram_too_long_for_the_framer_costs_the_relay_nothing() {
    // The relay is shared by every session: a viewer, whose IP holds a
    // permission, sends the relayed address 20 kB, which the server hands on
    // as one message. Over UDP such a datagram is dropped; over TCP it must
    // not close the connection, and the allocation, for the others.
    let turn = TurnServer::start(USERNAME, PASSWORD).await;
    let (ice, _blackhole) = udp_blocked(&turn, PASSWORD).await;
    let server = server_with(vec![ice], false).await;
    let (mut client, relayed) = connect_relay_only(&server, stream(), browser(false)).await.unwrap();
    assert_video(&mut client, 3).await;

    let peer = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    for _ in 0..3 {
        peer.send_to(&vec![0xAB; 20_000], relayed).await.unwrap();
    }
    // Longer than the backoff of a lost connection (about a second).
    tokio::time::sleep(Duration::from_millis(2500)).await;
    let stats = &turn.stats;
    assert_eq!(stats.tcp_connections.load(Ordering::Relaxed), 1, "the connection was closed and made again");
    assert_eq!(stats.tcp_allocations.load(Ordering::Relaxed), 1);
    // The session is still carried by the relay.
    let before = stats.to_peers.load(Ordering::Relaxed);
    tokio::time::sleep(Duration::from_millis(1000)).await;
    assert!(stats.to_peers.load(Ordering::Relaxed) > before, "the session no longer sends through the relay");
}
