//! A viewer retrying a refused offer in a loop: the session limit is
//! reported like the listeners' limits, once per interval with a count (M3).
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing, clippy::panic)]

mod common;

use std::time::Duration;

use axum::http::{Method, StatusCode};
use common::*;
use oxrtc_api::AnswerError;
use oxrtc_core::Codec;

/// The `refused=` count of a warning line.
fn refused(line: &str) -> u64 {
    let start = line.find("refused=").unwrap() + "refused=".len();
    line[start..].split(|c: char| !c.is_ascii_digit()).next().unwrap().parse().unwrap()
}

#[tokio::test]
async fn offers_refused_for_want_of_a_session_warn_once_per_interval() {
    let (logs, _guard) = Logs::capture();
    let api = Api::build(|s| s.with_webrtc(FakeAnswerer::new(Err(AnswerError::Busy))));
    api.factory.add("cam1", Script::video(Codec::H264, 0));
    api.call(Method::PUT, "/api/streams?name=cam&src=fake:cam1").await;
    let auth = basic(USER, PASS);
    let offer = || api.send(Method::POST, "/api/webrtc?src=cam", b"v=0".to_vec(), Some(&auth));
    let flood = 20;
    for _ in 0..flood {
        assert_eq!(offer().await.status, StatusCode::SERVICE_UNAVAILABLE);
    }
    let lines = || -> Vec<String> {
        logs.text().lines().filter(|l| l.contains("api: webrtc failed")).map(str::to_owned).collect()
    };
    let first = lines();
    assert!(first.len() <= 2, "{} warnings for {flood} refusals: {first:#?}", first.len());
    assert_eq!(first.first().map(|l| refused(l)), Some(1), "the first refusal is reported at once");
    tokio::time::sleep(oxrtc_core::refusals::REPORT_EVERY + Duration::from_millis(100)).await;
    assert_eq!(offer().await.status, StatusCode::SERVICE_UNAVAILABLE);
    let total: u64 = lines().iter().map(|l| refused(l)).sum();
    assert_eq!(total, flood + 1, "every refusal is counted once: {:#?}", lines());
}

#[tokio::test]
async fn other_failed_offers_still_warn_each_time() {
    let (logs, _guard) = Logs::capture();
    let api = Api::build(|s| s.with_webrtc(FakeAnswerer::new(Err(AnswerError::NoMatchingCodec))));
    api.factory.add("cam1", Script::video(Codec::H264, 0));
    api.call(Method::PUT, "/api/streams?name=cam&src=fake:cam1").await;
    for _ in 0..3 {
        let reply = api.send(Method::POST, "/api/webrtc?src=cam", b"v=0".to_vec(), Some(&basic(USER, PASS))).await;
        assert_eq!(reply.status, StatusCode::BAD_REQUEST);
    }
    let text = logs.text();
    assert_eq!(text.matches("api: webrtc failed: no matching codec").count(), 3, "{text}");
}
