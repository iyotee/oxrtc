//! Helpers shared by the integration tests.
#![allow(dead_code, missing_docs, clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]

use std::sync::Arc;
use std::time::Duration;

use oxrtc_core::{Drivers, Hub, Limits, SourceState, Stream};
use oxrtc_rtsp_client::{RtspClientConfig, RtspFactory};

/// Ports the owner's NVR and benches use on this machine: never bound, never named.
pub const FORBIDDEN_PORTS: [u16; 12] = [1984, 8554, 8555, 7998, 7999, 8000, 11984, 18554, 18555, 18998, 21984, 28554];

/// A hub with the RTSP factory, quick retries and a short idle grace.
pub fn hub(loopback_ports: Vec<u16>) -> Arc<Hub> {
    let mut drivers = Drivers::new();
    drivers.register(Arc::new(RtspFactory::new(RtspClientConfig { loopback_ports, ..RtspClientConfig::default() })));
    let limits = Limits {
        probe_timeout: Duration::from_secs(20),
        connect_timeout: Duration::from_secs(20),
        stall_timeout: Duration::from_secs(20),
        idle_grace: Duration::from_millis(100),
        backoff_initial: Duration::from_millis(200),
        backoff_max: Duration::from_secs(1),
        ..Limits::default()
    };
    Hub::new(drivers, limits)
}

/// A port nobody listens on right now (bound to 127.0.0.1:0, then released).
pub fn free_port() -> u16 {
    loop {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        if !FORBIDDEN_PORTS.contains(&port) {
            return port;
        }
    }
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
