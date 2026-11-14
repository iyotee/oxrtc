//! Configuration loader and HTTP API of oxrtc.
//!
//! The API is the go2rtc-compatible surface openIVR uses (see
//! `docs/COMPATIBILITY.md`), on axum. It does not depend on the WebRTC or
//! FFmpeg crates: it reaches them through [`WebRtcAnswerer`] and
//! [`Snapshotter`], which the binary implements.
//!
//! Every route is authenticated ([`Auth`]), loopback included; request bodies
//! are capped at [`MAX_BODY_BYTES`]; every request has a deadline; the number
//! of open connections is bounded; no response or log line carries a raw
//! source spec.
#![forbid(unsafe_code)]

use std::sync::Arc;
use std::time::Duration;

use oxrtc_core::Hub;

mod auth;
mod backend;
pub mod config;
mod info;
mod listener;
mod preload;
mod routes;

pub use auth::Auth;
pub use backend::{AnswerError, SnapshotError, Snapshotter, WebRtcAnswerer};
pub use listener::{IDLE_TIMEOUT, MAX_CONNECTIONS, SHUTDOWN_GRACE, ServeLimits, serve, serve_with_limits};
pub use preload::PRELOAD_WAIT;
pub use routes::{MAX_BODY_BYTES, MAX_SNAPSHOT_WIDTH, QUICK_TIMEOUT, SLOW_TIMEOUT, router};

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
        }
    }

    /// Enables `POST /api/webrtc`.
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
