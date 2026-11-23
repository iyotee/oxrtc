//! `GET /api/stream.mjpeg` (F5, D46): go2rtc's headers and parts, byte for
//! byte, each part's delimiter sent after it; a webcam's pictures passed on
//! with the standard Huffman tables written in; the transcode of a stream
//! with no JPEG, one source for every reader, listed with the stream; the
//! refusals; the bound it shares with `stream.mp4`; the end with its client
//! over TCP, with the server's stop and with a silent source; and a slow
//! reader that loses pictures and says so.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing, clippy::panic)]

mod common;

use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use bytes::Bytes;
use common::*;
use futures_util::StreamExt;
use oxrtc_api::{ApiState, Mp4Limits, ServeLimits};
use oxrtc_core::jpeg::header as jpeg_header;
use oxrtc_core::{
    BoxFuture, Codec, Declared, FactoryCtx, Hub, MediaKind, SourceDriver, SourceFactory, SourceSpec, SpecError,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tower::ServiceExt;

/// `ffmpeg:<stream>#video=mjpeg` played by a webcam's script: the transcode
/// a stream with no JPEG is given, without FFmpeg.
struct FakeTranscoder(Arc<FakeFactory>, std::sync::Mutex<Vec<String>>);

impl SourceFactory for FakeTranscoder {
    fn schemes(&self) -> &[&'static str] {
        &["ffmpeg"]
    }

    fn build(&self, spec: &SourceSpec, ctx: &FactoryCtx) -> Result<Arc<dyn SourceDriver>, SpecError> {
        self.1.lock().unwrap().push(format!("{} for {}", spec.redacted(), ctx.stream_name()));
        self.0.build(&SourceSpec::parse("fake:webcam").unwrap(), ctx)
    }
}

/// A source declaring JPEG whose runs end at once: what the transcode's
/// readers see when FFmpeg fails.
struct Declares;

impl SourceDriver for Declares {
    fn declared(&self) -> Option<Declared> {
        Some(vec![(MediaKind::Video, Codec::Jpeg)])
    }
    fn run(
        self: Arc<Self>,
        _sink: oxrtc_core::ProducerSink,
        cancel: oxrtc_core::CancellationToken,
    ) -> BoxFuture<'static, Result<(), oxrtc_core::SourceError>> {
        Box::pin(async move {
            cancel.cancelled().await;
            Ok(())
        })
    }
}

/// `webcam`: JPEG pictures every 20 ms; `camera`: H.264 alone; `slow`:
/// a picture every 100 ms; `few`: 5 pictures. With `transcoder`, the
/// `ffmpeg:` scheme encodes a stream with no JPEG into `webcam`'s pictures.
fn setup(limits: Mp4Limits, transcoder: bool) -> (ApiState, Arc<Hub>, Arc<FakeTranscoder>) {
    let factory = Arc::new(FakeFactory::default());
    factory.add("webcam", Script::video(Codec::Jpeg, 10_000));
    factory.add("camera", Script::video(Codec::H264, 10_000));
    factory.add("slow", Script { interval: Duration::from_millis(100), ..Script::video(Codec::Jpeg, 10_000) });
    factory.add("few", Script::video(Codec::Jpeg, 5));
    factory.add(
        "big",
        Script { video_bytes: 200_000, interval: Duration::from_millis(5), ..Script::video(Codec::Jpeg, 100_000) },
    );
    let transcode = Arc::new(FakeTranscoder(factory.clone(), std::sync::Mutex::default()));
    let mut drivers = oxrtc_core::Drivers::new();
    drivers.register(factory);
    if transcoder {
        drivers.register(transcode.clone());
    }
    let hub = Hub::new(drivers, limits_for_tests());
    for name in ["webcam", "camera", "slow", "few", "big"] {
        hub.upsert(name, &[&format!("fake:{name}")]).unwrap();
    }
    (ApiState::new(hub.clone(), "1.9.15", auth()).with_mp4_limits(limits), hub, transcode)
}

fn limits_for_tests() -> oxrtc_core::Limits {
    oxrtc_core::Limits { transcode_idle_grace: Duration::from_millis(300), ..common::limits() }
}

fn get(uri: &str) -> Request<Body> {
    Request::get(uri).header(header::AUTHORIZATION, basic(USER, PASS)).body(Body::empty()).unwrap()
}

/// A response's head, and its body read part by part.
struct Parts {
    status: StatusCode,
    headers: axum::http::HeaderMap,
    body: axum::body::BodyDataStream,
    held: Vec<u8>,
}

impl Parts {
    async fn of(router: &axum::Router, uri: &str) -> Self {
        let response = router.clone().oneshot(get(uri)).await.unwrap();
        let (parts, body) = response.into_parts();
        Self { status: parts.status, headers: parts.headers, body: body.into_data_stream(), held: Vec::new() }
    }

    fn header(&self, name: header::HeaderName) -> &str {
        self.headers.get(name).map_or("", |v| v.to_str().unwrap())
    }

    /// Reads until `want` bytes are held; `false` at the body's end.
    async fn fill(&mut self, want: usize) -> bool {
        while self.held.len() < want {
            match tokio::time::timeout(Duration::from_secs(5), self.body.next()).await.unwrap() {
                Some(chunk) => self.held.extend_from_slice(&chunk.unwrap()),
                None => return false,
            }
        }
        true
    }

    /// The next picture, as go2rtc's part says it; `None` at the body's end.
    /// The first part comes after the opening delimiter.
    async fn picture(&mut self, first: bool) -> Option<Bytes> {
        let head = "Content-Type: image/jpeg\r\nContent-Length: ";
        let opening = if first { "--frame\r\n" } else { "" };
        if !self.fill(opening.len() + head.len() + 12).await {
            return None;
        }
        let text = String::from_utf8_lossy(&self.held).into_owned();
        assert!(text.starts_with(&format!("{opening}{head}")), "{:?}", &text[..text.len().min(80)]);
        let at = opening.len() + head.len();
        let line_end = text[at..].find("\r\n\r\n").unwrap() + at;
        let length: usize = text[at..line_end].parse().unwrap();
        let start = line_end + 4;
        // The picture, its line end, and the next part's delimiter.
        assert!(self.fill(start + length + 2 + 9).await, "a part cut short");
        let picture = Bytes::copy_from_slice(&self.held[start..start + length]);
        assert_eq!(&self.held[start + length..start + length + 11], b"\r\n--frame\r\n", "the delimiter after the part");
        self.held.drain(..start + length + 11);
        Some(picture)
    }
}

fn consumers(hub: &Hub, stream: &str) -> Vec<oxrtc_core::ConsumerSnapshot> {
    hub.get(stream).unwrap().snapshot().consumers
}

#[tokio::test]
async fn go2rtcs_headers_and_parts_each_picture_with_its_length() {
    let (state, hub, _) = setup(Mp4Limits::default(), false);
    let router = oxrtc_api::router(state);
    let mut parts = Parts::of(&router, "/api/stream.mjpeg?src=webcam").await;
    assert_eq!(parts.status, StatusCode::OK);
    assert_eq!(parts.header(header::CONTENT_TYPE), "multipart/x-mixed-replace; boundary=frame");
    assert_eq!(parts.header(header::CACHE_CONTROL), "no-cache");
    assert_eq!(parts.header(header::PRAGMA), "no-cache");
    assert_eq!(parts.header(header::CONTENT_LENGTH), "");
    let mut numbers = Vec::new();
    for i in 0..20 {
        let picture = parts.picture(i == 0).await.unwrap();
        // The webcam's AVI1 picture, with the standard tables written in.
        let read = jpeg_header(&picture).unwrap();
        assert!(read.has_tables, "the standard Huffman tables are written in");
        assert_eq!(read.size, Some((64, 48)));
        numbers.push(picture.clone());
    }
    assert!(numbers.windows(2).all(|w| w[0] != w[1]), "twenty pictures, each another");
    let listed = consumers(&hub, "webcam");
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].kind, "mjpeg");
    assert!(listed[0].frames >= 20);
}

#[tokio::test]
async fn a_stream_with_no_jpeg_is_encoded_once_for_every_reader() {
    let (state, hub, transcoder) = setup(Mp4Limits::default(), true);
    let router = oxrtc_api::router(state);
    let mut first = Parts::of(&router, "/api/stream.mjpeg?src=camera").await;
    let mut second = Parts::of(&router, "/api/stream.mjpeg?src=camera").await;
    assert_eq!((first.status, second.status), (StatusCode::OK, StatusCode::OK));
    assert!(first.picture(true).await.is_some() && second.picture(true).await.is_some());
    assert_eq!(*transcoder.1.lock().unwrap(), vec!["ffmpeg:camera#video=mjpeg for camera#mjpeg".to_owned()]);
    let listing = hub.get("camera").unwrap().snapshot();
    let transcode = listing.sources.iter().find(|s| s.url == "ffmpeg:camera#video=mjpeg").unwrap();
    assert_eq!(transcode.demand, 2, "one source, both readers");
    assert_eq!(listing.consumers.iter().filter(|c| c.kind == "mjpeg").count(), 2);
    drop((first, second));
    assert!(
        eventually(Duration::from_secs(3), || {
            let listing = hub.get("camera").unwrap().snapshot();
            listing.consumers.iter().all(|c| c.kind != "mjpeg")
                && listing.sources.iter().all(|s| s.state == oxrtc_core::SourceState::Idle)
        })
        .await,
        "readers gone, the transcode stops after its grace"
    );
    // The JPEG of a stream that has one is never transcoded.
    assert!(Parts::of(&router, "/api/stream.mjpeg?src=webcam").await.picture(true).await.is_some());
    assert_eq!(transcoder.1.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn what_cannot_be_served_is_refused_with_go2rtcs_words() {
    let (state, _hub, _) = setup(Mp4Limits::default(), false);
    let router = oxrtc_api::router(state);
    for (uri, status, text) in [
        ("/api/stream.mjpeg?src=nope", StatusCode::NOT_FOUND, "stream not found"),
        ("/api/stream.mjpeg", StatusCode::BAD_REQUEST, "src is required"),
        ("/api/stream.mjpeg?src=webcam&queue=fast", StatusCode::BAD_REQUEST, "queue"),
        // No JPEG, and no FFmpeg to encode one.
        ("/api/stream.mjpeg?src=camera", StatusCode::INTERNAL_SERVER_ERROR, "codecs not matched"),
    ] {
        let reply = router.clone().oneshot(get(uri)).await.unwrap();
        assert_eq!(reply.status(), status, "{uri}");
        let body = axum::body::to_bytes(reply.into_body(), 4096).await.unwrap();
        assert!(String::from_utf8_lossy(&body).contains(text), "{uri}: {body:?}");
    }
}

#[tokio::test]
async fn it_shares_the_bound_of_mp4_responses_per_address() {
    let (logs, _guard) = Logs::capture();
    let limits = Mp4Limits { max_responses_per_ip: 2, ..Mp4Limits::default() };
    let (state, _hub, _) = setup(limits, false);
    let router = oxrtc_api::router(state);
    let first = Parts::of(&router, "/api/stream.mjpeg?src=webcam").await;
    let _second = Parts::of(&router, "/api/stream.mjpeg?src=slow").await;
    let third = Parts::of(&router, "/api/stream.mjpeg?src=webcam").await;
    assert_eq!(third.status, StatusCode::SERVICE_UNAVAILABLE);
    assert!(logs.text().contains("api: per-address mjpeg response limit reached, refusing"), "{}", logs.text());
    drop(first);
    assert_eq!(Parts::of(&router, "/api/stream.mjpeg?src=webcam").await.status, StatusCode::OK, "a slot freed");
}

#[tokio::test]
async fn a_silent_source_ends_the_response() {
    let limits = Mp4Limits { silence: Duration::from_millis(300), ..Mp4Limits::default() };
    let (state, _hub, _) = setup(limits, false);
    let router = oxrtc_api::router(state);
    let mut parts = Parts::of(&router, "/api/stream.mjpeg?src=few").await;
    let mut got = 0;
    while parts.picture(got == 0).await.is_some() {
        got += 1;
    }
    assert_eq!(got, 5);
}

#[tokio::test]
async fn a_slow_reader_loses_pictures_and_says_so() {
    let (logs, _guard) = Logs::capture();
    let (state, hub, _) = setup(Mp4Limits::default(), false);
    let router = oxrtc_api::router(state);
    let mut parts = Parts::of(&router, "/api/stream.mjpeg?src=big").await;
    parts.picture(true).await.unwrap();
    // 200 KB every 5 ms, not read for longer than a live reader's queue holds.
    tokio::time::sleep(Duration::from_millis(3500)).await;
    for _ in 0..200 {
        parts.picture(false).await.unwrap();
    }
    assert!(consumers(&hub, "big")[0].dropped > 0, "the stream's listing counts the loss");
    assert!(eventually(Duration::from_secs(2), || logs.text().contains("mjpeg reader fell behind the stream")).await);
}

#[tokio::test]
async fn over_tcp_it_is_chunked_and_ends_with_its_client_or_the_server() {
    let (state, hub, _) = setup(Mp4Limits::default(), false);
    let server = common::ws::serve(state, ServeLimits::default()).await;
    let mut stream = TcpStream::connect(server.addr).await.unwrap();
    let request =
        format!("GET /api/stream.mjpeg?src=webcam HTTP/1.1\r\nHost: x\r\nAuthorization: {}\r\n\r\n", basic(USER, PASS));
    stream.write_all(request.as_bytes()).await.unwrap();
    let mut some = vec![0; 2048];
    stream.read_exact(&mut some).await.unwrap();
    let text = String::from_utf8_lossy(&some).to_ascii_lowercase();
    assert!(text.starts_with("http/1.1 200") && text.contains("transfer-encoding: chunked"), "{text}");
    assert!(text.contains("content-type: multipart/x-mixed-replace; boundary=frame"), "{text}");
    assert_eq!(consumers(&hub, "webcam").len(), 1);
    drop(stream);
    assert!(eventually(Duration::from_millis(300), || consumers(&hub, "webcam").is_empty()).await, "its client gone");

    let mut stream = TcpStream::connect(server.addr).await.unwrap();
    stream.write_all(request.as_bytes()).await.unwrap();
    stream.read_exact(&mut some).await.unwrap();
    server.cancel.cancel();
    let mut rest = Vec::new();
    let read = tokio::time::timeout(Duration::from_secs(2), stream.read_to_end(&mut rest)).await;
    assert!(read.is_ok(), "the server's stop ends the body");
    assert!(rest.ends_with(b"0\r\n\r\n"), "a complete chunked body");
    assert!(eventually(Duration::from_millis(500), || consumers(&hub, "webcam").is_empty()).await);
}

#[tokio::test]
async fn the_transcode_s_failure_is_the_readers_error() {
    struct Failing;
    impl SourceFactory for Failing {
        fn schemes(&self) -> &[&'static str] {
            &["ffmpeg"]
        }
        fn build(&self, _spec: &SourceSpec, _ctx: &FactoryCtx) -> Result<Arc<dyn SourceDriver>, SpecError> {
            Ok(Arc::new(Declares))
        }
    }
    let factory = Arc::new(FakeFactory::default());
    factory.add("camera", Script::video(Codec::H264, 10_000));
    let mut drivers = oxrtc_core::Drivers::new();
    drivers.register(factory);
    drivers.register(Arc::new(Failing));
    let hub = Hub::new(drivers, limits_for_tests());
    hub.upsert("camera", &["fake:camera"]).unwrap();
    let router = oxrtc_api::router(ApiState::new(hub, "1.9.15", auth()));
    let reply = router.oneshot(get("/api/stream.mjpeg?src=camera")).await.unwrap();
    assert_eq!(reply.status(), StatusCode::INTERNAL_SERVER_ERROR);
    let body = axum::body::to_bytes(reply.into_body(), 4096).await.unwrap();
    assert!(String::from_utf8_lossy(&body).starts_with("streams:"), "{body:?}");
}
