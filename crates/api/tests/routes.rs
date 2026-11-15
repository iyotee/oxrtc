//! Every route, its authentication, its errors and the JSON it answers.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing, clippy::panic)]

mod common;

use std::time::Duration;

use axum::http::{Method, StatusCode, header};
use bytes::Bytes;
use common::*;
use oxrtc_api::{AnswerError, ApiState, Auth, MAX_BODY_BYTES, SnapshotError};
use oxrtc_core::{Codec, ConsumerTag, Event, TrackInfo, Want};
use serde_json::json;

#[tokio::test]
async fn version() {
    let api = Api::new();
    let reply = api.call(Method::GET, "/api").await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(reply.content_type(), "application/json");
    assert_eq!(reply.json(), json!({"version": "1.2.3-test"}));
}

#[tokio::test]
async fn every_route_requires_authentication() {
    let api = Api::build(|s| {
        s.with_webrtc(FakeAnswerer::new(Ok("v=0".into()))).with_snapshots(FakeSnapshotter::new(Ok(Bytes::new())))
    });
    for (m, uri) in ROUTES {
        for authorization in [None, Some(basic(USER, "wrong")), Some(basic("root", PASS)), Some("Bearer nope".into())] {
            let reply = api.send(method(m), uri, Vec::new(), authorization.as_deref()).await;
            assert_eq!(reply.status, StatusCode::UNAUTHORIZED, "{m} {uri} with {authorization:?}");
            let challenge = reply.headers.get(header::WWW_AUTHENTICATE).unwrap().to_str().unwrap();
            assert!(challenge.starts_with("Basic"), "{challenge}");
        }
    }
    assert!(api.hub.is_empty(), "no unauthenticated call reached a handler");
    // Either credential is enough.
    assert_eq!(api.send(Method::GET, "/api", vec![], Some(&format!("Bearer {TOKEN}"))).await.status, StatusCode::OK);
    assert_eq!(api.call(Method::GET, "/api").await.status, StatusCode::OK);
    assert_eq!(api.call(Method::GET, "/no/such/route").await.status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn unauthenticated_only_when_configured() {
    let (logs, _guard) = Logs::capture();
    let factory = std::sync::Arc::new(FakeFactory::default());
    let hub = hub(factory.clone());
    let router = oxrtc_api::router(ApiState::new(hub.clone(), "v", Auth::Unauthenticated));
    assert!(logs.text().contains("authentication is disabled"), "the opt-in is logged");
    let api = Api { router, hub, factory };
    assert_eq!(api.send(Method::GET, "/api", vec![], None).await.status, StatusCode::OK);
    assert_eq!(api.send(Method::GET, "/api/streams", vec![], None).await.status, StatusCode::OK);
}

#[tokio::test]
async fn streams_put_get_delete() {
    let api = Api::new();
    api.factory.add("cam1", Script::video(Codec::H264, 0));
    let reply = api.call(Method::PUT, "/api/streams?name=cam&src=fake:cam1&src=fake:cam1%23timeout%3D5").await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text());
    let stream = api.hub.get("cam").unwrap();
    assert_eq!(stream.specs().count(), 2);
    assert_eq!(stream.specs().nth(1).unwrap().fragment("timeout"), Some("5"));

    let all = api.call(Method::GET, "/api/streams").await;
    assert_eq!(all.status, StatusCode::OK);
    let idle = |url: &str| json!({"url": url, "state": "idle", "failures": 0, "frames": 0, "bytes": 0, "rejected": 0, "demand": 0});
    let sources = json!([idle("fake:cam1"), idle("fake:cam1#timeout=5")]);
    assert_eq!(all.json(), json!({"cam": {"producers": [], "consumers": [], "sources": sources}}));
    let one = api.call(Method::GET, "/api/streams?src=cam").await;
    assert_eq!(one.json(), json!({"producers": [], "consumers": [], "sources": sources}));
    let missing = api.call(Method::GET, "/api/streams?src=nope").await;
    assert_eq!(missing.status, StatusCode::NOT_FOUND);

    for (uri, reason) in [
        ("/api/streams?src=fake:cam1", "name is required"),
        ("/api/streams?name=cam", "src is required"),
        ("/api/streams?name=bad%20name&src=fake:cam1", "invalid stream name"),
        ("/api/streams?name=x&src=fake:unknown", "no such fake"),
        ("/api/streams?name=x&src=nonsense", "no valid scheme"),
    ] {
        let reply = api.call(Method::PUT, uri).await;
        assert_eq!(reply.status, StatusCode::BAD_REQUEST, "{uri}");
        assert!(reply.text().contains(reason), "{uri}: {}", reply.text());
    }

    assert_eq!(api.call(Method::DELETE, "/api/streams?src=cam").await.status, StatusCode::OK);
    assert!(stream.is_closed() && api.hub.get("cam").is_none());
    assert_eq!(api.call(Method::DELETE, "/api/streams?src=cam").await.status, StatusCode::NOT_FOUND);
    assert_eq!(api.call(Method::DELETE, "/api/streams").await.status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn exec_echo_and_expr_are_refused() {
    let api = Api::new();
    for src in ["exec:ffmpeg%20-i%20x", "echo:/bin/sh", "expr:let%20x", "file:/etc/passwd"] {
        let reply = api.call(Method::PUT, &format!("/api/streams?name=evil&src={src}")).await;
        assert_eq!(reply.status, StatusCode::BAD_REQUEST, "{src}");
        assert!(reply.text().contains("unsupported source scheme"), "{}", reply.text());
    }
    assert!(api.hub.is_empty());
}

#[tokio::test]
async fn too_many_query_parameters() {
    let api = Api::new();
    let query: Vec<String> = (0..65).map(|i| format!("src=fake:x{i}")).collect();
    let reply = api.call(Method::PUT, &format!("/api/streams?name=x&{}", query.join("&"))).await;
    assert_eq!(reply.status, StatusCode::BAD_REQUEST);
    assert!(reply.text().contains("too many query parameters"));
    let query: Vec<String> = (0..63).map(|_| "src=fake:x".to_owned()).collect();
    let reply = api.call(Method::PUT, &format!("/api/streams?name=x&{}", query.join("&"))).await;
    assert!(reply.text().contains("too many sources"), "63 pairs pass the query cap: {}", reply.text());
}

#[tokio::test]
async fn identical_put_keeps_a_live_subscription() {
    let api = Api::new();
    api.factory.add("cam1", Script::video(Codec::H264, 1000));
    api.factory.add("cam2", Script::video(Codec::H264, 1000));
    let put = "/api/streams?name=cam&src=fake://u:p@cam1";
    assert_eq!(api.call(Method::PUT, put).await.status, StatusCode::OK);
    let stream = api.hub.get("cam").unwrap();
    let mut sub = stream.subscribe(&Want::all(), ConsumerTag::new("rtsp")).await.unwrap();
    assert!(matches!(sub.recv().await.unwrap(), Event::Tracks(_)));
    assert!(matches!(sub.recv().await.unwrap(), Event::Frame { .. }));

    assert_eq!(api.call(Method::PUT, put).await.status, StatusCode::OK);
    assert!(!stream.is_closed());
    assert!(std::sync::Arc::ptr_eq(&stream, &api.hub.get("cam").unwrap()));
    for _ in 0..3 {
        assert!(matches!(sub.recv().await.unwrap(), Event::Frame { .. }), "the viewer keeps receiving");
    }

    assert_eq!(api.call(Method::PUT, "/api/streams?name=cam&src=fake:cam2").await.status, StatusCode::OK);
    assert!(stream.is_closed());
    let ended = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if sub.recv().await.is_err() {
                break;
            }
        }
    })
    .await;
    assert!(ended.is_ok(), "different sources end the old subscription");
}

#[tokio::test]
async fn streams_json_shape() {
    let api = Api::new();
    api.factory.add(
        "cam1",
        Script::tracks(vec![TrackInfo::video(Codec::H265), TrackInfo::audio(Codec::Pcma, 16_000, 1)], 1000),
    );
    api.factory.add("quiet", Script::tracks(vec![TrackInfo::audio(Codec::Opus, 48_000, 2)], 0));
    api.factory.add("idle", Script::video(Codec::H264, 10));
    let put = "/api/streams?name=cam&src=fake://admin:pw@cam1/live&src=fake:quiet&src=fake:idle";
    assert_eq!(api.call(Method::PUT, put).await.status, StatusCode::OK);
    let stream = api.hub.get("cam").unwrap();
    let video = Want::from_query("video").unwrap();
    let audio = Want::from_query("audio=opus").unwrap();
    let mut viewer = stream.subscribe(&video, ConsumerTag::new("webrtc").with_remote("127.0.0.1:5000")).await.unwrap();
    let _audio = stream.subscribe(&audio, ConsumerTag::new("rtsp")).await.unwrap();
    for _ in 0..8 {
        viewer.recv().await.unwrap();
    }

    let info = api.call(Method::GET, "/api/streams?src=cam").await.json();
    let producers = info["producers"].as_array().unwrap();
    assert_eq!(producers.len(), 2, "the idle source is not a producer: {info}");
    assert_eq!(producers[0]["url"], "fake://***@cam1/live");
    assert_eq!(producers[0]["medias"], json!(["video, recvonly, H265", "audio, recvonly, PCMA/16000"]));
    let receivers = producers[0]["receivers"].as_array().unwrap();
    assert_eq!(receivers[0]["codec"], json!({"codec_name": "h265", "codec_type": "video", "clock_rate": 90000}));
    assert_eq!(receivers[1]["codec"], json!({"codec_name": "pcma", "codec_type": "audio", "clock_rate": 16000}));
    assert!(receivers[0]["packets"].as_u64().unwrap() > 0);
    assert_eq!(producers[1]["medias"], json!(["audio, recvonly, OPUS/48000/2"]));
    assert_eq!(
        producers[1]["receivers"],
        json!([{"codec": {"codec_name": "opus", "codec_type": "audio", "clock_rate": 48000}}]),
        "packets is omitted while 0"
    );
    let consumers = info["consumers"].as_array().unwrap();
    assert_eq!(consumers[0]["format_name"], "webrtc");
    assert_eq!(consumers[0]["medias"], json!([]));
    assert_eq!(consumers[0]["remote_addr"], "127.0.0.1:5000");
    assert!(consumers[0]["frames"].as_u64().unwrap() >= 1, "frames delivered: {}", consumers[0]);
    assert_eq!(consumers[0]["dropped"], 0);
    assert_eq!(consumers[1]["format_name"], "rtsp");
    assert!(consumers[1].get("remote_addr").is_none());
    // Every source, the idle one included, with its state (oxrtc only).
    let sources = info["sources"].as_array().unwrap();
    assert_eq!(sources.len(), 3, "{info}");
    assert_eq!(sources[0]["state"], "running");
    assert!(sources[0]["last_frame_age_ms"].as_u64().is_some(), "{}", sources[0]);
    assert_eq!(sources[2]["state"], "idle");
}

#[tokio::test]
async fn webrtc_route() {
    let factory_api = |result| Api::build(move |s| s.with_webrtc(FakeAnswerer::new(result)));
    let api = Api::new();
    api.factory.add("cam1", Script::video(Codec::H264, 0));
    api.call(Method::PUT, "/api/streams?name=cam&src=fake:cam1").await;
    assert_eq!(api.call(Method::POST, "/api/webrtc?src=cam").await.status, StatusCode::NOT_IMPLEMENTED);

    let answerer = FakeAnswerer::new(Ok("v=0\r\nanswer".into()));
    let api = Api::build(|s| s.with_webrtc(answerer.clone()));
    api.factory.add("cam1", Script::video(Codec::H264, 0));
    api.call(Method::PUT, "/api/streams?name=cam&src=fake:cam1").await;
    let offer = b"v=0\r\no=- 1 1 IN IP4 0.0.0.0\r\n".to_vec();
    let reply = api.send(Method::POST, "/api/webrtc?src=nope", offer.clone(), Some(&basic(USER, PASS))).await;
    assert_eq!((reply.status, reply.text().as_str()), (StatusCode::NOT_FOUND, "stream not found"));
    let reply = api.send(Method::POST, "/api/webrtc?src=cam", offer.clone(), Some(&basic(USER, PASS))).await;
    assert_eq!(reply.status, StatusCode::CREATED);
    assert_eq!(reply.content_type(), "application/sdp");
    assert_eq!(reply.text(), "v=0\r\nanswer");
    let seen = answerer.seen.lock().unwrap().clone();
    assert_eq!(seen, vec![("cam".to_owned(), String::from_utf8(offer).unwrap(), None)]);

    let reply = api.send(Method::POST, "/api/webrtc?src=cam", vec![0xff, 0xfe], Some(&basic(USER, PASS))).await;
    assert_eq!(reply.status, StatusCode::BAD_REQUEST);
    let reply =
        api.send(Method::POST, "/api/webrtc?src=cam", vec![b'a'; MAX_BODY_BYTES + 1], Some(&basic(USER, PASS))).await;
    assert_eq!(reply.status, StatusCode::PAYLOAD_TOO_LARGE);
    let reply =
        api.send(Method::POST, "/api/webrtc?src=cam", vec![b'a'; MAX_BODY_BYTES], Some(&basic(USER, PASS))).await;
    assert_eq!(reply.status, StatusCode::CREATED, "exactly the limit passes");

    for (result, status, text) in [
        (
            AnswerError::Unavailable("no source opened".into()),
            StatusCode::INTERNAL_SERVER_ERROR,
            "streams: no source opened",
        ),
        (AnswerError::BadOffer("no media".into()), StatusCode::BAD_REQUEST, "bad offer: no media"),
        (AnswerError::NoMatchingCodec, StatusCode::BAD_REQUEST, "no matching codec"),
        (AnswerError::Busy, StatusCode::SERVICE_UNAVAILABLE, "too many sessions"),
        (AnswerError::Internal("boom".into()), StatusCode::INTERNAL_SERVER_ERROR, "internal error: boom"),
    ] {
        let api = factory_api(Err(result));
        api.factory.add("cam1", Script::video(Codec::H264, 0));
        api.call(Method::PUT, "/api/streams?name=cam&src=fake:cam1").await;
        let reply = api.send(Method::POST, "/api/webrtc?src=cam", b"v=0".to_vec(), Some(&basic(USER, PASS))).await;
        assert_eq!((reply.status, reply.text().as_str()), (status, text));
    }
}

#[tokio::test]
async fn frame_jpeg_route() {
    let api = Api::new();
    api.factory.add("cam1", Script::video(Codec::H264, 0));
    api.call(Method::PUT, "/api/streams?name=cam&src=fake:cam1").await;
    assert_eq!(api.call(Method::GET, "/api/frame.jpeg?src=cam").await.status, StatusCode::NOT_IMPLEMENTED);

    let snapshotter = FakeSnapshotter::new(Ok(Bytes::from_static(b"\xff\xd8jpeg")));
    let api = Api::build(|s| s.with_snapshots(snapshotter.clone()));
    api.factory.add("cam1", Script::video(Codec::H264, 0));
    api.call(Method::PUT, "/api/streams?name=cam&src=fake:cam1").await;
    let reply = api.call(Method::GET, "/api/frame.jpeg?src=cam&width=640").await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(reply.content_type(), "image/jpeg");
    assert_eq!(&reply.body[..], b"\xff\xd8jpeg");
    assert_eq!(api.call(Method::GET, "/api/frame.jpeg?src=cam").await.status, StatusCode::OK);
    assert_eq!(*snapshotter.widths.lock().unwrap(), vec![Some(640), None]);
    assert_eq!(api.call(Method::GET, "/api/frame.jpeg?src=nope").await.status, StatusCode::NOT_FOUND);
    for width in ["0", "abc", "-1", "7681", "16384", "99999999"] {
        let reply = api.call(Method::GET, &format!("/api/frame.jpeg?src=cam&width={width}")).await;
        assert_eq!(reply.status, StatusCode::BAD_REQUEST, "{width}");
    }

    let failing = FakeSnapshotter::new(Err(SnapshotError::Unavailable("no keyframe yet".into())));
    let api = Api::build(|s| s.with_snapshots(failing));
    api.factory.add("cam1", Script::video(Codec::H264, 0));
    api.call(Method::PUT, "/api/streams?name=cam&src=fake:cam1").await;
    let reply = api.call(Method::GET, "/api/frame.jpeg?src=cam").await;
    assert_eq!(reply.status, StatusCode::INTERNAL_SERVER_ERROR);
    assert!(reply.text().contains("no keyframe yet"));
}

#[tokio::test(start_paused = true)]
async fn request_deadlines() {
    let api = Api::build(|s| {
        s.with_snapshots(FakeSnapshotter::slow(Duration::from_secs(3600)))
            .with_webrtc(FakeAnswerer::slow(Ok("v=0".into()), Duration::from_secs(50)))
    });
    api.factory.add("cam1", Script::video(Codec::H264, 0));
    api.call(Method::PUT, "/api/streams?name=cam&src=fake:cam1").await;
    let start = tokio::time::Instant::now();
    let reply = api.call(Method::GET, "/api/frame.jpeg?src=cam").await;
    assert_eq!(reply.status, StatusCode::REQUEST_TIMEOUT);
    assert_eq!(start.elapsed().as_secs(), 10);
    let reply = api.send(Method::POST, "/api/webrtc?src=cam", b"v=0".to_vec(), Some(&basic(USER, PASS))).await;
    assert_eq!(reply.status, StatusCode::CREATED, "webrtc has 60 s");

    let api = Api::build(|s| s.with_webrtc(FakeAnswerer::slow(Ok("v=0".into()), Duration::from_secs(70))));
    api.factory.add("cam1", Script::video(Codec::H264, 0));
    api.call(Method::PUT, "/api/streams?name=cam&src=fake:cam1").await;
    let start = tokio::time::Instant::now();
    let reply = api.send(Method::POST, "/api/webrtc?src=cam", b"v=0".to_vec(), Some(&basic(USER, PASS))).await;
    assert_eq!(reply.status, StatusCode::REQUEST_TIMEOUT);
    assert_eq!(start.elapsed().as_secs(), 60);
}

#[tokio::test]
async fn no_password_in_any_response_or_log() {
    const SECRET: &str = "hunter2pw";
    let (logs, _guard) = Logs::capture();
    let api = Api::build(|s| {
        s.with_webrtc(FakeAnswerer::new(Ok("v=0".into()))).with_snapshots(FakeSnapshotter::new(Ok(Bytes::new())))
    });
    api.factory.add("cam1", Script::video(Codec::H264, 1000));
    let mut bodies = Vec::new();
    for uri in [
        format!("/api/streams?name=cam&src=fake://admin:{SECRET}@cam1"),
        format!("/api/streams?name=bad&src=exec://admin:{SECRET}@x"),
        format!("/api/streams?name=bad&src=fake://admin:{SECRET}@missing"),
        format!("/api/streams?name=rtsp://admin:{SECRET}@x&src=fake:cam1"),
        format!("/api/streams?src=rtsp://admin:{SECRET}@x"),
        format!("/api/streams?name=x&src=rtsp://admin:{SECRET}@x%0d%0aHost:%20evil"),
    ] {
        bodies.push(api.call(Method::PUT, &uri).await.text());
    }
    let stream = api.hub.get("cam").unwrap();
    let mut sub = stream.subscribe(&Want::all(), ConsumerTag::new("rtsp")).await.unwrap();
    sub.recv().await.unwrap();
    sub.recv().await.unwrap();
    for uri in [
        "/api/streams".to_owned(),
        "/api/streams?src=cam".to_owned(),
        format!("/api/streams?src=rtsp://admin:{SECRET}@x"),
        "/api/preload".to_owned(),
    ] {
        bodies.push(api.call(Method::GET, &uri).await.text());
    }
    bodies.push(api.call(Method::PUT, "/api/preload?src=cam&video").await.text());
    bodies.push(api.call(Method::GET, "/api/preload").await.text());
    bodies.push(api.call(Method::DELETE, &format!("/api/streams?src=rtsp://admin:{SECRET}@x")).await.text());
    // Backends whose messages quote a URL with its password (an FFmpeg error
    // does), and a codec name taken from the query.
    let failing = Api::build(|s| {
        s.with_webrtc(FakeAnswerer::new(Err(AnswerError::Unavailable(format!(
            "rtsp://admin:{SECRET}@10.0.0.9/live: 401 Unauthorized"
        )))))
        .with_snapshots(FakeSnapshotter::new(Err(SnapshotError::Internal(format!(
            "ffmpeg: rtsp://admin:{SECRET}@10.0.0.9/live: Connection refused"
        )))))
    });
    failing.factory.add("cam1", Script::video(Codec::H264, 1000));
    failing.call(Method::PUT, "/api/streams?name=cam&src=fake:cam1").await;
    let reply = failing.send(Method::POST, "/api/webrtc?src=cam", b"v=0".to_vec(), Some(&basic(USER, PASS))).await;
    assert_eq!(reply.status, StatusCode::INTERNAL_SERVER_ERROR);
    assert!(reply.text().starts_with("streams: rtsp://***@10.0.0.9/live"), "{}", reply.text());
    bodies.push(reply.text());
    let reply = failing.call(Method::GET, "/api/frame.jpeg?src=cam").await;
    assert_eq!(reply.status, StatusCode::INTERNAL_SERVER_ERROR);
    bodies.push(reply.text());
    let reply = failing.call(Method::PUT, &format!("/api/preload?src=cam&audio=rtsp://admin:{SECRET}@x")).await;
    assert_eq!(reply.status, StatusCode::BAD_REQUEST);
    bodies.push(reply.text());

    let all = bodies.join("\n");
    assert!(all.contains("fake://***@cam1"), "{all}");
    assert!(!all.contains(SECRET), "a response leaks the password: {all}");
    let logged = logs.text();
    assert!(logged.contains("api: stream refused"), "the refusals are logged: {logged}");
    assert!(!logged.contains(SECRET), "a log line leaks the password: {logged}");
}

#[tokio::test]
async fn streams_show_every_source_its_state_and_its_redacted_error() {
    // go2rtc lists a running source as a producer and says nothing of one that
    // keeps failing; oxrtc lists every source with its state and counters.
    let api = Api::new();
    let mut down = Script::video(Codec::H264, 0);
    down.fails_with = Some("camera refused rtsp://admin:hunter2@10.0.0.9/live".into());
    api.factory.add("down", down);
    let reply = api.call(Method::PUT, "/api/streams?name=cam&src=fake:down").await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text());
    let held = api.call(Method::PUT, "/api/preload?src=cam&video").await;
    assert_eq!(held.status, StatusCode::INTERNAL_SERVER_ERROR, "{}", held.text());

    let info = api.call(Method::GET, "/api/streams?src=cam").await;
    assert!(!info.text().contains("hunter2"), "a password in /api/streams: {}", info.text());
    let source = &info.json()["sources"][0];
    assert_eq!(source["url"], "fake:down");
    assert!(source["failures"].as_u64().unwrap() >= 1, "{source}");
    assert!(source["error"].as_str().unwrap().contains("rtsp://***@10.0.0.9/live"), "{source}");
    assert!(source.get("last_frame_age_ms").is_none(), "no frame yet: {source}");
}
