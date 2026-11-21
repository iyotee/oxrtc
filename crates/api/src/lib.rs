//! Configuration loader and HTTP API of oxrtc.
//!
//! The API is the go2rtc-compatible surface openIVR uses (see
//! `docs/COMPATIBILITY.md`), on axum. It does not depend on the WebRTC or
//! FFmpeg crates: it reaches them through [`WebRtcAnswerer`] and
//! [`Snapshotter`], which the binary implements.
//!
//! `GET /api/ws` is go2rtc's WebSocket API ([`ws`]): the same WebRTC
//! sessions, signalled over a socket, and MSE (fragmented MP4 from
//! `oxrtc-mp4`) written over it.
//!
//! Every route is authenticated ([`Auth`]), loopback included; request bodies
//! are capped at [`MAX_BODY_BYTES`]; every request has a deadline; the number
//! of open connections is bounded; no response or log line carries a raw
//! source spec.
#![forbid(unsafe_code)]

use std::sync::Arc;
use std::time::Duration;

use oxrtc_core::{Hub, Refusals};

mod auth;
mod backend;
pub mod config;
mod ice;
mod info;
mod linger;
mod listener;
mod preload;
mod resource;
mod routes;
pub mod ws;

/// Harnesses for the fuzz targets (`fuzz/`) and their stable property tests.
#[cfg(any(test, feature = "fuzzing"))]
#[doc(hidden)]
pub mod fuzzing;

pub use auth::Auth;
pub use backend::{AnswerError, Answered, IceSummary, ResourceError, SnapshotError, Snapshotter, WebRtcAnswerer};
pub use linger::{LINGER_BYTES, LINGER_TIMEOUT};
pub use listener::{
    HEAD_TIMEOUT, IDLE_TIMEOUT, MAX_CONNECTIONS, MAX_CONNECTIONS_PER_IP, SHUTDOWN_GRACE, ServeLimits, serve,
    serve_with_limits,
};
pub use preload::PRELOAD_WAIT;
pub use resource::TRICKLE_CONTENT_TYPE;
pub use routes::{MAX_BODY_BYTES, MAX_SNAPSHOT_WIDTH, QUICK_TIMEOUT, SLOW_TIMEOUT, router};
pub use ws::{OriginPolicy, WsLimits};

use preload::Preloads;

/// What the routes share.
#[derive(Clone)]
pub struct ApiState {
    hub: Arc<Hub>,
    version: Arc<str>,
    auth: Arc<Auth>,
    answerer: Option<Arc<dyn WebRtcAnswerer>>,
    snapshotter: Option<Arc<dyn Snapshotter>>,
    preloads: Arc<Preloads>,
    preload_wait: Duration,
    /// Offers refused for want of a session slot, reported at most once per
    /// interval: a viewer retrying in a loop must not write one warning per
    /// attempt.
    busy: Refusals,
    /// Which pages may open `/api/ws` (`api.origin`).
    ws_origin: OriginPolicy,
    /// The bounds of `/api/ws`.
    ws_limits: WsLimits,
    /// Its sockets open, against those bounds.
    sockets: Arc<ws::Sockets>,
}

impl std::fmt::Debug for ApiState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ApiState")
            .field("hub", &self.hub)
            .field("version", &self.version)
            .field("auth", &self.auth)
            .field("webrtc", &self.answerer.is_some())
            .field("snapshots", &self.snapshotter.is_some())
            .field("preloads", &self.preloads.len())
            .field("ws_origin", &self.ws_origin)
            .field("ws_limits", &self.ws_limits)
            .finish()
    }
}

impl ApiState {
    /// The API of `hub`, answering `version` on `GET /api`.
    pub fn new(hub: Arc<Hub>, version: &str, auth: Auth) -> Self {
        let max_preloads = hub.limits().max_streams;
        Self {
            hub,
            version: version.into(),
            auth: Arc::new(auth),
            answerer: None,
            snapshotter: None,
            preloads: Arc::new(Preloads::new(max_preloads)),
            preload_wait: PRELOAD_WAIT,
            busy: Refusals::new(),
            ws_origin: OriginPolicy::default(),
            ws_limits: WsLimits::default(),
            sockets: Arc::new(ws::Sockets::new(&WsLimits::default())),
        }
    }

    /// Which pages may open `/api/ws` ([`OriginPolicy::from_config`] of
    /// `api.origin`).
    pub fn with_ws_origin(mut self, origin: OriginPolicy) -> Self {
        self.ws_origin = origin;
        self
    }

    /// Other bounds for `/api/ws` than [`WsLimits::default`].
    pub fn with_ws_limits(mut self, limits: WsLimits) -> Self {
        self.sockets = Arc::new(ws::Sockets::new(&limits));
        self.ws_limits = limits;
        self
    }

    /// Enables `POST /api/webrtc` and the session resources it names.
    pub fn with_webrtc(mut self, answerer: Arc<dyn WebRtcAnswerer>) -> Self {
        self.answerer = Some(answerer);
        self
    }

    /// Enables `GET /api/frame.jpeg`.
    pub fn with_snapshots(mut self, snapshotter: Arc<dyn Snapshotter>) -> Self {
        self.snapshotter = Some(snapshotter);
        self
    }

    /// How long `PUT /api/preload` waits for tracks ([`PRELOAD_WAIT`] by
    /// default; capped at it).
    pub fn with_preload_wait(mut self, wait: Duration) -> Self {
        self.preload_wait = wait.min(PRELOAD_WAIT);
        self
    }

    /// Releases every preload (at shutdown).
    pub fn release_preloads(&self) {
        self.preloads.clear();
    }
}
