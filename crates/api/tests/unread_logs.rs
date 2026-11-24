//! A value `frame.jpeg` or `stream.mp4` cannot read is named in the log, once
//! per interval and route, never once per request (D2). Alone in its binary: a
//! log capture races with the callsites of tests that run beside it.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing, clippy::panic)]

mod common;

use axum::http::{Method, StatusCode};
use bytes::Bytes;
use common::*;
use oxrtc_core::Codec;

#[tokio::test]
async fn each_bad_value_gives_the_picture_and_one_warning() {
    let (logs, _guard) = Logs::capture();
    let snapshotter = FakeSnapshotter::new(Ok(Bytes::from_static(b"\xff\xd8jpeg\xff\xd9")));
    let api = Api::build(|s| s.with_snapshots(snapshotter.clone()));
    api.factory.add("cam1", Script::video(Codec::H264, 0));
    api.call(Method::PUT, "/api/streams?name=cam&src=fake:cam1").await;
    for _ in 0..20 {
        let reply = api.call(Method::GET, "/api/frame.jpeg?src=cam&rotate=45&width=0&cache=abc").await;
        assert_eq!(reply.status, StatusCode::OK);
    }
    let text = logs.text();
    let lines: Vec<&str> = text.lines().filter(|l| l.contains("frame.jpeg: a value that cannot be read")).collect();
    assert_eq!(lines.len(), 1, "one warning for twenty requests: {text}");
    let line = lines[0];
    assert!(line.contains("WARN"), "{line}");
    for named in [r#"rotate="45""#, r#"width="0""#, r#"cache="abc""#, "stream=cam"] {
        assert!(line.contains(named), "{named} in {line}");
    }
    // A value that can be read is no warning; another route has its own.
    api.call(Method::GET, "/api/frame.jpeg?src=cam&rotate=90&cache=10s").await;
    assert_eq!(logs.text().matches("a value that cannot be read").count(), 1);
}
