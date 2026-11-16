//! The WHEP session resource over the WebRTC egress: a `PATCH` adds the
//! candidates a client trickles, a `DELETE` ends the session at once, and the
//! per-address limits count what a `DELETE` gives back.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing, clippy::panic)]

mod common;

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use common::turn_server::TurnServer;
use common::*;
use oxrtc_core::{CancellationToken, Codec, Stream};
use oxrtc_webrtc::{
    Answered, IceServer, MAX_CANDIDATES_PER_PATCH, MAX_TRICKLED_CANDIDATES, ResourceError, WebRtcConfig, WebRtcServer,
    WhepError,
};
use str0m::Candidate;
use str0m::change::SdpAnswer;
use str0m::media::{Direction, MediaKind as RtcKind};
use tokio::net::UdpSocket;

fn camera() -> Arc<Stream> {
    let (params, video) = synthetic_h264();
    let hub = hub(FakeFactory::default().with("cam", av_script(Codec::H264, params, video, false)));
    hub.upsert("cam", &["fake:cam"]).unwrap()
}

/// A browser that has gathered nothing yet: its offer carries no candidate,
/// and the candidate it will have is trickled by `PATCH`.
struct Trickler {
    rtc: str0m::Rtc,
    socket: UdpSocket,
    local: SocketAddr,
    answered: Answered,
    /// Its candidate, as a line of an `application/trickle-ice-sdpfrag` body.
    line: String,
    ufrag: String,
}

async fn offer_without_candidates(server: &WebRtcServer, stream: Arc<Stream>) -> Result<Trickler, WhepError> {
    let socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let local = socket.local_addr().unwrap();
    let mut rtc = browser(false).build(Instant::now());
    let mut api = rtc.sdp_api();
    api.add_media(RtcKind::Video, Direction::RecvOnly, None, None, None);
    api.add_media(RtcKind::Audio, Direction::RecvOnly, None, None, None);
    let (offer, pending) = api.apply().unwrap();
    let sdp = offer.to_sdp_string();
    assert!(!sdp.contains("a=candidate"), "the offer carries no candidate");
    let ufrag = sdp.lines().find_map(|l| l.strip_prefix("a=ice-ufrag:")).unwrap().trim().to_owned();
    let answered = server.answer_resource(stream, &sdp, Some(local), None).await?;
    rtc.sdp_api().accept_answer(pending, SdpAnswer::from_sdp_string(&answered.sdp).unwrap()).unwrap();
    let candidate = Candidate::host(local, "udp").unwrap();
    let line = format!("a={}", candidate.to_sdp_string());
    rtc.add_local_candidate(candidate);
    Ok(Trickler { rtc, socket, local, answered, line, ufrag })
}

impl Trickler {
    fn fragment(&self) -> String {
        format!("a=ice-ufrag:{}\r\n{}\r\na=end-of-candidates\r\n", self.ufrag, self.line)
    }

    fn play(self) -> (Client, String) {
        let Self { rtc, socket, local, answered, .. } = self;
        let (client, events, control) = Client::new(local);
        tokio::spawn(drive(rtc, socket, events, control, None));
        (client, answered.resource)
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn each_answer_names_its_own_unguessable_session() {
    let stream = camera();
    let server = server(4, Duration::from_secs(10), WAIT).await;
    let mut ids = Vec::new();
    for _ in 0..3 {
        let session = offer_without_candidates(&server, stream.clone()).await.unwrap();
        ids.push(session.answered.resource.clone());
        assert!(session.answered.sdp.contains("a=candidate"), "the answer is complete, as ever");
    }
    for id in &ids {
        assert_eq!(id.len(), 32, "128 bits");
        assert!(id.bytes().all(|b| b.is_ascii_hexdigit()));
    }
    ids.sort();
    ids.dedup();
    assert_eq!(ids.len(), 3);
    assert_eq!(server.session_count(), 3);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_client_whose_offer_carries_no_candidate_plays_after_its_patch() {
    // Not a proof that the PATCH is what connects it: an ICE-lite server learns
    // the peer's route from the peer's own checks, so this passes without the
    // PATCH too (measured by leaving it out). What the PATCH changes is tested
    // where it can: `a_trickled_candidate_is_one_the_server_checks` (full ICE)
    // and `a_trickled_address_gets_a_permission_on_the_relay`.
    let server = server(4, Duration::from_secs(10), WAIT).await;
    let session = offer_without_candidates(&server, camera()).await.unwrap();
    server.trickle(&session.answered.resource, &session.fragment()).await.unwrap();
    let (mut client, _id) = session.play();
    assert_video(&mut client, 10).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_trickled_candidate_is_one_the_server_checks() {
    // A server-reflexive candidate makes the session full ICE, whose agent
    // checks the remote candidates it knows: here, only the trickled one.
    let mapped: SocketAddr = "198.51.100.77:40000".parse().unwrap();
    let stun = stun_server(mapped).await;
    let ice = IceServer { urls: vec![format!("stun:{stun}")], username: None, credential: None };
    let server = server_with(vec![ice], false).await;
    let session = offer_without_candidates(&server, camera()).await.unwrap();
    assert!(!session.answered.sdp.contains("a=ice-lite"), "this session runs full ICE");
    let mut buf = [0u8; 1500];
    let quiet = tokio::time::timeout(Duration::from_millis(600), session.socket.recv_from(&mut buf)).await;
    assert!(quiet.is_err(), "nothing is sent to a candidate the server has not been told of");

    server.trickle(&session.answered.resource, &session.fragment()).await.unwrap();
    let (n, from) = tokio::time::timeout(WAIT, session.socket.recv_from(&mut buf))
        .await
        .expect("the server checks the trickled candidate")
        .unwrap();
    assert!(n >= 20 && buf[4..8] == [0x21, 0x12, 0xA4, 0x42], "a STUN message");
    assert_eq!(buf[0..2], [0x00, 0x01], "a binding request");
    assert_eq!(from, server.local_addr());
}

/// A STUN server that answers every binding request with `mapped`.
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
async fn a_trickled_address_gets_a_permission_on_the_relay() {
    let turn = TurnServer::start("1700000000:site", "s3cret-credential").await;
    let ice = IceServer {
        urls: vec![format!("turn:{}?transport=udp", turn.addr)],
        username: Some("1700000000:site".into()),
        credential: Some("s3cret-credential".into()),
    };
    let server = server_with(vec![ice], false).await;
    let session = offer_without_candidates(&server, camera()).await.unwrap();
    let viewer: std::net::IpAddr = "203.0.113.9".parse().unwrap();
    assert!(!turn.stats.permissions.lock().unwrap().contains(&viewer));
    let line = "a=candidate:1 1 udp 1686052607 203.0.113.9 50001 typ srflx raddr 192.168.1.20 rport 50000\r\n";
    server.trickle(&session.answered.resource, line).await.unwrap();
    assert!(
        eventually(WAIT, || turn.stats.permissions.lock().unwrap().contains(&viewer)).await,
        "the relay lets the viewer's checks through"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_delete_ends_the_session_at_once() {
    let stream = camera();
    let server = server(4, Duration::from_secs(10), WAIT).await;
    let session = offer_without_candidates(&server, stream.clone()).await.unwrap();
    server.trickle(&session.answered.resource, &session.fragment()).await.unwrap();
    let (mut client, id) = session.play();
    assert_video(&mut client, 5).await;
    assert_eq!(stream.snapshot().consumers.len(), 1);

    server.close(&id).await.unwrap();
    // Not "soon": when the call returns, the viewer is gone from the stream.
    assert!(stream.snapshot().consumers.is_empty(), "the consumer left with the answer to the DELETE");
    assert_eq!(server.session_count(), 0);
    assert_eq!(server.close(&id).await, Err(ResourceError::NotFound), "the resource is gone");
    assert_eq!(server.trickle(&id, "a=end-of-candidates\r\n").await, Err(ResourceError::NotFound));
    assert!(client.wait_for(WAIT, |e| *e == ClientEvent::Closed).await.is_some(), "the viewer is told");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_session_that_ended_by_itself_is_not_found() {
    let server = server(4, Duration::from_secs(10), WAIT).await;
    let session = offer_without_candidates(&server, camera()).await.unwrap();
    let (mut client, id) = session.play();
    assert_video(&mut client, 3).await;
    client.stop(Control::Close);
    assert!(eventually(WAIT, || server.session_count() == 0).await);
    assert_eq!(server.close(&id).await, Err(ResourceError::NotFound));
    assert_eq!(server.trickle(&id, "").await, Err(ResourceError::NotFound));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unknown_id_is_not_found_whatever_it_looks_like() {
    let server = server(4, Duration::from_secs(10), WAIT).await;
    let _session = offer_without_candidates(&server, camera()).await.unwrap();
    for id in ["", "0", "ffffffffffffffffffffffffffffffff", "../../api/streams", "ice"] {
        assert_eq!(server.close(id).await, Err(ResourceError::NotFound), "{id:?}");
        assert_eq!(server.trickle(id, "a=end-of-candidates\r\n").await, Err(ResourceError::NotFound), "{id:?}");
    }
    assert_eq!(server.session_count(), 1, "no guess ended a session");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_ice_restart_is_refused_and_the_same_fragment_is_not() {
    let server = server(4, Duration::from_secs(10), WAIT).await;
    let session = offer_without_candidates(&server, camera()).await.unwrap();
    let id = session.answered.resource.clone();
    let restart =
        format!("a=ice-ufrag:not{}\r\na=ice-pwd:0123456789012345678901\r\n{}\r\n", session.ufrag, session.line);
    assert_eq!(server.trickle(&id, &restart).await, Err(ResourceError::IceRestart));
    server.trickle(&id, &session.fragment()).await.unwrap();
    // A fragment that names no fragment at all is a trickle of the same session.
    server.trickle(&id, &session.line).await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn trickled_candidates_are_bounded_per_request_and_per_session() {
    let server = server(4, Duration::from_secs(10), WAIT).await;
    let session = offer_without_candidates(&server, camera()).await.unwrap();
    let id = session.answered.resource.clone();
    let line = |i: usize| format!("a=candidate:{i} 1 udp 1686052607 198.51.100.{} 5000 typ host\r\n", i % 250 + 1);
    let batch = |from: usize, count: usize| (from..from + count).map(line).collect::<String>();

    let over = batch(0, MAX_CANDIDATES_PER_PATCH + 1);
    assert!(matches!(server.trickle(&id, &over).await, Err(ResourceError::BadFragment(_))));
    for round in 0..MAX_TRICKLED_CANDIDATES / MAX_CANDIDATES_PER_PATCH {
        let body = batch(round * MAX_CANDIDATES_PER_PATCH, MAX_CANDIDATES_PER_PATCH);
        server.trickle(&id, &body).await.unwrap();
    }
    let one_more = batch(100, 1);
    assert!(matches!(server.trickle(&id, &one_more).await, Err(ResourceError::BadFragment(_))), "the session's bound");
    // Candidates that cannot be read are not taken, so they do not count.
    server.trickle(&id, "a=candidate:9 1 udp 1 0a1b2c3d-1234.local 5000 typ host\r\n").await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_delete_gives_back_the_callers_place() {
    // One session per caller address: a viewer that leaves with a DELETE can
    // come back at once, one that has not left cannot have two.
    let config = WebRtcConfig {
        listen: "127.0.0.1:0".parse().unwrap(),
        max_sessions: 4,
        max_sessions_per_ip: 1,
        tcp: false,
        ..WebRtcConfig::default()
    };
    let server = WebRtcServer::bind(config, CancellationToken::new()).await.unwrap();
    let stream = camera();
    let first = offer_without_candidates(&server, stream.clone()).await.unwrap();
    let refused = offer_without_candidates(&server, stream.clone()).await;
    assert!(matches!(refused, Err(WhepError::TooManySessions)), "the address holds its one session");

    server.close(&first.answered.resource).await.unwrap();
    let again = offer_without_candidates(&server, stream).await;
    assert!(again.is_ok(), "the place came back with the DELETE, not after the idle timeout");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_delete_frees_a_viewer_that_never_connected() {
    // The case go2rtc leaves for about 15 s: an answered offer whose peer never
    // sent a check (a tab closed during ICE gathering).
    let stream = camera();
    let server = server(4, Duration::from_secs(60), WAIT).await;
    let session = offer_without_candidates(&server, stream.clone()).await.unwrap();
    assert_eq!(stream.snapshot().consumers.len(), 1, "listed from the answer on");
    let started = Instant::now();
    server.close(&session.answered.resource).await.unwrap();
    assert!(started.elapsed() < Duration::from_secs(1), "not the idle timeout, not the close wait");
    assert!(stream.snapshot().consumers.is_empty());
    assert_eq!(server.session_count(), 0);
}
