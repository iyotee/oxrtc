//! go2rtc's `GetOrPatch` on the routes a viewer opens (F7 review): `src` that
//! is no stream's name but a source, with or without `name`, sets the stream
//! as go2rtc 1.9.15 does on `/api/ws`, `/api/frame.jpeg` and
//! `/api/stream.mp4` (measured; its other routes answer `404`) - what the
//! WebRTC Camera card's integration sends (`/api/ws?src=<camera url>&name=
//! <entity>`, `/api/frame.jpeg?src=<poster url>`).
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing, clippy::panic)]

mod common;

use std::sync::Arc;

use axum::http::{Method, StatusCode};
use common::ws::{open_ok, serve};
use common::*;
use oxrtc_api::ServeLimits;
use oxrtc_api::config::Permissions;
use oxrtc_core::{Codec, CodecParams, Hub, TrackInfo};
use oxrtc_mp4::fuzzing::{H264_PPS, H264_SPS, hex};
use serde_json::json;

/// A picture track with its parameter sets, which the MP4 writers need.
fn camera() -> Script {
    let h264 = TrackInfo::video(Codec::H264).with_params(CodecParams::H264 { sps: hex(H264_SPS), pps: hex(H264_PPS) });
    Script::tracks(vec![h264], 10_000)
}

fn sources(hub: &Hub, name: &str) -> Option<Vec<String>> {
    hub.get(name).map(|s| s.specs().map(|spec| spec.expose().to_owned()).collect())
}

fn api(permissions: Permissions) -> Api {
    let api = Api::build(move |s| {
        s.with_permissions(permissions)
            .with_rtsp_listen("127.0.0.1:8554")
            .with_snapshots(FakeSnapshotter::new(Ok(bytes::Bytes::from_static(b"\xff\xd8"))))
    });
    api.factory.add("cam1", camera());
    api.factory.add("cam2", camera());
    api
}

#[tokio::test]
async fn a_source_in_src_sets_the_stream_on_the_routes_go2rtc_sets_it_on() {
    let api = api(Permissions::default());
    // Named: the stream `name`, reading the source.
    let reply = api.call(Method::GET, "/api/frame.jpeg?src=fake:cam1&name=camera.front").await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text());
    assert_eq!(sources(&api.hub, "camera.front"), Some(vec!["fake:cam1".to_owned()]));
    // Unnamed: a stream named after the source (go2rtc names it by the URL,
    // which is no stream name here), the same one for the same source.
    assert_eq!(api.call(Method::GET, "/api/frame.jpeg?src=fake:cam2").await.status, StatusCode::OK);
    assert_eq!(api.call(Method::GET, "/api/frame.jpeg?src=fake:cam2").await.status, StatusCode::OK);
    let named: Vec<String> = api.hub.streams().iter().map(|s| s.name().to_owned()).collect();
    assert_eq!(named.len(), 2, "{named:?}");
    let derived = named.iter().find(|n| *n != "camera.front").unwrap();
    assert!(derived.starts_with("src_"), "{derived}");
    assert_eq!(sources(&api.hub, derived), Some(vec!["fake:cam2".to_owned()]));
    let listed = api.call(Method::GET, "/api/streams").await.json();
    assert_eq!(listed[derived]["producers"][0]["url"], "fake:cam2", "{listed}");
    // stream.mp4 too (its answer ends with `duration`).
    let reply = api.call(Method::GET, "/api/stream.mp4?src=fake:cam1&name=recorded&duration=1").await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text());
    assert_eq!(sources(&api.hub, "recorded"), Some(vec!["fake:cam1".to_owned()]));
    // go2rtc 1.9.15's other routes answer 404 and set nothing (measured).
    for uri in [
        "/api/frame.mp4?src=fake:cam1&name=n1",
        "/api/stream.m3u8?src=fake:cam1&name=n2",
        "/api/stream.mjpeg?src=fake:cam1&name=n3",
        "/api/streams?src=fake:cam1&name=n4",
    ] {
        assert_eq!(api.call(Method::GET, uri).await.status, StatusCode::NOT_FOUND, "{uri}");
    }
    assert_eq!(api.hub.streams().len(), 3);
}

#[tokio::test]
async fn a_source_is_held_to_the_rules_of_a_put() {
    let api = api(Permissions::default());
    for src in ["exec:cmd%20/c%20calc", "echo:x", "nope://x", "file:///etc/passwd", "ffmpeg:-i%20/etc/passwd", "nope"] {
        let reply = api.call(Method::GET, &format!("/api/frame.jpeg?src={src}&name=bad")).await;
        assert_eq!(reply.status, StatusCode::NOT_FOUND, "{src}: {}", reply.text());
    }
    // A name that is no stream name here.
    let reply = api.call(Method::GET, "/api/frame.jpeg?src=fake:cam1&name=a/b").await;
    assert_eq!(reply.status, StatusCode::NOT_FOUND);
    assert!(api.hub.streams().is_empty(), "nothing set");
    // Off with the stream changes.
    let api = self::api(Permissions { stream_changes: false, ..Permissions::default() });
    let reply = api.call(Method::GET, "/api/frame.jpeg?src=fake:cam1&name=camera.front").await;
    assert_eq!(reply.status, StatusCode::NOT_FOUND);
    assert!(api.hub.streams().is_empty());
}

#[tokio::test]
async fn a_link_to_one_of_this_servers_own_streams_reads_that_stream() {
    let api = api(Permissions::default());
    api.hub.upsert("cam", &["fake:cam1"]).unwrap();
    // go2rtc links `rtsp://<host>/<name>` to its stream `name`; here only
    // on this server's RTSP port, so that a camera's own path is never taken
    // for one of this server's streams.
    let reply =
        api.call(Method::GET, "/api/frame.jpeg?src=rtsp%3A%2F%2F127.0.0.1%3A8554%2Fcam%3Fvideo&name=alias").await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text());
    assert_eq!(sources(&api.hub, "alias"), Some(vec!["stream:cam".to_owned()]));
    let reply = api.call(Method::GET, "/api/frame.jpeg?src=rtsp%3A%2F%2Fu%3Ap%40localhost%3A8554%2Fcam").await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(api.hub.streams().len(), 2, "unnamed: the stream itself, nothing set");
    // Another port: a camera's URL like any other (no RTSP driver in this hub).
    let reply = api.call(Method::GET, "/api/frame.jpeg?src=rtsp%3A%2F%2F127.0.0.1%3A554%2Fcam&name=other").await;
    assert_eq!(reply.status, StatusCode::NOT_FOUND);
    assert!(api.hub.get("other").is_none());
}

#[tokio::test]
async fn a_named_stream_that_already_reads_the_source_is_left_as_it_is() {
    let api = api(Permissions::default());
    // Home Assistant's integration sets its camera with a transcode of its
    // own; the WebRTC Camera card then names the camera's source alone.
    let before = api.hub.upsert("camera.f7", &["fake:cam1", "fake:cam2"]).unwrap();
    assert_eq!(api.call(Method::GET, "/api/frame.jpeg?src=fake:cam1&name=camera.f7").await.status, StatusCode::OK);
    let after = api.hub.get("camera.f7").unwrap();
    assert!(Arc::ptr_eq(&before, &after), "the same stream: its readers go on");
    assert_eq!(sources(&api.hub, "camera.f7").unwrap().len(), 2);
    // Another source: the stream reads that one, as go2rtc's patch.
    api.factory.add("cam3", camera());
    assert_eq!(api.call(Method::GET, "/api/frame.jpeg?src=fake:cam3&name=camera.f7").await.status, StatusCode::OK);
    assert_eq!(sources(&api.hub, "camera.f7"), Some(vec!["fake:cam3".to_owned()]));
}

#[tokio::test]
async fn the_websocket_sets_the_stream_its_src_names_as_a_source() {
    let factory = Arc::new(FakeFactory::default());
    factory.add("cam1", camera());
    let hub = hub(factory);
    let state = oxrtc_api::ApiState::new(hub.clone(), "1.9.15", auth());
    let server = serve(state, ServeLimits::default()).await;
    let mut client = open_ok(server.addr, "?src=fake%3Acam1&name=camera.card").await;
    client.send(json!({"type": "mse", "value": "avc1.640029,mp4a.40.2"})).await;
    let reply = client.json().await;
    assert_eq!(reply["type"], "mse", "{reply}");
    assert_eq!(sources(&hub, "camera.card"), Some(vec!["fake:cam1".to_owned()]));
}
