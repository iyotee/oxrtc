//! An API at its total connection bound (M3). Its own test binary: the log
//! line under test is also written by `serve.rs`'s limit tests, and a log
//! capture shares tracing's per-callsite interest with every thread of its
//! process.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing, clippy::panic)]

mod common;

use std::sync::Arc;
use std::time::Duration;

use common::*;
use oxrtc_api::{ApiState, ServeLimits};
use oxrtc_core::CancellationToken;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

/// The total does not refuse: a connection over it waits in the accept
/// queue. It used to wait in silence, so a saturated API showed nothing in
/// the log; reaching the bound is now reported like the per-address
/// refusals, at most once per interval with a count.
#[tokio::test]
async fn reaching_the_total_is_reported_once_per_interval() {
    let (logs, _guard) = Logs::capture();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let cancel = CancellationToken::new();
    let router = oxrtc_api::router(ApiState::new(hub(Arc::new(FakeFactory::default())), "v", auth()));
    let limits = ServeLimits { max_connections: 1, ..ServeLimits::default() };
    tokio::spawn(oxrtc_api::serve_with_limits(listener, router, cancel.clone(), limits));
    let request =
        format!("GET /api HTTP/1.1\r\nHost: x\r\nAuthorization: {}\r\nConnection: close\r\n\r\n", basic(USER, PASS));
    let lines = || -> Vec<String> {
        logs.text().lines().filter(|l| l.contains("api: connection limit reached")).map(str::to_owned).collect()
    };
    // Five connections one after the other: each takes the only slot.
    for _ in 0..5 {
        let mut held = TcpStream::connect(addr).await.unwrap();
        held.write_all(request.as_bytes()).await.unwrap();
        let mut answer = Vec::new();
        tokio::time::timeout(Duration::from_secs(5), held.read_to_end(&mut answer)).await.unwrap().unwrap();
        assert!(answer.starts_with(b"HTTP/1.1 200"), "{}", String::from_utf8_lossy(&answer));
    }
    let reported = lines();
    assert!(!reported.is_empty(), "the bound was reached five times, and the log says nothing");
    assert!(reported.len() <= 2, "{} warnings within one interval: {reported:#?}", reported.len());
    assert!(reported[0].contains("limit=1") && reported[0].contains("reached=1"), "{}", reported[0]);
    // One interval later, the next time the bound is reached reports the
    // ones held back.
    tokio::time::sleep(oxrtc_core::refusals::REPORT_EVERY + Duration::from_millis(100)).await;
    let mut last = TcpStream::connect(addr).await.unwrap();
    last.write_all(request.as_bytes()).await.unwrap();
    let mut answer = Vec::new();
    tokio::time::timeout(Duration::from_secs(5), last.read_to_end(&mut answer)).await.unwrap().unwrap();
    assert!(eventually(Duration::from_secs(2), || lines().len() > reported.len()).await, "{:#?}", lines());
    cancel.cancel();
}
