//! Scripted sources, fake backends and an HTTP helper for the API's tests.
#![allow(dead_code, missing_docs)]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing, clippy::panic)]

use std::collections::HashMap;
use std::io::Write;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::http::{HeaderMap, Method, Request, StatusCode, header};
use base64::Engine;
use bytes::Bytes;
use oxrtc_api::{AnswerError, ApiState, Auth, SnapshotError, Snapshotter, WebRtcAnswerer};
use oxrtc_core::{
    BoxFuture, CancellationToken, Codec, Declared, Drivers, FactoryCtx, Frame, Hub, Limits, MediaKind, ProducerSink,
    SourceDriver, SourceError, SourceFactory, SourceSpec, SpecError, Stream, TrackInfo,
};
use tower::ServiceExt;

pub const USER: &str = "admin";
pub const PASS: &str = "api-s3cret";
pub const TOKEN: &str = "tok-abcdef";

pub const KEY: &[u8] = &[0, 0, 0, 2, 0x65, 0x88];
pub const DELTA: &[u8] = &[0, 0, 0, 2, 0x41, 0x9a];

#[derive(Clone)]
pub struct Script {
    pub declared: Option<Declared>,
    /// Empty: the source never announces tracks.
    pub tracks: Vec<TrackInfo>,
    pub tracks_after: Duration,
    /// Frames to publish, cycling over the tracks; video frames every 10th is a keyframe.
    pub frames: usize,
    pub interval: Duration,
}

impl Script {
    pub fn tracks(tracks: Vec<TrackInfo>, frames: usize) -> Self {
        Self { declared: None, tracks, tracks_after: Duration::ZERO, frames, interval: Duration::from_millis(20) }
    }

    pub fn video(codec: Codec, frames: usize) -> Self {
        Self::tracks(vec![TrackInfo::video(codec)], frames)
    }

    /// A source that declares `declared` and never produces anything.
    pub fn silent(declared: Declared) -> Self {
        Self { declared: Some(declared), ..Self::tracks(Vec::new(), 0) }
    }
}

struct FakeDriver(Script);

impl SourceDriver for FakeDriver {
    fn declared(&self) -> Option<Declared> {
        self.0.declared.clone()
    }

    fn run(
        self: Arc<Self>,
        sink: ProducerSink,
        cancel: CancellationToken,
    ) -> BoxFuture<'static, Result<(), SourceError>> {
        Box::pin(async move {
            let script = &self.0;
            if script.tracks.is_empty() {
                cancel.cancelled().await;
                return Ok(());
            }
            tokio::time::sleep(script.tracks_after).await;
            sink.set_tracks(script.tracks.clone()).map_err(|e| SourceError::Refused(e.to_string()))?;
            for n in 0..script.frames {
                tokio::time::sleep(script.interval).await;
                let track = n % script.tracks.len();
                let key = (n / script.tracks.len()).is_multiple_of(10);
                let data = match script.tracks[track].kind {
                    MediaKind::Video if key => Bytes::from_static(KEY),
                    MediaKind::Video => Bytes::from_static(DELTA),
                    MediaKind::Audio => Bytes::from_static(&[0xd5; 160]),
                };
                sink.publish(Frame::new(track as u8, n as i64 * 3600, key, data))
                    .map_err(|e| SourceError::Refused(e.to_string()))?;
            }
            cancel.cancelled().await;
            Ok(())
        })
    }
}

/// Sources `fake:<key>` or `fake://user:pass@<key>`, scripted by key.
#[derive(Default)]
pub struct FakeFactory {
    scripts: Mutex<HashMap<String, Script>>,
}

impl FakeFactory {
    pub fn add(&self, key: &str, script: Script) {
        self.scripts.lock().unwrap().insert(key.to_owned(), script);
    }
}

impl SourceFactory for FakeFactory {
    fn schemes(&self) -> &[&'static str] {
        &["fake"]
    }

    fn build(&self, spec: &SourceSpec, _ctx: &FactoryCtx) -> Result<Arc<dyn SourceDriver>, SpecError> {
        let target = spec.target();
        let key = target.rsplit('@').next().unwrap_or(target).trim_start_matches('/');
        let key = key.split('/').next().unwrap_or(key);
        let script = self.scripts.lock().unwrap().get(key).cloned().ok_or(SpecError::Invalid("no such fake".into()))?;
        Ok(Arc::new(FakeDriver(script)))
    }
}

pub fn limits() -> Limits {
    Limits {
        idle_grace: Duration::from_millis(300),
        probe_timeout: Duration::from_secs(2),
        connect_timeout: Duration::from_secs(5),
        stall_timeout: Duration::from_secs(5),
        backoff_initial: Duration::from_millis(100),
        backoff_max: Duration::from_secs(1),
        ..Limits::default()
    }
}

pub fn hub(factory: Arc<FakeFactory>) -> Arc<Hub> {
    let mut drivers = Drivers::new();
    drivers.register(factory);
    Hub::new(drivers, limits())
}

pub fn auth() -> Auth {
    Auth::Credentials { basic: Some((USER.into(), PASS.into())), bearer: Some(TOKEN.into()) }
}

pub fn basic(user: &str, pass: &str) -> String {
    format!("Basic {}", base64::engine::general_purpose::STANDARD.encode(format!("{user}:{pass}")))
}

/// A fake answerer: records offers and answers with a scripted result.
pub struct FakeAnswerer {
    pub result: Result<String, AnswerError>,
    pub delay: Duration,
    pub seen: Mutex<Vec<(String, String, Option<SocketAddr>)>>,
}

impl FakeAnswerer {
    pub fn new(result: Result<String, AnswerError>) -> Arc<Self> {
        Arc::new(Self { result, delay: Duration::ZERO, seen: Mutex::new(Vec::new()) })
    }
    pub fn slow(result: Result<String, AnswerError>, delay: Duration) -> Arc<Self> {
        Arc::new(Self { result, delay, seen: Mutex::new(Vec::new()) })
    }
}

impl WebRtcAnswerer for FakeAnswerer {
    fn answer(
        &self,
        stream: Arc<Stream>,
        offer_sdp: String,
        remote: Option<SocketAddr>,
    ) -> BoxFuture<'static, Result<String, AnswerError>> {
        self.seen.lock().unwrap().push((stream.name().to_owned(), offer_sdp, remote));
        let result = self.result.clone();
        let delay = self.delay;
        Box::pin(async move {
            tokio::time::sleep(delay).await;
            result
        })
    }
}

pub struct FakeSnapshotter {
    pub result: Result<Bytes, SnapshotError>,
    pub delay: Duration,
    pub widths: Mutex<Vec<Option<u32>>>,
}

impl FakeSnapshotter {
    pub fn new(result: Result<Bytes, SnapshotError>) -> Arc<Self> {
        Arc::new(Self { result, delay: Duration::ZERO, widths: Mutex::new(Vec::new()) })
    }
    pub fn slow(delay: Duration) -> Arc<Self> {
        Arc::new(Self { result: Ok(Bytes::from_static(b"\xff\xd8")), delay, widths: Mutex::new(Vec::new()) })
    }
}

impl Snapshotter for FakeSnapshotter {
    fn jpeg(&self, _stream: Arc<Stream>, width: Option<u32>) -> BoxFuture<'static, Result<Bytes, SnapshotError>> {
        self.widths.lock().unwrap().push(width);
        let result = self.result.clone();
        let delay = self.delay;
        Box::pin(async move {
            tokio::time::sleep(delay).await;
            result
        })
    }
}

/// A test API: the router, the hub, and the scripted factory.
pub struct Api {
    pub router: Router,
    pub hub: Arc<Hub>,
    pub factory: Arc<FakeFactory>,
}

impl Api {
    pub fn new() -> Self {
        Self::build(|s| s)
    }

    pub fn build(configure: impl FnOnce(ApiState) -> ApiState) -> Self {
        let factory = Arc::new(FakeFactory::default());
        let hub = hub(factory.clone());
        let state = configure(ApiState::new(hub.clone(), "1.2.3-test", auth()));
        Self { router: oxrtc_api::router(state), hub, factory }
    }

    pub async fn send(&self, method: Method, uri: &str, body: Vec<u8>, authorization: Option<&str>) -> Reply {
        let mut request = Request::builder().method(method).uri(uri);
        if let Some(value) = authorization {
            request = request.header(header::AUTHORIZATION, value);
        }
        let request = request.body(Body::from(body)).unwrap();
        let response = self.router.clone().oneshot(request).await.unwrap();
        let status = response.status();
        let headers = response.headers().clone();
        let body = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
        Reply { status, headers, body }
    }

    /// An authenticated call.
    pub async fn call(&self, method: Method, uri: &str) -> Reply {
        self.send(method, uri, Vec::new(), Some(&basic(USER, PASS))).await
    }
}

#[derive(Debug)]
pub struct Reply {
    pub status: StatusCode,
    pub headers: HeaderMap,
    pub body: Bytes,
}

impl Reply {
    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }
    pub fn json(&self) -> serde_json::Value {
        serde_json::from_slice(&self.body).unwrap_or_else(|e| panic!("not JSON ({e}): {}", self.text()))
    }
    pub fn content_type(&self) -> &str {
        self.headers.get(header::CONTENT_TYPE).and_then(|v| v.to_str().ok()).unwrap_or("")
    }
}

/// Every route of the API, with a method that reaches its handler.
pub const ROUTES: &[(&str, &str)] = &[
    ("GET", "/api"),
    ("GET", "/api/streams"),
    ("PUT", "/api/streams?name=x&src=fake:x"),
    ("DELETE", "/api/streams?src=x"),
    ("GET", "/api/preload"),
    ("PUT", "/api/preload?src=x"),
    ("DELETE", "/api/preload?src=x"),
    ("POST", "/api/webrtc?src=x"),
    ("GET", "/api/frame.jpeg?src=x"),
    ("GET", "/no/such/route"),
];

/// Captures every log line written while the guard lives (current thread).
#[derive(Clone, Default)]
pub struct Logs(Arc<Mutex<Vec<u8>>>);

impl Logs {
    pub fn capture() -> (Self, tracing::subscriber::DefaultGuard) {
        let logs = Self::default();
        let writer = logs.clone();
        let subscriber = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::TRACE)
            .with_ansi(false)
            .with_writer(move || writer.clone())
            .finish();
        (logs, tracing::subscriber::set_default(subscriber))
    }

    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.0.lock().unwrap()).into_owned()
    }
}

impl Write for Logs {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Polls `check` every 20 ms for up to `limit`.
pub async fn eventually(limit: Duration, mut check: impl FnMut() -> bool) -> bool {
    let deadline = tokio::time::Instant::now() + limit;
    while tokio::time::Instant::now() < deadline {
        if check() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    check()
}

pub fn method(name: &str) -> Method {
    Method::from_bytes(name.as_bytes()).unwrap()
}
