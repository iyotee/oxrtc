//! The HTTP driver against scripted webcams on 127.0.0.1:0: a stream and a
//! picture per request, the way the cameras the owner has serve them,
//! authentication, redirects, failures and what they say about themselves.
//! Needs no external binary.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]

mod common;

use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use common::{Reply, Server, digest_ok, failure_of, hub, ok, picture, status, stream_of, watch};
use oxrtc_core::{Codec, SourceState};

#[tokio::test]
async fn a_stream_of_pictures_is_published_as_jpeg_video() {
    let pictures = vec![picture(1, 3000), picture(2, 10), picture(3, 70_000)];
    let served = pictures.clone();
    let camera =
        Server::start(move |_| stream_of("--myboundary", "--myboundary", served.clone(), Duration::from_millis(20)))
            .await;
    let hub = hub();
    let stream = hub.upsert("cam", &[camera.url("/mjpg/video.mjpg")]).unwrap();
    let (tracks, got) = watch(&stream, 6).await;
    assert_eq!(tracks.len(), 1);
    assert_eq!((tracks[0].codec, tracks[0].clock_rate), (Codec::Jpeg, 90_000));
    // In order, whole, whatever frame the viewer started at.
    let start = pictures.iter().position(|p| *p == got[0]).unwrap();
    for (i, picture) in got.iter().enumerate() {
        assert_eq!(*picture, pictures[(start + i) % pictures.len()], "picture {i}");
    }
    let snapshot = stream.snapshot();
    assert_eq!(snapshot.sources[0].tracks[0].codec, Codec::Jpeg);
    assert!(snapshot.sources[0].frames >= 6);
    let request = &camera.requests()[0];
    assert_eq!(request.path, "/mjpg/video.mjpg");
    assert_eq!(request.header("host"), Some(format!("127.0.0.1:{}", camera.port).as_str()));
    assert!(request.header("user-agent").unwrap().starts_with("oxrtc/"));
}

#[tokio::test]
async fn the_source_declares_its_codec_before_it_runs() {
    let camera = Server::start(|_| Reply::Raw(status(404, &[]))).await;
    let hub = hub();
    let stream = hub.upsert("cam", &[camera.url("/")]).unwrap();
    // A viewer that wants H.264 finds no source for it, without the camera being asked.
    let want = oxrtc_core::Want::from_query("video=h264").unwrap();
    let refused = stream.subscribe(&want, oxrtc_core::ConsumerTag::new("t")).await;
    assert!(refused.is_err());
    assert!(camera.requests().is_empty(), "the camera was asked: {:?}", camera.requests());
}

#[tokio::test]
async fn a_chunked_stream_with_no_declared_boundary_and_no_part_lengths_is_read() {
    let pictures = vec![picture(7, 1000), picture(8, 2000)];
    let served = pictures.clone();
    let camera = Server::start(move |_| {
        let served = served.clone();
        let mut i = 0usize;
        Reply::Stream {
            head: b"HTTP/1.1 200 OK\r\nContent-Type: multipart/x-mixed-replace\r\nTransfer-Encoding: chunked\r\n\r\n"
                .to_vec(),
            next: Box::new(move || {
                i += 1;
                // One part, as one chunk: no length, a line break after the picture.
                let mut body = b"--cam\r\nContent-Type: image/jpeg\r\n\r\n".to_vec();
                body.extend_from_slice(&served[(i - 1) % served.len()]);
                body.extend_from_slice(b"\r\n");
                let mut chunk = format!("{:x}\r\n", body.len()).into_bytes();
                chunk.extend(body);
                chunk.extend_from_slice(b"\r\n");
                chunk
            }),
            every: Duration::from_millis(20),
        }
    })
    .await;
    let stream = hub().upsert("cam", &[camera.url("/")]).unwrap();
    let (_, got) = watch(&stream, 4).await;
    // A part without a length is complete when the next delimiter arrives.
    let start = pictures.iter().position(|p| *p == got[0]).unwrap();
    for (i, picture) in got.iter().enumerate() {
        assert_eq!(*picture, pictures[(start + i) % 2], "picture {i}");
    }
}

/// The response of an Axis camera behind an `https` page, as the owner's `camera_4_src`
/// answered on 2026-11-16 (the head byte for byte, the parts' framing, one picture a
/// second): HTTP/1.0, cache headers, a space after the `;`, `Connection: close`, no
/// length for the whole response, a length for each part.
#[tokio::test]
async fn the_head_of_a_public_axis_webcam_is_read_as_it_is_sent() {
    let pictures = vec![picture(1, 86_000), picture(2, 86_100), picture(3, 86_200)];
    let served = pictures.clone();
    let camera = Server::start(move |_| {
        let served = served.clone();
        let mut i = 0usize;
        Reply::Stream {
            head: b"HTTP/1.0 200 OK\r\nDate: Mon, 16 Nov 2026 15:18:24 GMT\r\nCache-Control: no-cache\r\n\
                    Pragma: no-cache\r\nExpires: Thu, 01 Dec 1994 16:00:00 GMT\r\n\
                    Content-Type: multipart/x-mixed-replace; boundary=myboundary\r\n\
                    Strict-Transport-Security: max-age=15552000; includeSubDomains\r\nConnection: close\r\n\r\n"
                .to_vec(),
            next: Box::new(move || {
                i += 1;
                common::part("--myboundary", &served[(i - 1) % served.len()])
            }),
            every: Duration::from_millis(30),
        }
    })
    .await;
    let stream = hub().upsert("cam", &[camera.url("/view/")]).unwrap();
    let (tracks, got) = watch(&stream, 4).await;
    assert_eq!(tracks[0].codec, Codec::Jpeg);
    let start = pictures.iter().position(|p| *p == got[0]).unwrap();
    for (i, picture) in got.iter().enumerate() {
        assert_eq!(*picture, pictures[(start + i) % 3], "picture {i}");
    }
    assert_eq!(camera.requests()[0].path, "/view/");
    // One request: the response is the whole stream, nothing is asked for again.
    assert_eq!(camera.requests().len(), 1);
}

#[tokio::test]
async fn one_picture_per_request_is_polled_on_one_connection_at_its_interval() {
    let still = picture(5, 4000);
    let served = still.clone();
    let camera = Server::start(move |_| Reply::Raw(ok("image/jpeg", &served))).await;
    let stream = hub().upsert("cam", &[format!("{}#interval=100", camera.url("/snapshot.jpg?a=1"))]).unwrap();
    let (tracks, got) = watch(&stream, 5).await;
    assert_eq!(tracks[0].codec, Codec::Jpeg);
    assert!(got.iter().all(|p| *p == still));
    let requests = camera.requests();
    assert!(requests.len() >= 5);
    assert!(requests.iter().all(|r| r.connection == 0 && r.path == "/snapshot.jpg?a=1"), "one connection kept");
    for pair in requests.windows(2) {
        let gap = pair[1].at - pair[0].at;
        assert!(gap >= Duration::from_millis(90), "{gap:?} between two requests");
        assert!(gap < Duration::from_millis(600), "{gap:?} between two requests");
    }
    assert_eq!(camera.connections.load(std::sync::atomic::Ordering::SeqCst), 1);
}

#[tokio::test]
async fn a_camera_that_closes_after_each_picture_is_asked_again_on_a_new_connection() {
    let still = picture(5, 400);
    let served = still.clone();
    let camera = Server::start(move |_| {
        let mut out = b"HTTP/1.0 200 OK\r\nContent-Type: image/jpeg\r\n\r\n".to_vec();
        out.extend_from_slice(&served);
        Reply::Close(out)
    })
    .await;
    let stream = hub().upsert("cam", &[format!("{}#interval=20", camera.url("/s.jpg"))]).unwrap();
    let (_, got) = watch(&stream, 4).await;
    assert!(got.iter().all(|p| *p == still));
    assert!(camera.connections.load(std::sync::atomic::Ordering::SeqCst) >= 4);
}

#[tokio::test]
async fn basic_credentials_from_the_url_answer_a_challenge_and_stay_off_the_wire_until_asked() {
    let still = picture(1, 100);
    let wanted = format!("Basic {}", STANDARD.encode("admin:p@ss:w0rd"));
    let camera = Server::start(move |req| match req.header("authorization") {
        Some(a) if a == wanted => Reply::Raw(ok("image/jpeg", &still)),
        _ => Reply::Raw(status(401, &["WWW-Authenticate: Basic realm=\"cam\""])),
    })
    .await;
    let url = format!("http://admin:p%40ss%3Aw0rd@127.0.0.1:{}/s.jpg#interval=50", camera.port);
    let stream = hub().upsert("cam", &[url]).unwrap();
    let (_, got) = watch(&stream, 3).await;
    assert_eq!(got.len(), 3);
    let requests = camera.requests();
    assert!(requests[0].header("authorization").is_none(), "asked without credentials first");
    assert!(requests[1..].iter().all(|r| r.header("authorization").is_some()), "then with them every time");
    assert!(requests.iter().all(|r| !r.path.contains("w0rd") && !r.path.contains("admin")));
    // What the stream lists names no password.
    let shown = format!("{:?}", stream.snapshot());
    assert!(!shown.contains("w0rd"), "{shown}");
}

#[tokio::test]
async fn digest_credentials_are_answered_as_the_rfc_says_and_each_request_counts_up() {
    let still = picture(1, 100);
    let camera = Server::start(move |req| {
        if digest_ok(req.header("authorization"), &req.path, "root", "s3cret", "webcam", "abc123") {
            Reply::Raw(ok("image/jpeg", &still))
        } else {
            Reply::Raw(status(
                401,
                &["WWW-Authenticate: Digest realm=\"webcam\", nonce=\"abc123\", qop=\"auth\", algorithm=MD5"],
            ))
        }
    })
    .await;
    let stream =
        hub().upsert("cam", &[format!("http://root:s3cret@127.0.0.1:{}/image?x=1#interval=50", camera.port)]).unwrap();
    let (_, got) = watch(&stream, 3).await;
    assert_eq!(got.len(), 3);
    let requests = camera.requests();
    let counts: Vec<&str> = requests
        .iter()
        .filter_map(|r| r.header("authorization"))
        .filter_map(|a| a.split("nc=").nth(1).and_then(|r| r.split(',').next()))
        .collect();
    assert!(counts.len() >= 3 && counts[0] != counts[1], "nonce counts {counts:?}");
}

#[tokio::test]
async fn refused_credentials_are_an_authentication_error_that_never_shows_them() {
    let camera = Server::start(|_| Reply::Raw(status(401, &["WWW-Authenticate: Basic realm=\"cam\""]))).await;
    let url = format!("http://admin:hunter2@127.0.0.1:{}/s.jpg", camera.port);
    let stream = hub().upsert("cam", &[url]).unwrap();
    let error = failure_of(&stream, Duration::from_secs(10)).await;
    assert_eq!(error, "authentication refused");
    // Once with the credentials, not in a loop on the connection.
    assert_eq!(camera.requests().iter().filter(|r| r.header("authorization").is_some()).count(), 1);
    assert!(!format!("{:?}", stream.snapshot()).contains("hunter2"));
    // No credentials at all: the same answer.
    let stream = hub().upsert("anon", &[camera.url("/s.jpg")]).unwrap();
    assert_eq!(failure_of(&stream, Duration::from_secs(10)).await, "authentication refused");
}

#[tokio::test]
async fn redirects_are_followed_and_credentials_do_not_follow_them_to_another_origin() {
    let still = picture(1, 100);
    let elsewhere_still = still.clone();
    let elsewhere = Server::start(move |_| Reply::Raw(ok("image/jpeg", &elsewhere_still))).await;
    let target = elsewhere.url("/final.jpg");
    let same = still.clone();
    let camera = Server::start(move |req| match req.path.as_str() {
        "/old" => Reply::Raw(status(302, &["Location: /new.jpg?q=1"])),
        "/new.jpg?q=1" => Reply::Raw(ok("image/jpeg", &same)),
        "/away" => Reply::Raw(status(307, &[&format!("Location: {target}")])),
        _ => Reply::Raw(status(404, &[])),
    })
    .await;
    // Same origin: followed, and the final URL is where the next polls go.
    let stream = hub().upsert("a", &[format!("{}#interval=30", camera.url("/old"))]).unwrap();
    let (_, got) = watch(&stream, 3).await;
    assert!(got.iter().all(|p| *p == still));
    let paths: Vec<String> = camera.requests().iter().map(|r| r.path.clone()).collect();
    assert_eq!(&paths[..2], ["/old", "/new.jpg?q=1"]);
    assert!(paths[2..].iter().all(|p| p == "/new.jpg?q=1"), "{paths:?}");
    // Another origin: followed, credentials left behind.
    let url = format!("http://u:pw@127.0.0.1:{}/away#interval=30", camera.port);
    let stream = hub().upsert("b", &[url]).unwrap();
    let (_, got) = watch(&stream, 2).await;
    assert_eq!(got.len(), 2);
    assert!(elsewhere.requests().iter().all(|r| r.header("authorization").is_none()));
}

#[tokio::test]
async fn a_redirect_loop_is_a_protocol_error() {
    let camera = Server::start(|_| Reply::Raw(status(302, &["Location: /again"]))).await;
    let stream = hub().upsert("cam", &[camera.url("/start")]).unwrap();
    let error = failure_of(&stream, Duration::from_secs(10)).await;
    assert!(error.contains("redirects"), "{error}");
}

#[tokio::test]
async fn what_is_not_a_picture_is_an_error_naming_the_type_and_nothing_of_the_body() {
    let camera = Server::start(|req| match req.path.as_str() {
        "/html" => Reply::Raw(ok("text/html; charset=utf-8", b"<html>secret hunter2 page</html>")),
        "/missing" => Reply::Raw(status(404, &[])),
        "/busy" => Reply::Raw(status(503, &[])),
        _ => Reply::Raw(ok("image/jpeg", b"<html>login</html>")),
    })
    .await;
    let error = |path: &str| {
        let hub = hub();
        let stream = hub.upsert("cam", &[format!("http://user:hunter2@127.0.0.1:{}{path}", camera.port)]).unwrap();
        async move { failure_of(&stream, Duration::from_secs(10)).await }
    };
    let html = error("/html").await;
    assert!(html.contains("text/html") && html.contains("not a Motion JPEG source"), "{html}");
    assert!(!html.contains("hunter2"), "{html}");
    assert_eq!(error("/missing").await, "unreachable: HTTP 404");
    assert_eq!(error("/busy").await, "unreachable: HTTP 503");
}

#[tokio::test]
async fn answers_that_are_never_pictures_stall_the_source() {
    let camera = Server::start(|_| Reply::Raw(ok("image/jpeg", b"<html>login</html>"))).await;
    let stream = hub().upsert("cam", &[format!("{}#timeout=1#interval=50", camera.url("/s.jpg"))]).unwrap();
    let error = failure_of(&stream, Duration::from_secs(10)).await;
    assert_eq!(error, "no media within the stall timeout");
}

#[tokio::test]
async fn a_stream_that_goes_quiet_stalls_at_its_timeout() {
    let camera = Server::start(|_| {
        let mut sent = false;
        Reply::Stream {
            head: b"HTTP/1.0 200 OK\r\nContent-Type: multipart/x-mixed-replace;boundary=f\r\n\r\n".to_vec(),
            next: Box::new(move || {
                if std::mem::replace(&mut sent, true) { b"x".to_vec() } else { common::part("--f", &picture(1, 100)) }
            }),
            every: Duration::from_millis(100),
        }
    })
    .await;
    let stream = hub().upsert("cam", &[format!("{}#timeout=1", camera.url("/"))]).unwrap();
    let started = tokio::time::Instant::now();
    let error = failure_of(&stream, Duration::from_secs(10)).await;
    // One picture, then bytes that never make another: a stall, though the socket is not silent.
    assert_eq!(error, "no media within the stall timeout");
    assert!(started.elapsed() < Duration::from_secs(6), "{:?}", started.elapsed());
}

#[tokio::test]
async fn a_head_that_never_comes_ends_at_the_timeout() {
    let camera = Server::start(|_| Reply::Silent(Vec::new())).await;
    let stream = hub().upsert("cam", &[format!("{}#timeout=1", camera.url("/"))]).unwrap();
    let error = failure_of(&stream, Duration::from_secs(10)).await;
    assert!(error.contains("no answer within 1s"), "{error}");
}

#[tokio::test]
async fn a_part_over_the_bound_ends_the_run() {
    let camera = Server::start(|_| {
        Reply::Silent(
            b"HTTP/1.0 200 OK\r\nContent-Type: multipart/x-mixed-replace;boundary=f\r\n\r\n--f\r\nContent-Type: image/jpeg\r\nContent-Length: 99999999\r\n\r\n\xff\xd8"
                .to_vec(),
        )
    })
    .await;
    let stream = hub().upsert("cam", &[camera.url("/")]).unwrap();
    let error = failure_of(&stream, Duration::from_secs(10)).await;
    assert!(error.contains("a part over"), "{error}");
}

#[tokio::test]
async fn a_closing_delimiter_ends_the_source_and_it_comes_back() {
    let pictures = picture(1, 100);
    let camera = Server::start(move |_| {
        let mut out = b"HTTP/1.0 200 OK\r\nContent-Type: multipart/x-mixed-replace;boundary=f\r\n\r\n".to_vec();
        out.extend(common::part("--f", &pictures));
        out.extend_from_slice(b"--f--\r\n");
        Reply::Close(out)
    })
    .await;
    let stream = hub().upsert("cam", &[camera.url("/")]).unwrap();
    let (_, got) = watch(&stream, 3).await;
    assert_eq!(got.len(), 3);
    assert!(camera.connections.load(std::sync::atomic::Ordering::SeqCst) >= 2, "asked again after the end");
    assert!(matches!(
        stream.snapshot().sources[0].state,
        SourceState::Running | SourceState::Retrying { .. } | SourceState::Connecting { .. }
    ));
}

#[tokio::test]
async fn a_camera_nobody_listens_on_is_unreachable_without_leaking_the_url() {
    let port = {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.local_addr().unwrap().port()
    };
    let stream =
        hub().upsert("cam", &[format!("http://admin:hunter2@127.0.0.1:{port}/s.jpg?password=hunter3")]).unwrap();
    let error = failure_of(&stream, Duration::from_secs(10)).await;
    assert!(!error.contains("hunter"), "{error}");
    let listed = format!("{:?}", stream.snapshot());
    assert!(!listed.contains("hunter"), "{listed}");
}
