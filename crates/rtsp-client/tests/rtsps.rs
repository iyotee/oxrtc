//! `rtsps://` cameras: RTSP over TLS, against a fake camera with a certificate
//! of its own. A camera of this kind is what openIVR's TLS policy promotes a
//! plain `rtsp://` camera to, with no option on the URL.
// Test code: a failed expectation is a panic by design.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]

mod common;

use std::time::Duration;

use common::camera::{FakeCamera, Script, demand, header};
use common::tls::identity;
use oxrtc_core::{Codec, MediaKind, SourceState, SpecError};
use oxrtc_rtsp_client::RtspClientConfig;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

const WAIT: Duration = Duration::from_secs(10);

async fn frames(stream: &std::sync::Arc<oxrtc_core::Stream>, at_least: u64) {
    common::wait_state(stream, WAIT, |_| stream.snapshot().sources[0].frames >= at_least).await;
}

/// The camera's certificate is its own and nobody's authority has signed it:
/// the source plays all the same, as it does on go2rtc and as openIVR's probe
/// (which checks nothing either) found it (D20).
#[tokio::test]
async fn a_camera_with_a_self_signed_certificate_plays_by_default() {
    let (_, acceptor) = identity(&["camera.example"]);
    let camera = FakeCamera::start_tls(Script::PlayThenFrames, acceptor).await;
    let hub = common::hub(vec![]);
    let stream = hub.upsert("cam", &[camera.tls_url("admin:hunter2@", "#timeout=30")]).unwrap();
    let viewer = demand(&stream);
    frames(&stream, 2).await;
    let tracks = stream.snapshot().sources[0].tracks.clone();
    viewer.abort();
    assert_eq!((tracks[0].kind, tracks[0].codec), (MediaKind::Video, Codec::H264), "{tracks:?}");

    // What crossed the connection is RTSP in full: the request line names the
    // rtsps URL, without the user information, and the media was set up over TCP.
    let requests = camera.requests();
    let describe = requests.iter().find(|r| r.starts_with("DESCRIBE ")).expect("a DESCRIBE");
    assert!(describe.starts_with(&format!("DESCRIBE rtsps://127.0.0.1:{}/cam ", camera.port)), "{describe}");
    assert!(requests.iter().all(|r| !r.contains("hunter2") && !r.contains("admin@")), "{requests:?}");
    let setups: Vec<&String> = requests.iter().filter(|r| r.starts_with("SETUP ")).collect();
    assert!(!setups.is_empty());
    assert!(setups.iter().all(|s| header(s, "Transport").is_some_and(|t| t.contains("RTP/AVP/TCP"))), "{setups:?}");
}

/// A promoted camera is always one with a password: the challenge is answered
/// inside the TLS connection, the password reaches the camera decoded and in
/// that header only, never in a request line (the camera here refuses it, so
/// the whole exchange is the 401 and what the client answered).
#[tokio::test]
async fn the_password_of_an_rtsps_camera_travels_in_the_header_only() {
    let (_, acceptor) = identity(&["camera.example"]);
    let camera = FakeCamera::start_tls(Script::Unauthorized, acceptor).await;
    let hub = common::hub(vec![]);
    let stream = hub.upsert("cam", &[camera.tls_url("admin:p%40ss@", "#timeout=30")]).unwrap();
    let viewer = demand(&stream);
    let error = common::retry_error(&stream, WAIT).await;
    viewer.abort();
    assert_eq!(error, "authentication refused");
    let requests = camera.requests();
    assert!(
        requests.iter().all(|r| r.starts_with("DESCRIBE rtsps://127.0.0.1:") || r.starts_with("OPTIONS ")),
        "{requests:?}"
    );
    assert!(
        requests.iter().all(|r| !r.contains("p@ss") && !r.contains("p%40ss") && !r.contains("admin:")),
        "{requests:?}"
    );
    // Basic base64("admin:p@ss").
    assert!(requests.iter().any(|r| header(r, "Authorization") == Some("Basic YWRtaW46cEBzcw==")), "{requests:?}");
}

/// `sources[].error` is what an operator reads: the TLS reason is in it, the
/// password is not, and nothing was asked of a camera that was not verified.
#[tokio::test]
async fn verify_refuses_a_certificate_nobody_trusts_and_says_why() {
    let (_, acceptor) = identity(&["127.0.0.1"]);
    let camera = FakeCamera::start_tls(Script::PlayThenFrames, acceptor).await;
    let hub = common::hub(vec![]);
    let stream = hub.upsert("cam", &[camera.tls_url("admin:hunter2@", "#verify#timeout=30")]).unwrap();
    let viewer = demand(&stream);
    let error = common::retry_error(&stream, WAIT).await;
    viewer.abort();
    assert!(error.starts_with("unreachable: Unable to connect to RTSP server: TLS: "), "{error}");
    assert!(error.contains("UnknownIssuer"), "{error}");
    assert!(!error.contains("hunter2"), "{error}");
    assert!(camera.requests().is_empty(), "a request went out over an unverified connection");
}

#[tokio::test]
async fn verify_accepts_a_certificate_the_configuration_trusts_for_the_host() {
    let (cert, acceptor) = identity(&["127.0.0.1"]);
    let camera = FakeCamera::start_tls(Script::PlayThenFrames, acceptor).await;
    let hub = common::hub_with(RtspClientConfig { extra_roots: vec![cert.to_vec()], ..RtspClientConfig::default() });
    let stream = hub.upsert("cam", &[camera.tls_url("", "#verify#timeout=30")]).unwrap();
    let viewer = demand(&stream);
    frames(&stream, 2).await;
    viewer.abort();
}

/// A trusted certificate is still the certificate of the host the URL names.
#[tokio::test]
async fn verify_refuses_a_trusted_certificate_made_for_another_host() {
    let (cert, acceptor) = identity(&["camera.example"]);
    let camera = FakeCamera::start_tls(Script::PlayThenFrames, acceptor).await;
    let hub = common::hub_with(RtspClientConfig { extra_roots: vec![cert.to_vec()], ..RtspClientConfig::default() });
    let stream = hub.upsert("cam", &[camera.tls_url("", "#verify#timeout=30")]).unwrap();
    let viewer = demand(&stream);
    let error = common::retry_error(&stream, WAIT).await;
    viewer.abort();
    assert!(error.contains("TLS") && error.contains("not valid for name"), "{error}");
    assert!(camera.requests().is_empty());
}

/// A camera whose `rtsps` port speaks plain RTSP answers the ClientHello with
/// a refusal in RTSP: the operator reads that it was not TLS, not a timeout.
#[tokio::test]
async fn a_camera_that_does_not_speak_tls_is_said_so() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let plain = tokio::spawn(async move {
        while let Ok((mut socket, _)) = listener.accept().await {
            let mut hello = [0u8; 512];
            let _ = socket.read(&mut hello).await;
            let _ = socket
                .write_all(
                    b"RTSP/1.0 400 Bad Request
CSeq: 0

",
                )
                .await;
        }
    });
    let hub = common::hub(vec![]);
    let stream = hub.upsert("cam", &[format!("rtsps://127.0.0.1:{port}/cam#timeout=30")]).unwrap();
    let viewer = demand(&stream);
    let error = common::retry_error(&stream, WAIT).await;
    viewer.abort();
    plain.abort();
    assert!(error.starts_with("unreachable: Unable to connect to RTSP server: TLS: "), "{error}");
}

/// A camera that drops the connection (it rebooted) is said so in words, not
/// in rustls's, which end with the link of a documentation page.
#[tokio::test]
async fn a_camera_that_drops_the_connection_is_said_to_have_done_so() {
    let (_, acceptor) = identity(&["camera.example"]);
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let camera = tokio::spawn(async move {
        while let Ok((socket, _)) = listener.accept().await {
            // The handshake, then nothing: the connection is dropped, as a camera's is.
            drop(acceptor.accept(socket).await);
        }
    });
    let hub = common::hub(vec![]);
    let stream = hub.upsert("cam", &[format!("rtsps://127.0.0.1:{port}/cam#timeout=30")]).unwrap();
    let viewer = demand(&stream);
    let error = common::retry_error(&stream, WAIT).await;
    viewer.abort();
    camera.abort();
    assert!(error.contains("the camera closed the TLS connection without ending it"), "{error}");
    assert!(!error.contains("docs.rs"), "{error}");
}

/// A camera that accepts the connection and never answers the handshake is
/// reported as that, not as a DESCRIBE nobody answered.
#[tokio::test]
async fn a_handshake_that_never_ends_is_reported_as_one() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let held = tokio::spawn(async move {
        let mut sockets = Vec::new();
        while let Ok((socket, _)) = listener.accept().await {
            sockets.push(socket);
        }
    });
    let hub = common::hub(vec![]);
    let stream = hub.upsert("cam", &[format!("rtsps://127.0.0.1:{port}/cam#timeout=2")]).unwrap();
    let viewer = demand(&stream);
    let error = common::retry_error(&stream, WAIT).await;
    viewer.abort();
    held.abort();
    assert_eq!(error, "unreachable: Unable to connect to RTSP server: the TLS handshake took over 1s");
}

/// retina's TEARDOWN goes down the connection it has, here the TLS one.
#[tokio::test]
async fn the_last_viewer_leaving_tears_the_session_down_over_tls() {
    let (_, acceptor) = identity(&["camera.example"]);
    let camera = FakeCamera::start_tls(Script::PlayThenSilence, acceptor).await;
    let hub = common::hub(vec![]);
    let stream = hub.upsert("cam", &[camera.tls_url("", "#timeout=30")]).unwrap();
    let viewer = demand(&stream);
    common::wait_state(&stream, WAIT, |_| !stream.snapshot().sources[0].tracks.is_empty()).await;
    viewer.abort();
    common::wait_state(&stream, WAIT, |s| *s == SourceState::Idle).await;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while !camera.requests().iter().any(|r| r.starts_with("TEARDOWN rtsps://")) {
        assert!(tokio::time::Instant::now() < deadline, "no TEARDOWN: {:?}", camera.requests());
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[tokio::test]
async fn the_flags_are_checked_when_the_stream_is_created() {
    let hub = common::hub(vec![]);
    for (source, ok) in [
        ("rtsps://cam.example/live", true),
        ("rtsps://cam.example/live#verify", true),
        ("rtsps://cam.example/live#insecure", true),
        ("rtsps://cam.example/live#verify=0#insecure", true),
        ("rtsps://cam.example/live#verify#insecure", false),
        ("rtsp://cam.example/live#verify", false),
        ("rtsp://cam.example/live#insecure", true),
    ] {
        let made = hub.upsert("cam", &[source]);
        assert_eq!(made.is_ok(), ok, "{source}: {:?}", made.err());
        if !ok {
            let Err(oxrtc_core::HubError::Spec { error, .. }) = made else { panic!("{source}") };
            assert!(matches!(error, SpecError::Invalid(_)), "{source}: {error:?}");
        }
    }
}
