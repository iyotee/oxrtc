//! `PUT/GET/DELETE /api/preload`: held only once the tracks are known, `500`
//! when a wanted media is missing, replaced by a second `PUT`, and never
//! serialised across streams.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing, clippy::panic)]

mod common;

use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::http::{Method, StatusCode};
use common::*;
use oxrtc_core::{Codec, MediaKind, TrackInfo};
use serde_json::json;

fn preload_consumers(api: &Api, name: &str) -> usize {
    api.hub.get(name).unwrap().snapshot().consumers.iter().filter(|c| c.kind == "preload").count()
}

async fn put_stream(api: &Api, name: &str, key: &str, script: Script) {
    api.factory.add(key, script);
    let reply = api.call(Method::PUT, &format!("/api/streams?name={name}&src=fake:{key}")).await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text());
}

#[tokio::test]
async fn held_once_tracks_are_known_and_listed() {
    let api = Api::new();
    let tracks = vec![TrackInfo::video(Codec::H264), TrackInfo::audio(Codec::Opus, 48_000, 2)];
    let script = Script { tracks_after: Duration::from_millis(300), ..Script::tracks(tracks, 1000) };
    put_stream(&api, "cam_rec", "rec", script).await;

    let start = Instant::now();
    let reply = api.call(Method::PUT, "/api/preload?src=cam_rec&audio=opus").await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text());
    assert!(start.elapsed() >= Duration::from_millis(250), "200 only once the tracks are known");

    let info = api.call(Method::GET, "/api/streams?src=cam_rec").await.json();
    assert_eq!(info["consumers"][0]["format_name"], "preload");
    assert_eq!(info["consumers"][0]["medias"], json!(["audio, sendonly, OPUS"]));
    assert_eq!(info["producers"][0]["medias"], json!(["video, recvonly, H264", "audio, recvonly, OPUS/48000/2"]));
    let list = api.call(Method::GET, "/api/preload").await.json();
    assert_eq!(list, json!({"cam_rec": {"query": "audio=opus", "medias": ["audio, sendonly, OPUS"], "held": true}}));

    // A video preload lists a video entry.
    let reply = api.call(Method::PUT, "/api/preload?src=cam_rec&video&audio=opus").await;
    assert_eq!(reply.status, StatusCode::OK);
    let info = api.call(Method::GET, "/api/streams?src=cam_rec").await.json();
    assert!(eventually(Duration::from_secs(2), || preload_consumers(&api, "cam_rec") == 1).await);
    let info_consumers = api.call(Method::GET, "/api/streams?src=cam_rec").await.json()["consumers"].clone();
    assert_eq!(info_consumers[0]["medias"], json!(["video, sendonly, H264", "audio, sendonly, OPUS"]), "{info}");

    assert_eq!(api.call(Method::DELETE, "/api/preload?src=cam_rec").await.status, StatusCode::OK);
    assert!(eventually(Duration::from_secs(2), || preload_consumers(&api, "cam_rec") == 0).await);
    assert_eq!(api.call(Method::GET, "/api/preload").await.json(), json!({}));
    assert_eq!(api.call(Method::DELETE, "/api/preload?src=cam_rec").await.status, StatusCode::OK, "idempotent");
}

#[tokio::test]
async fn refused_when_a_wanted_media_is_missing() {
    let api = Api::new();
    put_stream(&api, "cam", "video_only", Script::video(Codec::H265, 1000)).await;
    let reply = api.call(Method::PUT, "/api/preload?src=cam&audio=opus").await;
    assert_eq!(reply.status, StatusCode::INTERNAL_SERVER_ERROR);
    assert!(reply.text().contains("no source offers"), "{}", reply.text());
    assert_eq!(api.call(Method::GET, "/api/preload").await.json(), json!({}), "the failed preload is dropped");
    assert_eq!(preload_consumers(&api, "cam"), 0);

    // The retry openIVR makes: video alone.
    assert_eq!(api.call(Method::PUT, "/api/preload?src=cam&video=").await.status, StatusCode::OK);
    // Naming no media holds whatever the stream has.
    assert_eq!(api.call(Method::PUT, "/api/preload?src=cam").await.status, StatusCode::OK);
    let list = api.call(Method::GET, "/api/preload").await.json();
    assert_eq!(list["cam"], json!({"query": "", "medias": ["video, sendonly, H265"], "held": true}));

    // Two sources, the audio one declared but never connecting: the partial
    // track list (after the probe timeout) lacks audio.
    api.factory.add("v", Script::video(Codec::H264, 1000));
    api.factory.add("a", Script::silent(vec![(MediaKind::Audio, Codec::Opus)]));
    api.call(Method::PUT, "/api/streams?name=pair&src=fake:v&src=fake:a").await;
    let reply = api.call(Method::PUT, "/api/preload?src=pair&video&audio=opus").await;
    assert_eq!(reply.status, StatusCode::INTERNAL_SERVER_ERROR);
    assert!(reply.text().contains("no audio track"), "{}", reply.text());
    assert!(eventually(Duration::from_secs(2), || preload_consumers(&api, "pair") == 0).await);

    for (uri, status) in [
        ("/api/preload?src=nope", StatusCode::NOT_FOUND),
        ("/api/preload?src=cam&audio=vp9", StatusCode::BAD_REQUEST),
        ("/api/preload?src=cam&audio=h264", StatusCode::BAD_REQUEST),
        ("/api/preload", StatusCode::BAD_REQUEST),
    ] {
        assert_eq!(api.call(Method::PUT, uri).await.status, status, "{uri}");
    }
}

#[tokio::test]
async fn bounded_wait() {
    let api = Api::build(|s| s.with_preload_wait(Duration::from_millis(300)));
    put_stream(&api, "never", "never", Script::silent(vec![(MediaKind::Video, Codec::H264)])).await;
    let start = Instant::now();
    let reply = api.call(Method::PUT, "/api/preload?src=never&video").await;
    assert_eq!(reply.status, StatusCode::INTERNAL_SERVER_ERROR);
    assert!(reply.text().contains("no track within"), "{}", reply.text());
    assert!(start.elapsed() < Duration::from_secs(2));
    assert_eq!(api.call(Method::GET, "/api/preload").await.json(), json!({}));
    assert!(eventually(Duration::from_secs(2), || preload_consumers(&api, "never") == 0).await);
}

#[tokio::test]
async fn a_second_put_replaces_the_first() {
    let api = Arc::new(Api::new());
    let script = Script { tracks_after: Duration::from_millis(800), ..Script::video(Codec::H264, 1000) };
    put_stream(&api, "cam", "slow", script).await;

    let first = tokio::spawn({
        let api = api.clone();
        async move { api.call(Method::PUT, "/api/preload?src=cam&video").await }
    });
    tokio::time::sleep(Duration::from_millis(100)).await;
    let start = Instant::now();
    let second = tokio::spawn({
        let api = api.clone();
        async move { api.call(Method::PUT, "/api/preload?src=cam&video=h264").await }
    });
    let first = first.await.unwrap();
    assert_eq!(first.status, StatusCode::INTERNAL_SERVER_ERROR);
    assert!(first.text().contains("replaced"), "{}", first.text());
    assert!(start.elapsed() < Duration::from_millis(500), "the first answers as soon as it is replaced");
    assert_eq!(second.await.unwrap().status, StatusCode::OK);
    assert!(eventually(Duration::from_secs(2), || preload_consumers(&api, "cam") == 1).await);

    // Replacing a held preload: still exactly one.
    assert_eq!(api.call(Method::PUT, "/api/preload?src=cam&video").await.status, StatusCode::OK);
    assert!(eventually(Duration::from_secs(2), || preload_consumers(&api, "cam") == 1).await);
    assert_eq!(api.call(Method::GET, "/api/preload").await.json()["cam"]["query"], "video");
}

#[tokio::test]
async fn a_slow_stream_does_not_delay_another() {
    let api = Arc::new(Api::new());
    put_stream(&api, "stuck", "stuck", Script::silent(vec![(MediaKind::Video, Codec::H264)])).await;
    put_stream(&api, "fine", "fine", Script::video(Codec::H264, 1000)).await;

    let stuck = tokio::spawn({
        let api = api.clone();
        async move { api.call(Method::PUT, "/api/preload?src=stuck&video").await }
    });
    tokio::time::sleep(Duration::from_millis(50)).await;
    let start = Instant::now();
    let reply = api.call(Method::PUT, "/api/preload?src=fine&video").await;
    assert_eq!(reply.status, StatusCode::OK);
    assert!(start.elapsed() < Duration::from_secs(1), "took {:?}", start.elapsed());
    assert!(!stuck.is_finished(), "the stuck preload is still waiting");
    let list = api.call(Method::GET, "/api/preload").await.json();
    assert_eq!(list["stuck"]["held"], false);
    assert_eq!(list["fine"]["held"], true);

    // Removing the stream ends its pending preload.
    assert_eq!(api.call(Method::DELETE, "/api/streams?src=stuck").await.status, StatusCode::OK);
    let reply = tokio::time::timeout(Duration::from_secs(2), stuck).await.unwrap().unwrap();
    assert_eq!(reply.status, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(api.call(Method::GET, "/api/preload").await.json().as_object().unwrap().len(), 1);
}

#[tokio::test]
async fn a_closed_stream_drops_its_held_preload() {
    let api = Api::new();
    put_stream(&api, "cam", "a", Script::video(Codec::H264, 1000)).await;
    assert_eq!(api.call(Method::PUT, "/api/preload?src=cam&video").await.status, StatusCode::OK);
    api.factory.add("b", Script::video(Codec::H265, 1000));
    assert_eq!(api.call(Method::PUT, "/api/streams?name=cam&src=fake:b").await.status, StatusCode::OK);
    let mut released = false;
    for _ in 0..100 {
        if api.call(Method::GET, "/api/preload").await.json() == json!({}) {
            released = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(released, "the replaced stream's preload is released");
    assert_eq!(preload_consumers(&api, "cam"), 0, "the new stream is not preloaded");
}

#[tokio::test]
async fn dropping_the_router_releases_held_preloads() {
    let api = Api::new();
    put_stream(&api, "cam", "a", Script::video(Codec::H264, 1000)).await;
    assert_eq!(api.call(Method::PUT, "/api/preload?src=cam&video").await.status, StatusCode::OK);
    assert_eq!(preload_consumers(&api, "cam"), 1);
    let Api { router, hub, .. } = api;
    drop(router);
    let released = eventually(Duration::from_secs(2), || {
        hub.get("cam").unwrap().snapshot().consumers.iter().all(|c| c.kind != "preload")
    })
    .await;
    assert!(released, "no preload outlives the API that held it");
}

/// What the stream's first source (the camera's picture) is held by.
fn picture_demand(api: &Api, name: &str) -> usize {
    api.hub.get(name).unwrap().snapshot().sources[0].demand
}

/// A camera's picture at once, and its sound from a source of its own (a
/// transcode) that takes `sound_after` to start, or never with `None`.
async fn camera_with_sound(api: &Api, sound_after: Option<Duration>) {
    let video = vec![(MediaKind::Video, Codec::H264)];
    let audio = vec![(MediaKind::Audio, Codec::Opus)];
    api.factory.add("v", Script { declared: Some(video), ..Script::video(Codec::H264, 10_000) });
    let sound = match sound_after {
        Some(after) => Script {
            declared: Some(audio),
            tracks_after: after,
            ..Script::tracks(vec![TrackInfo::audio(Codec::Opus, 48_000, 2)], 10_000)
        },
        None => Script::silent(audio),
    };
    api.factory.add("a", sound);
    let reply = api.call(Method::PUT, "/api/streams?name=cam_rec&src=fake:v&src=fake:a").await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text());
    assert_eq!(api.call(Method::PUT, "/api/preload?src=cam_rec&video").await.status, StatusCode::OK);
    assert_eq!(picture_demand(api, "cam_rec"), 1);
}

#[tokio::test]
async fn a_new_preload_holds_before_the_old_one_is_released() {
    // openIVR's regain of a camera's sound: held by its picture, it asks
    // for the sound, whose transcode takes a while to start.
    let api = Arc::new(Api::new());
    camera_with_sound(&api, Some(Duration::from_millis(800))).await;
    let regain = tokio::spawn({
        let api = api.clone();
        async move { api.call(Method::PUT, "/api/preload?src=cam_rec&audio=opus").await }
    });
    // While the sound starts, the picture stays held (go2rtc released it
    // first: here, for longer than the idle grace, the camera stopped).
    let (mut lowest, mut listed) = (usize::MAX, Vec::new());
    while !regain.is_finished() {
        lowest = lowest.min(picture_demand(&api, "cam_rec"));
        listed.push(api.call(Method::GET, "/api/preload").await.json()["cam_rec"].clone());
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(lowest, 1, "the camera was unheld while its sound started");
    // The hold listed meanwhile is the one that holds.
    let hold = json!({"query": "video", "medias": ["video, sendonly, H264"], "held": true});
    assert!(listed.len() > 10 && listed.iter().all(|l| *l == hold), "{listed:?}");
    assert_eq!(regain.await.unwrap().status, StatusCode::OK);
    let list = api.call(Method::GET, "/api/preload").await.json();
    assert_eq!(list["cam_rec"], json!({"query": "audio=opus", "medias": ["audio, sendonly, OPUS"], "held": true}));
    // Then, and only then, the old hold goes.
    assert!(eventually(Duration::from_secs(2), || picture_demand(&api, "cam_rec") == 0).await);
    assert!(eventually(Duration::from_secs(2), || preload_consumers(&api, "cam_rec") == 1).await);
}

#[tokio::test]
async fn a_replacement_that_fails_leaves_the_hold() {
    let api = Api::build(|s| s.with_preload_wait(Duration::from_millis(400)));
    camera_with_sound(&api, None).await;
    let reply = api.call(Method::PUT, "/api/preload?src=cam_rec&audio=opus").await;
    assert_eq!(reply.status, StatusCode::INTERNAL_SERVER_ERROR, "{}", reply.text());
    let list = api.call(Method::GET, "/api/preload").await.json();
    assert_eq!(list["cam_rec"], json!({"query": "video", "medias": ["video, sendonly, H264"], "held": true}));
    assert!(eventually(Duration::from_secs(2), || preload_consumers(&api, "cam_rec") == 1).await);
    assert_eq!(picture_demand(&api, "cam_rec"), 1, "the camera is still held");
    // openIVR's next step, video alone, replaces it as before.
    assert_eq!(api.call(Method::PUT, "/api/preload?src=cam_rec&video=").await.status, StatusCode::OK);
    assert!(eventually(Duration::from_secs(2), || preload_consumers(&api, "cam_rec") == 1).await);
    assert_eq!(api.call(Method::GET, "/api/preload").await.json()["cam_rec"]["held"], true);
}
