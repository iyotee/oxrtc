//! Scripted sources, fake backends and an HTTP helper for the API's tests.
#![allow(dead_code, missing_docs)]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing, clippy::panic)]

use std::collections::HashMap;
use std::io::Write;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::http::{HeaderMap, Method, Request, StatusCode, header};
use base64::Engine;
use bytes::Bytes;
use oxrtc_api::{AnswerError, Answered, ApiState, Auth, ResourceError, SnapshotError, Snapshotter, WebRtcAnswerer};
use oxrtc_core::jpeg::Shape;
use oxrtc_core::{
    BoxFuture, CancellationToken, Codec, Declared, Drivers, FactoryCtx, Frame, Hub, Limits, MediaKind, ProducerSink,
    SourceDriver, SourceError, SourceFactory, SourceSpec, SpecError, Stream, TrackInfo,
};
use tower::ServiceExt;

pub mod ws;

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
    /// Every run fails at once with this error (an unreachable camera).
    pub fails_with: Option<String>,
    /// When not zero, each video frame is one NAL unit of this many bytes
    /// (a camera's picture), not [`KEY`] and [`DELTA`].
    pub video_bytes: usize,
    /// Published again just before the first frame: a camera whose
    /// parameter sets in band are not those of its description (the RTSP
    /// client publishes its track list again when it finds them).
    pub in_band: Option<Vec<TrackInfo>>,
    /// Published before frame `n`: a camera whose track list changes while
    /// it streams (a reconnection that brings its sound back). The frames
    /// still cycle over [`Script::tracks`].
    pub retrack: Option<(usize, Vec<TrackInfo>)>,
    /// A keyframe every this many frames of a track (10 by default).
    pub keyframe_every: usize,
}

impl Script {
    pub fn tracks(tracks: Vec<TrackInfo>, frames: usize) -> Self {
        Self {
            declared: None,
            tracks,
            tracks_after: Duration::ZERO,
            frames,
            interval: Duration::from_millis(20),
            fails_with: None,
            video_bytes: 0,
            in_band: None,
            retrack: None,
            keyframe_every: 10,
        }
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
            if let Some(error) = &script.fails_with {
                return Err(SourceError::Unreachable(error.clone()));
            }
            if script.tracks.is_empty() {
                cancel.cancelled().await;
                return Ok(());
            }
            tokio::time::sleep(script.tracks_after).await;
            sink.set_tracks(script.tracks.clone()).map_err(|e| SourceError::Refused(e.to_string()))?;
            for n in 0..script.frames {
                tokio::time::sleep(script.interval).await;
                if let (0, Some(in_band)) = (n, &script.in_band) {
                    sink.set_tracks(in_band.clone()).map_err(|e| SourceError::Refused(e.to_string()))?;
                }
                if let Some((at, tracks)) = &script.retrack
                    && *at == n
                {
                    sink.set_tracks(tracks.clone()).map_err(|e| SourceError::Refused(e.to_string()))?;
                }
                let track = n % script.tracks.len();
                let jpeg = script.tracks[track].codec == Codec::Jpeg;
                // Every Motion JPEG picture stands alone.
                let key = jpeg || (n / script.tracks.len()).is_multiple_of(script.keyframe_every.max(1));
                let data = match script.tracks[track].kind {
                    MediaKind::Video if jpeg => jpeg_picture(n as u64, script.video_bytes),
                    MediaKind::Video if script.video_bytes > 0 => picture(script.video_bytes, key, n as u64),
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

/// A JPEG as a webcam sends one (`AVI1`, no Huffman tables: the decoder's
/// standard ones), its number `n` in its scan, `size` bytes of scan at least.
pub fn jpeg_picture(n: u64, size: usize) -> Bytes {
    let mut out = vec![0xff, 0xd8, 0xff, 0xe0, 0, 16, b'A', b'V', b'I', b'1', 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];
    out.extend_from_slice(&[0xff, 0xdb, 0, 67, 0]);
    out.extend_from_slice(&[1; 64]);
    out.extend_from_slice(&[0xff, 0xc0, 0, 17, 8, 0, 48, 0, 64, 3, 1, 0x21, 0, 2, 0x11, 1, 3, 0x11, 1]);
    out.extend_from_slice(&[0xff, 0xda, 0, 12, 3, 1, 0, 2, 0x11, 3, 0x11, 0, 63, 0]);
    out.extend_from_slice(&n.to_be_bytes().map(|b| if b == 0xff { 0xfe } else { b }));
    out.resize(out.len().max(size), 0x5a);
    out.extend_from_slice(&[0xff, 0xd9]);
    out.into()
}

/// One length-prefixed NAL unit of `size` bytes, an IDR slice or not, the
/// frame's number `n` after its header (so that a test can tell frames apart).
fn picture(size: usize, key: bool, n: u64) -> Bytes {
    let size = size.max(9);
    let mut data = Vec::with_capacity(size + 4);
    data.extend_from_slice(&(size as u32).to_be_bytes());
    data.push(if key { 0x65 } else { 0x41 });
    data.extend_from_slice(&n.to_be_bytes());
    data.resize(size + 4, 0x5a);
    data.into()
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
    /// The `User-Agent` of each offer, in the same order.
    pub agents: Mutex<Vec<Option<String>>>,
    /// Each replacement of the ICE servers, as the API handed it over.
    pub ice: Mutex<Vec<Vec<oxrtc_api::config::IceServer>>>,
    /// The id of the session resource every answer names.
    pub resource: String,
    /// What a request on a resource other than `resource` is answered with,
    /// and on `resource` itself when it is `Err`.
    pub refusal: Mutex<Option<ResourceError>>,
    /// Each `PATCH` that reached the answerer: the resource and the body.
    pub patched: Mutex<Vec<(String, String)>>,
    /// Each `DELETE` that reached it.
    pub closed: Mutex<Vec<String>>,
}

/// The resource id the fake answers name.
pub const RESOURCE: &str = "0123456789abcdef0123456789abcdef";

impl FakeAnswerer {
    pub fn new(result: Result<String, AnswerError>) -> Arc<Self> {
        Self::slow(result, Duration::ZERO)
    }
    pub fn slow(result: Result<String, AnswerError>, delay: Duration) -> Arc<Self> {
        Arc::new(Self {
            result,
            delay,
            seen: Mutex::new(Vec::new()),
            agents: Mutex::new(Vec::new()),
            ice: Mutex::new(Vec::new()),
            resource: RESOURCE.into(),
            refusal: Mutex::new(None),
            patched: Mutex::new(Vec::new()),
            closed: Mutex::new(Vec::new()),
        })
    }

    /// What a request on `resource` is answered with.
    fn resource_result(&self, resource: &str) -> Result<(), ResourceError> {
        if let Some(refusal) = self.refusal.lock().unwrap().clone() {
            return Err(refusal);
        }
        if resource == self.resource { Ok(()) } else { Err(ResourceError::NotFound) }
    }
}

impl WebRtcAnswerer for FakeAnswerer {
    fn set_ice_servers(&self, servers: Vec<oxrtc_api::config::IceServer>) -> oxrtc_api::IceSummary {
        let turn = servers.iter().filter(|s| s.urls.iter().any(|u| u.starts_with("turn:"))).count();
        let stun = servers.iter().filter(|s| s.urls.iter().any(|u| u.starts_with("stun:"))).count();
        self.ice.lock().unwrap().push(servers);
        oxrtc_api::IceSummary { turn, stun }
    }

    fn answer(
        &self,
        stream: Arc<Stream>,
        offer_sdp: String,
        remote: Option<SocketAddr>,
        user_agent: Option<String>,
    ) -> BoxFuture<'static, Result<Answered, AnswerError>> {
        self.seen.lock().unwrap().push((stream.name().to_owned(), offer_sdp, remote));
        self.agents.lock().unwrap().push(user_agent);
        let result = self.result.clone();
        let delay = self.delay;
        let resource = self.resource.clone();
        Box::pin(async move {
            tokio::time::sleep(delay).await;
            result.map(|sdp| Answered { sdp, resource })
        })
    }

    fn trickle(&self, resource: String, fragment: String) -> BoxFuture<'static, Result<(), ResourceError>> {
        self.patched.lock().unwrap().push((resource.clone(), fragment));
        let result = self.resource_result(&resource);
        Box::pin(async move { result })
    }

    fn close(&self, resource: String) -> BoxFuture<'static, Result<(), ResourceError>> {
        self.closed.lock().unwrap().push(resource.clone());
        let result = self.resource_result(&resource);
        Box::pin(async move { result })
    }
}

pub struct FakeSnapshotter {
    pub result: Result<Bytes, SnapshotError>,
    pub delay: Duration,
    pub shapes: Mutex<Vec<Shape>>,
}

impl FakeSnapshotter {
    pub fn new(result: Result<Bytes, SnapshotError>) -> Arc<Self> {
        Arc::new(Self { result, delay: Duration::ZERO, shapes: Mutex::new(Vec::new()) })
    }
    pub fn slow(delay: Duration) -> Arc<Self> {
        Arc::new(Self { result: Ok(Bytes::from_static(b"\xff\xd8")), delay, shapes: Mutex::new(Vec::new()) })
    }
}

impl Snapshotter for FakeSnapshotter {
    fn jpeg(&self, _stream: Arc<Stream>, shape: Shape) -> BoxFuture<'static, Result<Bytes, SnapshotError>> {
        self.shapes.lock().unwrap().push(shape);
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
        self.send_with(method, uri, body, authorization, &[]).await
    }

    /// [`Self::send`] with more headers.
    pub async fn send_with(
        &self,
        method: Method,
        uri: &str,
        body: Vec<u8>,
        authorization: Option<&str>,
        headers: &[(header::HeaderName, &[u8])],
    ) -> Reply {
        let mut request = Request::builder().method(method).uri(uri);
        if let Some(value) = authorization {
            request = request.header(header::AUTHORIZATION, value);
        }
        for (name, value) in headers {
            request = request.header(name, *value);
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
    ("GET", "/api/schemes"),
    ("GET", "/api/openapi.yaml"),
    ("GET", "/api/streams"),
    ("PUT", "/api/streams?name=x&src=fake:x"),
    ("PATCH", "/api/streams?name=x&src=fake:x"),
    ("POST", "/api/streams?src=x&dst=y"),
    ("GET", "/api/streams.dot"),
    ("GET", "/api/config"),
    ("POST", "/api/config"),
    ("PATCH", "/api/config"),
    ("GET", "/api/log"),
    ("DELETE", "/api/log"),
    ("POST", "/api/restart"),
    ("POST", "/api/exit?code=0"),
    ("DELETE", "/api/streams?src=x"),
    ("GET", "/api/preload"),
    ("PUT", "/api/preload?src=x"),
    ("DELETE", "/api/preload?src=x"),
    ("POST", "/api/webrtc?src=x"),
    ("PUT", "/api/webrtc/ice"),
    ("GET", "/api/frame.jpeg?src=x"),
    ("GET", "/api/stream.mp4?src=x"),
    ("GET", "/api/frame.mp4?src=x"),
    ("GET", "/api/stream.mjpeg?src=x"),
    ("GET", "/api/ws?src=x"),
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
        // A second dispatcher, alive for the whole test binary. While this
        // capture is the only one registered, tracing asks the dispatcher of
        // whichever thread reaches a callsite first whether anyone wants it:
        // another test's thread, which has none, answers never, for every
        // thread and for good, and the line this capture waits for is not
        // written (`no_password_in_any_response_or_log` failed four runs in
        // five with eight test threads). With two registered, every
        // dispatcher is asked, this one included.
        static SECOND: OnceLock<tracing::Dispatch> = OnceLock::new();
        SECOND.get_or_init(|| tracing::Dispatch::new(tracing::subscriber::NoSubscriber::default()));
        let guard = tracing::subscriber::set_default(subscriber);
        // A callsite another test thread registered before this capture
        // must not stay disabled for it (the same as oxrtc-core's channel
        // tests).
        tracing::callsite::rebuild_interest_cache();
        (logs, guard)
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
