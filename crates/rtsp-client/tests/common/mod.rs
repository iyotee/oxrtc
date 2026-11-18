//! Helpers shared by the integration tests.
#![allow(dead_code, missing_docs, clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]

pub mod camera;
pub mod tls;

use std::sync::Arc;
use std::time::Duration;

use oxrtc_core::{Drivers, Hub, Limits, SourceState, Stream};
use oxrtc_rtsp_client::{RtspClientConfig, RtspFactory};

/// Ports the owner's NVR and benches use on this machine: never bound, never named.
pub const FORBIDDEN_PORTS: [u16; 12] = [1984, 8554, 8555, 7998, 7999, 8000, 11984, 18554, 18555, 18998, 21984, 28554];

/// A hub with the RTSP factory, quick retries and a short idle grace.
pub fn hub(loopback_ports: Vec<u16>) -> Arc<Hub> {
    hub_with(RtspClientConfig { loopback_ports, ..RtspClientConfig::default() })
}

/// The same hub, with this client configuration.
pub fn hub_with(config: RtspClientConfig) -> Arc<Hub> {
    hub_built(config, limits())
}

/// The same hub, taking no frame over `max_frame_bytes`.
pub fn hub_capped(max_frame_bytes: usize) -> Arc<Hub> {
    hub_built(RtspClientConfig::default(), Limits { max_frame_bytes, ..limits() })
}

fn limits() -> Limits {
    Limits {
        probe_timeout: Duration::from_secs(20),
        connect_timeout: Duration::from_secs(20),
        stall_timeout: Duration::from_secs(20),
        idle_grace: Duration::from_millis(100),
        backoff_initial: Duration::from_millis(200),
        backoff_max: Duration::from_secs(1),
        ..Limits::default()
    }
}

fn hub_built(config: RtspClientConfig, limits: Limits) -> Arc<Hub> {
    let mut drivers = Drivers::new();
    drivers.register(Arc::new(RtspFactory::new(config)));
    Hub::new(drivers, limits)
}

/// Where the tests draw their ports: below the ones a system gives a socket
/// bound to port 0 or a connection's own end (32768 and up on Linux, 49152 and
/// up on Windows), so that a port drawn here stays closed, or free for the
/// program it was drawn for, whatever the other tests open meanwhile (the
/// same draw as `apps/oxrtc/tests/common`, which says what a port taken from
/// a bind to port 0 cost); clear of [`FORBIDDEN_PORTS`].
const DRAWN_PORTS: std::ops::Range<u16> = 22000..28000;

/// The ports this process drew: two tests of one binary never get the same.
static DRAWN: std::sync::Mutex<std::collections::BTreeSet<u16>> =
    std::sync::Mutex::new(std::collections::BTreeSet::new());

/// A port of [`DRAWN_PORTS`] nobody listens on right now, given to no other
/// test of this process.
pub fn free_port() -> u16 {
    use std::hash::{BuildHasher, Hasher};
    for _ in 0..500 {
        // The standard library's per-instance hash seed: random enough here.
        let draw = std::collections::hash_map::RandomState::new().build_hasher().finish();
        let port = DRAWN_PORTS.start + u16::try_from(draw % u64::from(DRAWN_PORTS.end - DRAWN_PORTS.start)).unwrap();
        let free = std::net::TcpListener::bind(("127.0.0.1", port)).is_ok();
        if free && !FORBIDDEN_PORTS.contains(&port) && DRAWN.lock().unwrap().insert(port) {
            return port;
        }
    }
    panic!("no free port in 500 draws of {DRAWN_PORTS:?}");
}

/// Polls the state of the stream's first source until `pred` holds.
pub async fn wait_state(stream: &Arc<Stream>, within: Duration, pred: impl Fn(&SourceState) -> bool) -> SourceState {
    let deadline = tokio::time::Instant::now() + within;
    loop {
        let state = stream.snapshot().sources[0].state.clone();
        if pred(&state) {
            return state;
        }
        assert!(tokio::time::Instant::now() < deadline, "state still {state:?} after {within:?}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// Waits until the first source is retrying, and returns its error text.
pub async fn retry_error(stream: &Arc<Stream>, within: Duration) -> String {
    match wait_state(stream, within, |s| matches!(s, SourceState::Retrying { .. })).await {
        SourceState::Retrying { error, .. } => error,
        other => panic!("not retrying: {other:?}"),
    }
}
