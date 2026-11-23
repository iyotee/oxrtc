//! The output memory budget through the API (D45): `GET /api` shows it, and
//! a `stream.mp4` reader draws from it while its response is open; the
//! `api.output_memory` key as the configuration file writes it.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing, clippy::panic)]

mod common;

use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use common::*;
use futures_util::StreamExt;
use oxrtc_api::ApiState;
use oxrtc_api::config::{OutputMemory, parse};
use oxrtc_core::host_memory::{Fixed, HostReading, MemorySource};
use oxrtc_core::memory::{BudgetSize, MemoryBudget};
use oxrtc_core::{Codec, CodecParams, TrackInfo};
use oxrtc_mp4::fuzzing::{H264_PPS, H264_SPS, hex};
use tower::ServiceExt;

const MIB: u64 = 1024 * 1024;

fn get(uri: &str) -> Request<Body> {
    Request::get(uri).header(header::AUTHORIZATION, basic(USER, PASS)).body(Body::empty()).unwrap()
}

async fn api_json(router: &axum::Router) -> serde_json::Value {
    let response = router.clone().oneshot(get("/api")).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), 1 << 16).await.unwrap();
    serde_json::from_slice(&body).unwrap()
}

#[tokio::test]
async fn get_api_shows_the_budget_and_a_stream_mp4_reader_draws_from_it() {
    let factory = Arc::new(FakeFactory::default());
    let h264 = TrackInfo::video(Codec::H264).with_params(CodecParams::H264 { sps: hex(H264_SPS), pps: hex(H264_PPS) });
    factory.add("cam", Script::tracks(vec![h264], 100_000));
    let hub = hub(factory);
    hub.upsert("cam", &["fake:cam"]).unwrap();
    let reading = HostReading { limit: 8 << 30, available: 6 << 30, source: MemorySource::CgroupV2 };
    assert!(hub.set_output_memory(MemoryBudget::new(BudgetSize::Bytes(64 * MIB), Box::new(Fixed(reading)))));
    let router = oxrtc_api::router(ApiState::new(hub.clone(), "1.9.15", auth()));
    let shown = api_json(&router).await;
    assert_eq!(shown["version"], "1.9.15");
    let memory = &shown["output_memory"];
    assert_eq!(memory["budget"], 64 * MIB);
    assert_eq!((memory["readers"].as_u64(), memory["skips"].as_u64()), (Some(0), Some(0)));
    assert_eq!(memory["limit"], 8u64 << 30);
    assert_eq!(memory["source"], "cgroup v2 memory.max");
    // A response being read is one reader of the budget, gone with it.
    let response = router.clone().oneshot(get("/api/stream.mp4?src=cam")).await.unwrap();
    let mut body = response.into_body().into_data_stream();
    let _ = tokio::time::timeout(Duration::from_secs(5), body.next()).await.unwrap();
    assert_eq!(api_json(&router).await["output_memory"]["readers"], 1);
    drop(body);
    assert!(eventually(Duration::from_secs(2), || hub.output_memory().unwrap().snapshot().readers == 0).await);
}

#[test]
fn the_key_takes_a_share_or_a_size() {
    let config = |value: &str| format!("api:\n  listen: \"127.0.0.1:0\"\n  output_memory: {value}\n");
    assert_eq!(parse(&config("\"25%\"")).unwrap().0.api.output_memory, Some(OutputMemory::PerMille(250)));
    assert_eq!(parse(&config("512")).unwrap().0.api.output_memory, Some(OutputMemory::Mib(512)));
    assert_eq!(parse("api:\n  listen: \"127.0.0.1:0\"\n").unwrap().0.api.output_memory, None);
    let refused = parse(&config("\"lots\"")).unwrap_err().to_string();
    assert!(refused.contains("api.output_memory: expected a share of memory"), "{refused}");
}
