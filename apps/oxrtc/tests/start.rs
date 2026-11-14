//! Starting and stopping in-process: what is refused before anything is
//! served, and how long a stop takes.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]

use std::time::{Duration, Instant};

use oxrtc::{App, StartError};
use oxrtc_api::config::{Config, parse};

fn config(api: &str, rtsp: &str, webrtc: &str, extra: &str) -> Config {
    let text = format!(
        "api:\n  listen: \"{api}\"\n  username: \"u\"\n  password: \"p\"\n\
         rtsp:\n  listen: \"{rtsp}\"\nwebrtc:\n  listen: \"{webrtc}\"\n{extra}"
    );
    parse(&text).unwrap().0
}

const ANY: &str = "127.0.0.1:0";

#[tokio::test]
async fn a_port_in_use_is_an_error_naming_the_address() {
    let taken = std::net::TcpListener::bind(ANY).unwrap();
    let addr = taken.local_addr().unwrap();
    for (api, rtsp) in [(addr.to_string(), ANY.to_owned()), (ANY.to_owned(), addr.to_string())] {
        let error = App::start(&config(&api, &rtsp, ANY, ""), None).await.unwrap_err();
        let StartError::Bind { addr: failed, .. } = &error else { panic!("{error}") };
        assert_eq!(*failed, addr);
        assert!(error.to_string().contains(&addr.to_string()), "{error}");
    }
    let udp = std::net::UdpSocket::bind(ANY).unwrap();
    let busy = udp.local_addr().unwrap();
    let error = App::start(&config(ANY, ANY, &busy.to_string(), ""), None).await.unwrap_err();
    assert!(error.to_string().contains(&busy.to_string()) && error.to_string().contains("webrtc.listen"), "{error}");
}

#[tokio::test]
async fn the_api_needs_credentials_unless_explicitly_opened() {
    let text = format!("api:\n  listen: \"{ANY}\"\nrtsp:\n  listen: \"{ANY}\"\nwebrtc:\n  listen: \"{ANY}\"\n");
    let open = parse(&text).unwrap().0;
    let error = App::start(&open, None).await.unwrap_err();
    assert!(matches!(error, StartError::Config(_)), "{error}");
    assert!(error.to_string().contains("api.username"), "{error}");
    let opened = parse(&text.replace("api:\n", "api:\n  allow_unauthenticated: true\n")).unwrap().0;
    assert!(opened.api.allow_unauthenticated);
    let app = App::start(&opened, None).await.unwrap();
    assert!(app.shutdown().await);
}

#[tokio::test]
async fn a_bad_listen_address_is_refused_before_binding() {
    let error = App::start(&config(ANY, "8554", ANY, ""), None).await.unwrap_err();
    assert!(error.to_string().starts_with("rtsp.listen"), "{error}");
    let error = App::start(&config("", ANY, ANY, ""), None).await.unwrap_err();
    assert!(error.to_string().contains("api.listen"), "{error}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_refused_stream_does_not_stop_the_others() {
    let streams = "streams:\n  a: [\"rtsp://user:hunter2@127.0.0.1:9/x\"]\n  bad: [\"exec:calc\"]\n  \
                   worse: [\"echo:hi\"]\n  c: [\"ffmpeg:a#video=h264\"]\n  d: [\"rtsp://127.0.0.1:1/a?video\"]\n";
    let app = App::start(&config(ANY, ANY, ANY, streams), None).await.unwrap();
    let mut names: Vec<String> = app.hub().streams().iter().map(|s| s.name().to_owned()).collect();
    names.sort();
    assert_eq!(names, ["a", "c", "d"]);
    assert!(app.shutdown().await);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_loopback_url_on_our_own_port_is_read_in_process() {
    // The RTSP port is only known once bound: the rtsp factory is set after
    // it. A loopback URL on that port is an in-process read, so naming the
    // stream itself is a self-reference and refused; on another port it is
    // a camera like any other.
    let probe = std::net::TcpListener::bind(ANY).unwrap();
    let port = probe.local_addr().unwrap().port();
    drop(probe);
    let streams = format!(
        "streams:\n  loop: [\"rtsp://127.0.0.1:{port}/loop?video\"]\n  other: [\"rtsp://127.0.0.1:{}/other?video\"]\n",
        port.wrapping_add(1).max(1)
    );
    let app = App::start(&config(ANY, &format!("127.0.0.1:{port}"), ANY, &streams), None).await.unwrap();
    assert_eq!(app.addresses().rtsp.port(), port);
    assert!(app.hub().get("loop").is_none(), "our own URL naming itself is a self-reference");
    assert!(app.hub().get("other").is_some(), "another port is a camera");
    assert!(app.shutdown().await);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shutdown_ends_every_listener_well_within_the_deadline() {
    let app = App::start(&config(ANY, ANY, ANY, ""), None).await.unwrap();
    let addrs = app.addresses();
    assert!(tokio::net::TcpStream::connect(addrs.api).await.is_ok());
    assert!(tokio::net::TcpStream::connect(addrs.rtsp).await.is_ok());
    let started = Instant::now();
    assert!(app.shutdown().await, "every task ended within the deadline");
    assert!(started.elapsed() < Duration::from_secs(4), "{:?}", started.elapsed());
    // The listeners are closed: binding the same ports works again.
    assert!(std::net::TcpListener::bind(addrs.api).is_ok());
    assert!(std::net::TcpListener::bind(addrs.rtsp).is_ok());
    assert!(std::net::UdpSocket::bind(addrs.webrtc).is_ok());
}
