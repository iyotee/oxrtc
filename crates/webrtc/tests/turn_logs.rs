//! TURN credentials never reach the log (SEC-005): every event and span at
//! every level, from this crate and from str0m, captured while one relay
//! refuses its credentials and another relays a session, over UDP and over TCP.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing, clippy::panic)]

mod common;

use std::fmt::Write as _;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use common::paths::connect_relay_only;
use common::turn_server::TurnServer;
use common::*;
use oxrtc_core::{CancellationToken, Codec};
use oxrtc_webrtc::{IceServer, WebRtcConfig, WebRtcServer};
use tracing::field::{Field, Visit};
use tracing::span::{Attributes, Id, Record};
use tracing::{Event, Metadata, Subscriber};

const USERNAME: &str = "1700000000:secret-site";
const PASSWORD: &str = "pa55-w0rd-credential";
const WRONG: &str = "not-the-credential";

#[derive(Clone, Default)]
struct Capture(Arc<Mutex<String>>);

struct Visitor<'a>(&'a mut String);

impl Visit for Visitor<'_> {
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        let _ = write!(self.0, "{}={value:?} ", field.name());
    }
}

impl Subscriber for Capture {
    fn enabled(&self, _: &Metadata<'_>) -> bool {
        true
    }
    fn new_span(&self, span: &Attributes<'_>) -> Id {
        span.record(&mut Visitor(&mut self.0.lock().unwrap()));
        Id::from_u64(1)
    }
    fn record(&self, _: &Id, values: &Record<'_>) {
        values.record(&mut Visitor(&mut self.0.lock().unwrap()));
    }
    fn record_follows_from(&self, _: &Id, _: &Id) {}
    fn event(&self, event: &Event<'_>) {
        let mut log = self.0.lock().unwrap();
        log.push_str(event.metadata().target());
        log.push(' ');
        event.record(&mut Visitor(&mut log));
        log.push('\n');
    }
    fn enter(&self, _: &Id) {}
    fn exit(&self, _: &Id) {}
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn turn_credentials_never_reach_the_log() {
    let capture = Capture::default();
    tracing::subscriber::set_global_default(capture.clone()).unwrap();

    let good = TurnServer::start(USERNAME, PASSWORD).await;
    let refusing = TurnServer::start(USERNAME, PASSWORD).await;
    let entry = |server: &TurnServer, credential: &str| IceServer {
        // Both transports, as openIVR writes them: the TCP connection's
        // lines (connect, allocate, refuse) must not show the credential either.
        urls: vec![format!("turn:{}?transport=udp", server.addr), format!("turn:{}?transport=tcp", server.tcp_addr)],
        username: Some(USERNAME.into()),
        credential: Some(credential.into()),
    };
    let config = WebRtcConfig {
        listen: "127.0.0.1:0".parse().unwrap(),
        ice_servers: vec![entry(&refusing, WRONG), entry(&good, PASSWORD)],
        tcp: false,
        ..WebRtcConfig::default()
    };
    // The configuration itself, as the binary would print it.
    tracing::info!(?config, "configured");
    let server = WebRtcServer::bind(config, CancellationToken::new()).await.unwrap();
    let (params, video) = synthetic_h264();
    let hub = hub(FakeFactory::default().with("cam", av_script(Codec::H264, params, video, false)));
    let stream = hub.upsert("cam", &["fake:cam"]).unwrap();
    let (mut client, _) = connect_relay_only(&server, stream, browser(false)).await.unwrap();
    assert_eq!(
        client.wait_for(Duration::from_secs(10), |e| *e == ClientEvent::Connected).await,
        Some(ClientEvent::Connected)
    );
    assert!(client.wait_for(Duration::from_secs(10), |e| matches!(e, ClientEvent::Media { .. })).await.is_some());
    drop(server);
    tokio::time::sleep(Duration::from_millis(100)).await;

    let log = capture.0.lock().unwrap().clone();
    assert!(log.contains("turn allocated"), "the capture works: {} bytes", log.len());
    assert!(log.contains("transport=tcp"), "a TCP relay was named in the log");
    assert!(good.stats.tcp_allocations.load(std::sync::atomic::Ordering::Relaxed) >= 1);
    assert!(refusing.stats.tcp_connections.load(std::sync::atomic::Ordering::Relaxed) >= 1);
    assert!(log.contains("credentials refused"), "the refusal is logged");
    for secret in [USERNAME, PASSWORD, WRONG, "secret-site"] {
        assert!(!log.contains(secret), "{secret:?} reached the log");
    }
}
