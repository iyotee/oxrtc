//! The application's routes that change the server: `/api/config`,
//! `/api/log`, `/api/restart`, `/api/exit` - off unless the file allows them,
//! a written file always one this server starts from (M5 F7, D48).
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing, clippy::panic)]

mod common;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use axum::http::{Method, StatusCode, header};
use common::*;
use oxrtc_api::config::Permissions;
use oxrtc_api::{Command, LogRing};

const FILE: &str = "# the operator's comment\napi:\n  username: admin\n  password: api-s3cret\n  allow_config_write: true\n  allow_restart: true\nstreams:\n  # a camera\n  cam: rtsp://user:pw@192.168.1.10/x\nffmpeg:\n  bin: /usr/bin/ffmpeg\n";

fn scratch(tag: &str) -> PathBuf {
    static DRAWS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let draw = DRAWS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("oxrtc-api-control-{tag}-{}-{draw}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn with_file(tag: &str, permissions: Permissions) -> (Api, PathBuf) {
    let dir = scratch(tag);
    let path = dir.join("oxrtc.yaml");
    std::fs::write(&path, FILE).unwrap();
    let file = path.clone();
    (Api::build(move |s| s.with_config_path(file).with_permissions(permissions)), path)
}

fn allow_all() -> Permissions {
    Permissions { stream_changes: true, config_read: true, config_write: true, restart: true, exit: true }
}

async fn body(api: &Api, method: Method, uri: &str, body: &[u8]) -> Reply {
    api.send(method, uri, body.to_vec(), Some(&basic(USER, PASS))).await
}

#[tokio::test]
async fn every_route_that_changes_the_server_is_off_by_default_and_named() {
    let (api, path) = with_file("off", Permissions::default());
    for (method, uri, key) in [
        (Method::GET, "/api/config", "allow_config_read"),
        (Method::POST, "/api/config", "allow_config_write"),
        (Method::PATCH, "/api/config", "allow_config_write"),
        (Method::POST, "/api/restart", "allow_restart"),
        (Method::POST, "/api/exit?code=0", "allow_exit"),
    ] {
        let reply = body(&api, method.clone(), uri, b"api: {}\n").await;
        assert_eq!(reply.status, StatusCode::FORBIDDEN, "{method} {uri}");
        assert!(reply.text().contains(&format!("api.{key}")), "{method} {uri}: {}", reply.text());
    }
    assert_eq!(std::fs::read_to_string(&path).unwrap(), FILE, "untouched");
}

#[tokio::test]
async fn the_file_is_read_as_written_when_allowed() {
    let (api, _path) = with_file("read", Permissions { config_read: true, ..Permissions::default() });
    let reply = api.call(Method::GET, "/api/config").await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(reply.content_type(), "application/yaml");
    assert_eq!(reply.text(), FILE, "comments and credentials included, as go2rtc's");
    // Without a file, go2rtc's 410.
    let api = Api::build(|s| s.with_permissions(allow_all()));
    assert_eq!(api.call(Method::GET, "/api/config").await.status, StatusCode::GONE);
    assert_eq!(body(&api, Method::POST, "/api/config", FILE.as_bytes()).await.status, StatusCode::GONE);
}

#[tokio::test]
async fn post_writes_a_file_this_server_starts_from_and_nothing_else() {
    let (api, path) = with_file("post", allow_all());
    // A valid file, its comments kept (it is written as sent).
    let next = FILE.replace(
        "cam: rtsp://user:pw@192.168.1.10/x",
        "cam: rtsp://user:pw@192.168.1.11/x\n  # added\n  two: rtsp://192.168.1.12/y",
    );
    let reply = body(&api, Method::POST, "/api/config", next.as_bytes()).await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text());
    assert_eq!(std::fs::read_to_string(&path).unwrap(), next);
    // Larger than a request body elsewhere (64 KiB), within the file's 1 MiB.
    let big = format!("{next}# {}\n", "x".repeat(200 * 1024));
    assert_eq!(body(&api, Method::POST, "/api/config", big.as_bytes()).await.status, StatusCode::OK);
    std::fs::write(&path, &next).unwrap();
    for (case, text, status) in [
        ("not YAML", "streams: [unclosed".to_owned(), StatusCode::BAD_REQUEST),
        ("not a configuration", "api: [1, 2]\n".to_owned(), StatusCode::BAD_REQUEST),
        (
            "no credential: the server would not start",
            "api:\n  allow_config_write: true\n  allow_restart: true\nffmpeg:\n  bin: /usr/bin/ffmpeg\n".to_owned(),
            StatusCode::BAD_REQUEST,
        ),
        (
            "a permission turned on",
            next.replace("allow_restart: true", "allow_restart: true\n  allow_exit: true"),
            StatusCode::FORBIDDEN,
        ),
        ("a permission turned off", next.replace("  allow_restart: true\n", ""), StatusCode::FORBIDDEN),
        (
            "the API opened",
            next.replace("password: api-s3cret", "password: api-s3cret\n  allow_unauthenticated: true"),
            StatusCode::FORBIDDEN,
        ),
        ("another program as FFmpeg", next.replace("/usr/bin/ffmpeg", "/bin/sh"), StatusCode::FORBIDDEN),
        ("a template of FFmpeg arguments", format!("{next}  h264: -i /etc/shadow -f null -\n"), StatusCode::FORBIDDEN),
        (
            "loopback RTSP readers let in unchecked",
            format!("{next}rtsp:\n  local_auth: false\n"),
            StatusCode::FORBIDDEN,
        ),
        ("too large", format!("{next}# {}\n", "x".repeat(1024 * 1024)), StatusCode::PAYLOAD_TOO_LARGE),
    ] {
        let reply = body(&api, Method::POST, "/api/config", text.as_bytes()).await;
        assert_eq!(reply.status, status, "{case}: {}", reply.text());
        assert!(!reply.text().contains("api-s3cret") && !reply.text().contains("pw@"), "{case}: {}", reply.text());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), next, "{case}: the file untouched");
    }
    // Written by renaming a file beside it: nothing else left there.
    let dir = path.parent().unwrap();
    assert_eq!(std::fs::read_dir(dir).unwrap().count(), 1, "{:?}", std::fs::read_dir(dir).unwrap().collect::<Vec<_>>());
}

#[tokio::test]
async fn patch_merges_as_go2rtc_does() {
    let (api, path) = with_file("patch", allow_all());
    let reply = body(&api, Method::PATCH, "/api/config", b"streams:\n  added: rtsp://192.168.1.13/z\n").await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text());
    let written = std::fs::read_to_string(&path).unwrap();
    let (config, _) = oxrtc_api::config::parse(&written).unwrap();
    let names: Vec<&str> = config.streams.iter().map(|(name, _)| name.as_str()).collect();
    assert_eq!(names, ["cam", "added"], "merged into the file's streams: {written}");
    assert_eq!(config.api.password.as_deref(), Some("api-s3cret"));
    // Written again from the merged tree: the comments are lost, as go2rtc loses them.
    assert!(!written.contains("# a camera"), "{written}");
    for (case, change, status) in [
        ("a list", "[1, 2]", StatusCode::BAD_REQUEST),
        ("not YAML", "a: [", StatusCode::BAD_REQUEST),
        ("a permission", "api:\n  allow_exit: true\n", StatusCode::FORBIDDEN),
        ("FFmpeg", "ffmpeg:\n  bin: /bin/sh\n", StatusCode::FORBIDDEN),
    ] {
        let reply = body(&api, Method::PATCH, "/api/config", change.as_bytes()).await;
        assert_eq!(reply.status, status, "{case}: {}", reply.text());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), written, "{case}: the file untouched");
    }
}

#[tokio::test]
async fn the_log_is_go2rtcs_json_lines_and_can_be_cleared() {
    let api = Api::new();
    assert_eq!(api.call(Method::GET, "/api/log").await.status, StatusCode::NOT_IMPLEMENTED, "no log kept");
    let ring = Arc::new(LogRing::default());
    ring.push_event(&oxrtc_api::LogEvent { level: "info", time_ms: 1, module: "app", message: "oxrtc", fields: &[] });
    let kept = ring.clone();
    let api = Api::build(move |s| s.with_logs(kept));
    let reply = api.call(Method::GET, "/api/log").await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(reply.content_type(), "application/jsonlines");
    // One JSON object a line, as go2rtc's `log.html` reads them.
    let text = reply.text();
    assert!(text.ends_with('\n') && text.lines().count() == 1, "{text}");
    let line: serde_json::Value = serde_json::from_str(text.trim_end()).unwrap();
    assert_eq!(line, serde_json::json!({"level": "info", "time": 1, "module": "app", "message": "oxrtc"}));
    let reply = api.call(Method::DELETE, "/api/log").await;
    assert_eq!((reply.status, reply.text().as_str()), (StatusCode::OK, "OK"));
    // The clearing is itself logged by the server; the ring held nothing else.
    assert!(!ring.text().contains("oxrtc"), "{}", ring.text());
    assert_eq!(api.call(Method::POST, "/api/log").await.status, StatusCode::METHOD_NOT_ALLOWED);
}

#[tokio::test]
async fn restart_and_exit_answer_before_they_act() {
    let (commands, mut received) = tokio::sync::mpsc::channel(1);
    let (api, path) = with_file("restart", allow_all());
    let file = path.clone();
    let api = Api {
        router: oxrtc_api::router(
            oxrtc_api::ApiState::new(api.hub.clone(), "t", auth())
                .with_config_path(file)
                .with_permissions(allow_all())
                .with_commands(commands),
        ),
        ..api
    };
    // go2rtc's checks of the code.
    for uri in ["/api/exit", "/api/exit?code=126", "/api/exit?code=-1", "/api/exit?code=x"] {
        let reply = api.call(Method::POST, uri).await;
        assert_eq!(reply.status, StatusCode::BAD_REQUEST, "{uri}");
        assert_eq!(reply.text(), "Code must be in the range [0, 125]");
    }
    assert!(received.try_recv().is_err(), "nothing asked");
    // Answered, and the command sent once the answer is read.
    let reply = api.call(Method::POST, "/api/exit?code=7").await;
    assert_eq!((reply.status, reply.text().as_str()), (StatusCode::OK, "OK"));
    assert_eq!(tokio::time::timeout(Duration::from_secs(1), received.recv()).await.unwrap(), Some(Command::Exit(7)));
    let reply = api.call(Method::POST, "/api/restart").await;
    assert_eq!((reply.status, reply.text().as_str()), (StatusCode::OK, "OK"));
    assert_eq!(tokio::time::timeout(Duration::from_secs(1), received.recv()).await.unwrap(), Some(Command::Restart));
    // A file the server would not start from: refused, and the server goes on.
    std::fs::write(&path, "api:\n  allow_restart: true\n").unwrap();
    let reply = api.call(Method::POST, "/api/restart").await;
    assert_eq!(reply.status, StatusCode::BAD_REQUEST, "{}", reply.text());
    assert!(reply.text().contains("restart refused"), "{}", reply.text());
    assert!(received.try_recv().is_err());
    assert_eq!(api.call(Method::GET, "/api/restart").await.status, StatusCode::METHOD_NOT_ALLOWED);
    // Without the application behind it: `501`.
    let api = Api::build(|s| s.with_permissions(allow_all()));
    assert_eq!(api.call(Method::POST, "/api/exit?code=0").await.status, StatusCode::NOT_IMPLEMENTED);
    let _ = header::CONTENT_TYPE;
}

#[tokio::test]
async fn put_and_delete_write_the_file_as_go2rtc_does_when_the_file_allows_it() {
    // Not allowed: the stream changes, the file does not (openIVR's case).
    let (api, path) = with_file("persist-off", Permissions::default());
    api.factory.add("cam1", Script::video(oxrtc_core::Codec::H264, 0));
    assert_eq!(api.call(Method::PUT, "/api/streams?name=added&src=fake:cam1").await.status, StatusCode::OK);
    assert_eq!(std::fs::read_to_string(&path).unwrap(), FILE);

    let (api, path) = with_file("persist-on", allow_all());
    api.factory.add("cam1", Script::video(oxrtc_core::Codec::H264, 0));
    let reply = api.call(Method::PUT, "/api/streams?name=added&src=fake:cam1&src=fake:cam1%23timeout%3D5").await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text());
    let written = std::fs::read_to_string(&path).unwrap();
    assert!(
        written.starts_with("# the operator's comment\n") && written.contains("  # a camera\n"),
        "comments kept: {written}"
    );
    let (config, _) = oxrtc_api::config::parse(&written).unwrap();
    assert_eq!(
        config.streams,
        [
            ("cam".to_owned(), vec!["rtsp://user:pw@192.168.1.10/x".to_owned()]),
            ("added".to_owned(), vec!["fake:cam1".to_owned(), "fake:cam1#timeout=5".to_owned()]),
        ]
    );
    // Removed from the file with the stream.
    assert_eq!(api.call(Method::DELETE, "/api/streams?src=added").await.status, StatusCode::OK);
    assert_eq!(std::fs::read_to_string(&path).unwrap(), FILE, "back to the file as it was");
    // A flow-style section is written from the parsed tree (its comments lost).
    std::fs::write(
        &path,
        FILE.replace(
            "streams:\n  # a camera\n  cam: rtsp://user:pw@192.168.1.10/x\n",
            "streams: {cam: rtsp://user:pw@192.168.1.10/x}\n",
        ),
    )
    .unwrap();
    assert_eq!(api.call(Method::PUT, "/api/streams?name=added&src=fake:cam1").await.status, StatusCode::OK);
    let (config, _) = oxrtc_api::config::parse(&std::fs::read_to_string(&path).unwrap()).unwrap();
    assert_eq!(config.streams.len(), 2, "{:?}", config.streams);
}
