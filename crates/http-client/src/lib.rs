//! Camera ingest for oxrtc: `http://` and `https://` sources, the webcams
//! that serve Motion JPEG.
//!
//! [`HttpFactory`] registers for both schemes. A source is a URL a camera
//! answers a `GET` on, with one of two shapes, told by the response's
//! `Content-Type`:
//!
//! - **A stream**: `multipart/x-mixed-replace`, one JPEG per part, for as long
//!   as the camera keeps the response open ([`multipart`] is the reader).
//! - **A picture**: `image/jpeg` (or no type, or `application/octet-stream`):
//!   one JPEG per request. go2rtc asks again the moment it has the picture;
//!   this driver waits `#interval=<milliseconds>` (100 by default) between the
//!   starts of two requests, on the same connection when the camera keeps it
//!   open.
//!
//! The track is `video, JPEG` at 90 kHz: pictures are published as they are,
//! each one a keyframe, never decoded. A body that does not start as a JPEG is
//! skipped.
//!
//! Credentials come from the URL's user information, answered to a `401` as
//! Basic or Digest, and go to that origin only (a redirect to another drops
//! them). `https` verifies the certificate against the operating system's
//! trusted roots. `#timeout=N` sets the stall timeout in seconds, as for an
//! RTSP camera. Logs and errors name a source only in its redacted form.
#![forbid(unsafe_code)]

mod driver;
mod fetch;
/// The harness of the fuzz target (`fuzz/`) and its stable property tests.
#[cfg(any(test, feature = "fuzzing"))]
#[doc(hidden)]
pub mod fuzzing;
mod multipart;
mod target;
mod tls;

use std::sync::Arc;
use std::time::Duration;

use http::HeaderValue;
use oxrtc_core::spec::MAX_TIMEOUT_SECS;
use oxrtc_core::{FactoryCtx, SourceDriver, SourceFactory, SourceSpec, SpecError};

pub use driver::MAX_PICTURE_BYTES;
pub use fetch::{MAX_HEAD_BYTES, MAX_REDIRECTS};
pub use target::{DEFAULT_INTERVAL, INTERVAL_MS};

use crate::driver::HttpDriver;
use crate::tls::Tls;

/// How the HTTP client behaves.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpClientConfig {
    /// Stall timeout of a camera whose source has no `#timeout=N`.
    pub default_stall_timeout: Duration,
    /// `User-Agent` sent to cameras.
    pub user_agent: String,
    /// Certificates (DER) an `https` camera's may chain to, besides the
    /// operating system's: a private authority.
    pub extra_roots: Vec<Vec<u8>>,
}

impl Default for HttpClientConfig {
    fn default() -> Self {
        Self {
            default_stall_timeout: Duration::from_secs(15),
            user_agent: concat!("oxrtc/", env!("CARGO_PKG_VERSION")).to_owned(),
            extra_roots: Vec::new(),
        }
    }
}

/// Builds drivers for `http://` and `https://` sources.
#[derive(Debug, Clone)]
pub struct HttpFactory {
    default_stall_timeout: Duration,
    user_agent: HeaderValue,
    tls: Arc<Tls>,
}

impl HttpFactory {
    /// A factory with this configuration. A `default_stall_timeout` outside
    /// the range `#timeout=N` accepts (1 s to 3600 s) is clamped into it, and
    /// a `user_agent` that is not a valid header value is replaced by the
    /// default one.
    pub fn new(config: HttpClientConfig) -> Self {
        let user_agent = HeaderValue::from_str(&config.user_agent).unwrap_or_else(|_| {
            tracing::warn!("invalid HTTP user agent in the configuration; using the default one");
            HeaderValue::from_static(concat!("oxrtc/", env!("CARGO_PKG_VERSION")))
        });
        Self {
            default_stall_timeout: config
                .default_stall_timeout
                .clamp(Duration::from_secs(1), Duration::from_secs(MAX_TIMEOUT_SECS)),
            user_agent,
            tls: Arc::new(Tls::new(config.extra_roots)),
        }
    }
}

impl SourceFactory for HttpFactory {
    fn schemes(&self) -> &[&'static str] {
        &["http", "https"]
    }

    fn build(&self, spec: &SourceSpec, _ctx: &FactoryCtx) -> Result<Arc<dyn SourceDriver>, SpecError> {
        let target = target::parse(spec, self.default_stall_timeout)?;
        Ok(Arc::new(HttpDriver::new(target, spec.redacted(), self.user_agent.clone(), self.tls.clone())))
    }
}
