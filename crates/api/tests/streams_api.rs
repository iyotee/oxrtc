//! `/api/streams` as go2rtc 1.9.15 defines it - the producers' shape, the
//! probe, `PATCH`, `POST`, `api.allow_stream_changes` - and
//! `/api/streams.dot` (M5 F7, D48).
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing, clippy::panic)]

mod common;

use std::time::Duration;

use axum::http::{Method, StatusCode, header};
use common::*;
use oxrtc_api::config::{self, Permissions};
use oxrtc_core::{Codec, ConsumerTag, TrackInfo, Want};
use serde_json::{Value, json};

fn fixture(name: &str) -> Value {
    let path = format!("{}/tests/fixtures/go2rtc_1.9.15/{name}", env!("CARGO_MANIFEST_DIR"));
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

fn keys(value: &Value) -> Vec<String> {
    let mut keys: Vec<String> = value.as_object().unwrap().keys().cloned().collect();
    keys.sort();
    keys
}

#[tokio::test]
async fn producers_have_go2rtcs_shape_idle_and_running() {
    let api = Api::new();
    api.factory.add(
        "cam",
        Script::tracks(vec![TrackInfo::video(Codec::H264), TrackInfo::audio(Codec::Opus, 48_000, 2)], 1000),
    );
    api.factory.add("idle", Script::video(Codec::H264, 10));
    assert_eq!(api.call(Method::PUT, "/api/streams?name=live&src=fake:cam").await.status, StatusCode::OK);
    assert_eq!(
        api.call(Method::PUT, "/api/streams?name=idle&src=fake://admin:secret@idle/x").await.status,
        StatusCode::OK
    );

    // An idle source: `{"url"}` alone, its credential masked as go2rtc masks it.
    let idle = fixture("streams_idle.json");
    let theirs = &idle["idle"]["producers"][0];
    let ours = api.call(Method::GET, "/api/streams?src=idle").await.json();
    assert_eq!(keys(&ours["producers"][0]), keys(theirs), "{ours}");
    assert_eq!(theirs["url"], "rtsp://***@127.0.0.1:43999/none");
    assert_eq!(ours["producers"][0]["url"], "fake://***@idle/x");

    // A running one: every key oxrtc names as go2rtc does is go2rtc's.
    let stream = api.hub.get("live").unwrap();
    let mut viewer = stream.subscribe(&Want::all(), ConsumerTag::new("mse")).await.unwrap();
    for _ in 0..6 {
        viewer.recv().await.unwrap();
    }
    let running = fixture("stream_running.json");
    let theirs = &running["producers"][0];
    let ours = api.call(Method::GET, "/api/streams?src=live").await.json();
    let producer = &ours["producers"][0];
    for key in ["format_name", "medias", "receivers", "bytes_recv"] {
        assert!(theirs.get(key).is_some() && producer.get(key).is_some(), "{key}: {producer}");
    }
    assert_eq!(theirs["medias"], json!(["video, recvonly, H264", "audio, recvonly, OPUS/48000/2"]));
    assert_eq!(producer["medias"], theirs["medias"], "the same tracks, written the same way");
    for (mine, go2rtc) in producer["receivers"].as_array().unwrap().iter().zip(theirs["receivers"].as_array().unwrap())
    {
        for key in ["codec_name", "codec_type"] {
            assert_eq!(mine["codec"][key], go2rtc["codec"][key], "{key}");
        }
        assert!(mine["packets"].as_u64().is_some() && go2rtc["packets"].as_u64().is_some());
    }
    assert_eq!(producer["receivers"][1]["codec"]["sample_rate"], theirs["receivers"][1]["codec"]["sample_rate"]);
    assert_eq!(producer["receivers"][1]["codec"]["channels"], theirs["receivers"][1]["codec"]["channels"]);
    // Home Assistant's `go2rtc-client` decodes `{name: {producers: [{url}]}}`:
    // every producer has its `url`.
    let all = api.call(Method::GET, "/api/streams").await.json();
    for (_, stream) in all.as_object().unwrap() {
        for producer in stream["producers"].as_array().unwrap() {
            assert!(producer["url"].is_string(), "{producer}");
        }
    }
}

#[tokio::test]
async fn a_probe_opens_the_stream_lists_itself_and_lets_go() {
    let api = Api::new();
    let mut script = Script::tracks(vec![TrackInfo::video(Codec::H264), TrackInfo::audio(Codec::Pcma, 8000, 1)], 1000);
    script.tracks_after = Duration::from_millis(150);
    api.factory.add("cam", script);
    assert_eq!(api.call(Method::PUT, "/api/streams?name=cam&src=fake:cam").await.status, StatusCode::OK);
    // Without media in the query: what is known, nothing opened.
    let plain = api.call(Method::GET, "/api/streams?src=cam").await.json();
    assert_eq!(plain["producers"], json!([{"url": "fake:cam"}]));
    // go2rtc's probe link (`index.html`): the tracks once the source has them.
    let probe = api.call(Method::GET, "/api/streams?src=cam&video=all&audio=all&microphone").await;
    assert_eq!(probe.status, StatusCode::OK, "{}", probe.text());
    let probe = probe.json();
    assert_eq!(
        probe["producers"][0]["medias"],
        json!(["video, recvonly, H264", "audio, recvonly, PCMA/8000"]),
        "{probe}"
    );
    assert_eq!(probe["consumers"][0]["format_name"], "probe", "{probe}");
    // Gone with its answer.
    let stream = api.hub.get("cam").unwrap();
    assert!(eventually(Duration::from_secs(2), || stream.snapshot().consumers.is_empty()).await);
    // Unknown stream, and a query that is not one.
    assert_eq!(api.call(Method::GET, "/api/streams?src=nope&video=all").await.status, StatusCode::NOT_FOUND);
    assert_eq!(api.call(Method::GET, "/api/streams?src=cam&video=vp9").await.status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn a_probe_of_a_source_with_nothing_to_give_is_go2rtcs_500() {
    let api = Api::new();
    api.factory.add("mute", Script::tracks(Vec::new(), 0));
    assert_eq!(api.call(Method::PUT, "/api/streams?name=mute&src=fake:mute").await.status, StatusCode::OK);
    // go2rtc answers 500 with its consumer's error when no producer gives the
    // media; here once the hub gives up on the source's tracks (its probe
    // timeout, under `PROBE_WAIT`).
    let started = std::time::Instant::now();
    let probe = api.call(Method::GET, "/api/streams?src=mute&video").await;
    assert_eq!(probe.status, StatusCode::INTERNAL_SERVER_ERROR);
    assert!(probe.text().starts_with("streams: "), "{}", probe.text());
    assert!(started.elapsed() < oxrtc_api::PROBE_WAIT + Duration::from_secs(1), "{:?}", started.elapsed());
}

#[tokio::test]
async fn patch_sets_one_source_creates_and_links_names_as_go2rtc() {
    let api = Api::new();
    api.factory.add("a", Script::video(Codec::H264, 0));
    api.factory.add("b", Script::video(Codec::H264, 0));
    // Created when absent.
    assert_eq!(api.call(Method::PATCH, "/api/streams?name=cam&src=fake:a").await.status, StatusCode::OK);
    assert_eq!(api.hub.get("cam").unwrap().specs().map(|s| s.expose().to_owned()).collect::<Vec<_>>(), ["fake:a"]);
    // Its sources replaced by the one given.
    assert_eq!(api.call(Method::PUT, "/api/streams?name=two&src=fake:a&src=fake:b").await.status, StatusCode::OK);
    assert_eq!(api.call(Method::PATCH, "/api/streams?name=two&src=fake:b").await.status, StatusCode::OK);
    assert_eq!(api.hub.get("two").unwrap().specs().map(|s| s.expose().to_owned()).collect::<Vec<_>>(), ["fake:b"]);
    // A stream's name: the name reads that stream.
    assert_eq!(api.call(Method::PATCH, "/api/streams?name=other&src=cam").await.status, StatusCode::OK);
    assert_eq!(
        api.hub.get("other").unwrap().specs().map(|s| s.expose().to_owned()).collect::<Vec<_>>(),
        ["stream:cam"]
    );
    // go2rtc's refusals: no name (`400`), no source, a source no driver takes.
    for (uri, reason) in [
        ("/api/streams?src=fake:a", "name is required"),
        ("/api/streams?name=cam", "src is required"),
        ("/api/streams?name=x&src=exec:calc.exe", "scheme"),
        ("/api/streams?name=x&src=nonsense", "no valid scheme"),
    ] {
        let reply = api.call(Method::PATCH, uri).await;
        assert_eq!(reply.status, StatusCode::BAD_REQUEST, "{uri}");
        assert!(reply.text().contains(reason), "{uri}: {}", reply.text());
    }
    assert!(api.hub.get("x").is_none());
}

#[tokio::test]
async fn post_answers_what_go2rtc_answers_then_says_it_is_not_here_yet() {
    let api = Api::new();
    api.factory.add("a", Script::video(Codec::H264, 0));
    assert_eq!(api.call(Method::PUT, "/api/streams?name=cam&src=fake:a").await.status, StatusCode::OK);
    for (uri, status) in [
        ("/api/streams?src=cam", StatusCode::BAD_REQUEST),
        ("/api/streams?dst=cam", StatusCode::BAD_REQUEST),
        ("/api/streams?src=rtsp://x/y&dst=nope", StatusCode::NOT_FOUND),
        // A sound played to the camera (F12), a stream pushed to a URL (F10).
        ("/api/streams?src=ffmpeg:song.mp3&dst=cam", StatusCode::NOT_IMPLEMENTED),
        ("/api/streams?src=cam&dst=rtmp://example/live", StatusCode::NOT_IMPLEMENTED),
    ] {
        assert_eq!(api.call(Method::POST, uri).await.status, status, "{uri}");
    }
}

#[tokio::test]
async fn stream_changes_are_refused_when_the_file_says_so() {
    let (config, _) = config::parse("api:\n  token: t\n  allow_stream_changes: false\n").unwrap();
    assert!(!config.api.permissions.stream_changes);
    let api = Api::build(|s| s.with_permissions(config.api.permissions));
    api.factory.add("a", Script::video(Codec::H264, 0));
    api.hub.upsert("cam", &["fake:a"]).unwrap();
    for (method, uri) in [
        (Method::PUT, "/api/streams?name=x&src=fake:a"),
        (Method::PATCH, "/api/streams?name=x&src=fake:a"),
        (Method::POST, "/api/streams?src=fake:a&dst=cam"),
        (Method::DELETE, "/api/streams?src=cam"),
    ] {
        let reply = api.call(method.clone(), uri).await;
        assert_eq!(reply.status, StatusCode::FORBIDDEN, "{method} {uri}");
        assert!(reply.text().contains("api.allow_stream_changes"), "{}", reply.text());
    }
    assert!(api.hub.get("x").is_none() && api.hub.get("cam").is_some());
    assert_eq!(api.call(Method::GET, "/api/streams").await.status, StatusCode::OK, "reading is not a change");
    // On by default: openIVR and Home Assistant add their cameras with PUT.
    assert!(Permissions::default().stream_changes);
}

#[tokio::test]
async fn streams_dot_is_go2rtcs_graph() {
    let api = Api::new();
    api.factory.add(
        "cam",
        Script::tracks(vec![TrackInfo::video(Codec::H264), TrackInfo::audio(Codec::Opus, 48_000, 2)], 1000),
    );
    api.hub.upsert("cam", &["fake://admin:pw@cam/x"]).unwrap();
    api.hub.upsert("other", &["fake:cam"]).unwrap();
    let stream = api.hub.get("cam").unwrap();
    let tag = ConsumerTag::new("mse").with_remote("192.168.1.20:5000").with_user_agent("a \"quoted\" agent");
    let mut viewer = stream.subscribe(&Want::all(), tag).await.unwrap();
    let other = api.hub.get("other").unwrap();
    let mut second = other.subscribe(&Want::all(), ConsumerTag::new("rtsp")).await.unwrap();
    for _ in 0..4 {
        viewer.recv().await.unwrap();
        second.recv().await.unwrap();
    }
    let reply = api.call(Method::GET, "/api/streams.dot?src=cam&src=nope").await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(reply.headers[header::CONTENT_TYPE], "text/vnd.graphviz");
    let dot = reply.text();
    let theirs =
        std::fs::read_to_string(format!("{}/tests/fixtures/go2rtc_1.9.15/streams.dot", env!("CARGO_MANIFEST_DIR")))
            .unwrap();
    // go2rtc's frame and groups: the host, the producer, its tracks, the consumer.
    for part in
        ["digraph {\n", "[group=host];", "[group=producer, label=", "[group=node, label=", "[group=consumer, label="]
    {
        assert!(theirs.contains(part) && dot.contains(part), "{part}: {dot}");
    }
    assert!(dot.ends_with('}'), "{dot}");
    assert!(dot.contains("\"h264\"") && dot.contains("\"opus\"") && dot.contains("\"192.168.1.20\""), "{dot}");
    assert!(dot.contains("user_agent=a 'quoted' agent"), "quotes as go2rtc writes them: {dot}");
    assert!(!dot.contains("pw"), "no credential: {dot}");
    assert!(!dot.contains("other"), "only the streams asked for: {dot}");
    // Every stream when none is named.
    assert!(api.call(Method::GET, "/api/streams.dot").await.text().contains("stream=other"));
}
