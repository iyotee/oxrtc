//! The built binary's `POST /api/restart` (in this process, the file read
//! again), `POST /api/exit` (its code, after its answer) and `GET /api/log`
//! (M5 F7, D48). Needs no camera, FFmpeg or go2rtc.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]

mod common;

use std::time::{Duration, Instant};

use common::launch::{API_PASS, API_USER, Ports, wait_healthy};
use common::*;

#[tokio::test]
async fn restart_reads_the_file_again_and_exit_ends_with_its_code() {
    let dir = temp_dir("control");
    let ports = Ports::free();
    let text = format!(
        "api:\n  listen: \"127.0.0.1:{api}\"\n  username: \"{API_USER}\"\n  password: \"{API_PASS}\"\n  \
         allow_config_write: true\n  allow_restart: true\n  allow_exit: true\n\
         rtsp:\n  listen: \"127.0.0.1:{rtsp}\"\n\
         webrtc:\n  listen: \"127.0.0.1:{webrtc}\"\n  candidates: []\n\
         streams:\n  first: rtsp://127.0.0.1:9/x\n",
        api = ports.api,
        rtsp = ports.rtsp,
        webrtc = ports.webrtc,
    );
    let path = dir.join("oxrtc.yaml");
    std::fs::write(&path, text).unwrap();
    let mut oxrtc = Oxrtc::start(&path);
    let api = ports.api();
    wait_healthy(api).await;
    let pid = oxrtc.child.id();

    // go2rtc's facts on `GET /api`, the file's path among them.
    let info = Request::new("GET", "/api").auth(API_USER, API_PASS).send(api).await.json();
    assert_eq!(info["config_path"], std::path::absolute(&path).unwrap().display().to_string(), "{info}");
    assert!(info["version"].as_str().unwrap().starts_with("1.9.15+oxrtc."), "{info}");

    // The log: go2rtc's JSON lines, the start among them.
    let log = Request::new("GET", "/api/log").auth(API_USER, API_PASS).send(api).await;
    assert_eq!(log.status, 200);
    assert_eq!(log.header("content-type"), Some("application/jsonlines"));
    let lines: Vec<serde_json::Value> = log.text().lines().map(|l| serde_json::from_str(l).unwrap()).collect();
    assert!(lines.iter().any(|l| l["message"] == "listen" && l["module"] == "api"), "{}", log.text());

    // A stream added to the file, then a restart: the running server takes it.
    let change = b"streams:\n  second: rtsp://127.0.0.1:9/y\n";
    let reply =
        Request::new("PATCH", "/api/config").auth(API_USER, API_PASS).body("application/yaml", change).send(api).await;
    assert_eq!(reply.status, 200, "{}", reply.text());
    let streams = Request::new("GET", "/api/streams").auth(API_USER, API_PASS).send(api).await.json();
    assert!(streams.get("second").is_none(), "not before the restart: {streams}");
    let reply = Request::new("POST", "/api/restart").auth(API_USER, API_PASS).send(api).await;
    assert_eq!((reply.status, reply.text().as_str()), (200, "OK"), "answered before it acts");
    let started = Instant::now();
    tokio::time::sleep(Duration::from_millis(200)).await;
    wait_healthy(api).await;
    eprintln!("up again {:?} after the restart's answer", started.elapsed());
    let streams = Request::new("GET", "/api/streams").auth(API_USER, API_PASS).send(api).await.json();
    assert!(streams.get("first").is_some() && streams.get("second").is_some(), "{streams}");
    assert_eq!(oxrtc.child.id(), pid, "the same process");
    assert!(oxrtc.child.try_wait().unwrap().is_none());

    // Exit: answered, then the process ends with the code asked.
    let reply = Request::new("POST", "/api/exit?code=7").auth(API_USER, API_PASS).send(api).await;
    assert_eq!((reply.status, reply.text().as_str()), (200, "OK"));
    let status = tokio::time::timeout(Duration::from_secs(10), oxrtc.child.wait()).await.unwrap().unwrap();
    assert_eq!(status.code(), Some(7));
    let lines = oxrtc.drain().await;
    assert!(lines.iter().any(|l| l.contains("restarting from")), "{lines:?}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn a_restart_that_cannot_start_comes_back_on_the_configuration_that_ran() {
    // The restart's check reads the file; only the start itself finds a port
    // another program holds. go2rtc goes on without that listener; oxrtc
    // ended here with code 1 (F7 review), the API gone.
    let dir = temp_dir("control-fallback");
    let ports = Ports::free();
    let file = |rtsp: u16| {
        format!(
            "api:\n  listen: \"127.0.0.1:{api}\"\n  username: \"{API_USER}\"\n  password: \"{API_PASS}\"\n  \
             allow_config_write: true\n  allow_restart: true\n\
             rtsp:\n  listen: \"127.0.0.1:{rtsp}\"\n\
             webrtc:\n  listen: \"127.0.0.1:{webrtc}\"\n  candidates: []\n\
             streams:\n  first: rtsp://127.0.0.1:9/x\n",
            api = ports.api,
            webrtc = ports.webrtc,
        )
    };
    let path = dir.join("oxrtc.yaml");
    std::fs::write(&path, file(ports.rtsp)).unwrap();
    let mut oxrtc = Oxrtc::start(&path);
    let api = ports.api();
    wait_healthy(api).await;
    let pid = oxrtc.child.id();

    // Another program holds the port the new file names.
    let held = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let taken = held.local_addr().unwrap().port();
    let next = file(taken);
    let reply = Request::new("POST", "/api/config")
        .auth(API_USER, API_PASS)
        .body("application/yaml", next.as_bytes())
        .send(api)
        .await;
    assert_eq!(reply.status, 200, "{}", reply.text());
    let reply = Request::new("POST", "/api/restart").auth(API_USER, API_PASS).send(api).await;
    assert_eq!((reply.status, reply.text().as_str()), (200, "OK"));
    tokio::time::sleep(Duration::from_millis(300)).await;

    // Back on the configuration that ran, in the same process.
    wait_healthy(api).await;
    assert!(oxrtc.child.try_wait().unwrap().is_none(), "the process goes on");
    assert_eq!(oxrtc.child.id(), pid);
    let info = Request::new("GET", "/api").auth(API_USER, API_PASS).send(api).await.json();
    assert_eq!(info["rtsp"]["listen"], format!("127.0.0.1:{}", ports.rtsp), "{info}");
    let streams = Request::new("GET", "/api/streams").auth(API_USER, API_PASS).send(api).await.json();
    assert!(streams.get("first").is_some(), "{streams}");
    // The file is the operator's, as written: nothing rewritten behind them.
    assert_eq!(std::fs::read_to_string(&path).unwrap(), next);
    drop(held);
    oxrtc.child.kill().await.unwrap();
    let lines = oxrtc.drain().await;
    assert!(lines.iter().any(|l| l.contains("the configuration that ran before")), "{lines:?}");
    let _ = std::fs::remove_dir_all(&dir);
}
