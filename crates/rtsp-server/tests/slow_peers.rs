//! Peers that cost the server time: a request trickled byte by byte, and a
//! connect flood over the total limit (M3).
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing, clippy::panic)]

mod common;

use std::io::Write;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use common::*;
use oxrtc_rtsp_server::RtspServerConfig;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// The silence timeout restarts at every byte: a peer sending one byte a
/// second never trips it, and before the request deadline held its
/// connection until the size limit (64 KiB, about 18 hours at that pace).
#[tokio::test]
async fn a_request_trickled_one_byte_per_second_is_cut_at_its_deadline() {
    let hub = hub(Arc::new(Factory::default()));
    let config = RtspServerConfig {
        request_timeout: Duration::from_secs(2),
        session_timeout: Duration::from_secs(60),
        ..config()
    };
    let server = start(hub, config).await;
    let mut c = Client::connect(server.addr).await;
    let request = b"OPTIONS * RTSP/1.0\r\nCSeq: 1\r\n\r\n";
    let started = tokio::time::Instant::now();
    let mut answer = Vec::new();
    let mut closed = false;
    for byte in request {
        if c.stream.write_all(&[*byte]).await.is_err() {
            closed = true;
            break;
        }
        // One second per byte, reading whatever the server says meanwhile.
        let mut chunk = [0u8; 1024];
        match tokio::time::timeout(Duration::from_secs(1), c.stream.read(&mut chunk)).await {
            Err(_) => {}
            Ok(Ok(0) | Err(_)) => {
                closed = true;
                break;
            }
            Ok(Ok(n)) => answer.extend_from_slice(&chunk[..n]),
        }
    }
    let elapsed = started.elapsed();
    assert!(closed || c.closed_within(Duration::from_secs(2)).await, "the trickle kept its connection");
    let answer = String::from_utf8_lossy(&answer);
    assert!(answer.starts_with("RTSP/1.0 408"), "answered {answer:?}");
    assert!(elapsed < Duration::from_secs(6), "cut after {elapsed:?}, the deadline is 2 s");
}

/// The deadline runs from a message's first byte: the silence between two
/// requests, or the time to answer the first, is not counted against the
/// second.
#[tokio::test]
async fn the_deadline_counts_each_request_alone() {
    let hub = hub(Arc::new(Factory::default()));
    let config = RtspServerConfig { request_timeout: Duration::from_millis(500), ..config() };
    let server = start(hub, config).await;
    let mut c = Client::connect(server.addr).await;
    assert_eq!(c.request("OPTIONS", "*", &[], "").await.unwrap().status, 200);
    tokio::time::sleep(Duration::from_millis(1200)).await;
    assert_eq!(c.request("OPTIONS", "*", &[], "").await.unwrap().status, 200, "idle time counted");
    // A request in two halves, well within its deadline.
    c.stream.write_all(b"OPTIONS * RTSP/1.0\r\n").await.unwrap();
    tokio::time::sleep(Duration::from_millis(200)).await;
    c.stream.write_all(b"CSeq: 9\r\n\r\n").await.unwrap();
    let mut chunk = [0u8; 1024];
    let n = tokio::time::timeout(Duration::from_secs(2), c.stream.read(&mut chunk)).await.unwrap().unwrap();
    assert!(String::from_utf8_lossy(&chunk[..n]).starts_with("RTSP/1.0 200"));
}

/// Captures every log line written while the guard lives (current thread).
#[derive(Clone, Default)]
struct Logs(Arc<Mutex<Vec<u8>>>);

impl Logs {
    fn capture() -> (Self, tracing::subscriber::DefaultGuard) {
        let logs = Self::default();
        let writer = logs.clone();
        let subscriber = tracing_subscriber::fmt().with_ansi(false).with_writer(move || writer.clone()).finish();
        let guard = tracing::subscriber::set_default(subscriber);
        // A callsite another test thread registered first, with no
        // subscriber of its own, must not stay disabled for this one (the
        // same as oxrtc-core's channel tests).
        tracing::callsite::rebuild_interest_cache();
        (logs, guard)
    }

    fn lines(&self, needle: &str) -> Vec<String> {
        let text = String::from_utf8_lossy(&self.0.lock().unwrap()).into_owned();
        text.lines().filter(|l| l.contains(needle)).map(str::to_owned).collect()
    }
}

impl Write for Logs {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// The `refused=` count of a warning line.
fn refused(line: &str) -> u64 {
    let start = line.find("refused=").unwrap() + "refused=".len();
    line[start..].split(|c: char| !c.is_ascii_digit()).next().unwrap().parse().unwrap()
}

/// Like the per-address refusals, the total limit warns at most once per
/// interval, with how many connections it refused: it warned once per
/// connection before.
#[tokio::test]
async fn the_total_limit_warns_once_per_interval_with_a_count() {
    let (logs, _guard) = Logs::capture();
    let hub = hub(Arc::new(Factory::default()));
    let config = RtspServerConfig { max_connections: 1, ..config() };
    let server = start(hub, config).await;
    let mut held = Client::connect(server.addr).await;
    assert_eq!(held.request("OPTIONS", "*", &[], "").await.unwrap().status, 200);
    let flood = 20;
    for _ in 0..flood {
        let mut c = Client::connect(server.addr).await;
        assert!(c.closed_within(Duration::from_secs(2)).await, "a connection over the limit was served");
    }
    let lines = logs.lines("rtsp: connection limit reached");
    assert!(lines.len() <= 2, "{} warnings for {flood} refusals: {lines:#?}", lines.len());
    assert_eq!(lines.first().map(|l| refused(l)), Some(1), "the first refusal is reported at once");
    assert!(lines[0].contains("limit=1"), "{}", lines[0]);
    // One interval later, the next refusal reports the ones held back.
    tokio::time::sleep(oxrtc_core::refusals::REPORT_EVERY + Duration::from_millis(100)).await;
    let mut c = Client::connect(server.addr).await;
    assert!(c.closed_within(Duration::from_secs(2)).await);
    let lines = logs.lines("rtsp: connection limit reached");
    let total: u64 = lines.iter().map(|l| refused(l)).sum();
    assert_eq!(total, flood + 1, "every refusal is counted once: {lines:#?}");
}
