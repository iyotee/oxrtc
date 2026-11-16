//! Camera ingest for oxrtc: `rtsp://` and `rtsps://` sources, on top of
//! [retina](https://crates.io/crates/retina).
//!
//! [`RtspFactory`] registers for both schemes. A source is one of two things,
//! decided by [`SourceFactory::build`] without any I/O:
//!
//! - **Our own relay**: `rtsp://127.0.0.1:<port>/<name>?<query>` (also
//!   `localhost` and `[::1]`) whose port is one of
//!   [`RtspClientConfig::loopback_ports`]. That is how go2rtc configurations
//!   spell "another stream of this server"; it becomes the core's alias
//!   ([`LocalDriver`](oxrtc_core::local::LocalDriver)): its consumers read that
//!   stream's own source, with no network hop, no copy and no supervisor of
//!   its own. Its `#timeout=N` is accepted and has no effect (the stream it
//!   names has its own).
//! - **A camera**: DESCRIBE, SETUP over TCP interleaved (video first, then
//!   audio), PLAY, then every access unit is published into the core. H.264 and
//!   H.265 frames are length-prefixed NAL units (retina's own output), with the
//!   parameter sets repeated before each keyframe. `#timeout=N` sets the stall
//!   timeout of the source, in seconds.
//!
//! Credentials come from the URL's user information; retina receives them
//! separately and the URL without them. Logs name a source only in its
//! redacted form.
//!
//! retina logs through the `log` crate under the `retina` target, and some of
//! its warnings are routine with cheap cameras: the binary's log bridge is
//! expected to demote that target's warnings to debug (see [`RETINA_LOG_TARGET`]).
#![forbid(unsafe_code)]

mod codec;
mod driver;
mod target;
mod tls;

use std::sync::Arc;
use std::time::Duration;

use oxrtc_core::local::LocalDriver;
use oxrtc_core::{FactoryCtx, SourceDriver, SourceFactory, SourceSpec, SpecError};
use oxrtc_net::Tls;

pub use driver::{MAX_RTSP_MESSAGE_BYTES, MAX_STALE_WAIT};
pub use oxrtc_core::spec::MAX_TIMEOUT_SECS;
pub use target::{DEFAULT_RTSP_PORT, DEFAULT_RTSPS_PORT};

use crate::driver::RtspDriver;
use crate::target::Target;

/// The `log` target retina writes under.
pub const RETINA_LOG_TARGET: &str = "retina";

/// How the RTSP client behaves.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RtspClientConfig {
    /// Ports of our own RTSP server: a loopback URL on one of them is read
    /// in-process. Defaults to go2rtc's default `rtsp.listen` port, 8554; the
    /// binary sets the port it actually listens on.
    pub loopback_ports: Vec<u16>,
    /// Stall timeout of a camera whose source has no `#timeout=N`.
    pub default_stall_timeout: Duration,
    /// `User-Agent` sent to cameras.
    pub user_agent: String,
    /// Certificates (DER) an `rtsps` camera's may chain to, besides the
    /// operating system's, for a source that says `#verify`: a private
    /// authority.
    pub extra_roots: Vec<Vec<u8>>,
}

impl Default for RtspClientConfig {
    fn default() -> Self {
        Self {
            loopback_ports: vec![8554],
            default_stall_timeout: Duration::from_secs(15),
            user_agent: concat!("oxrtc/", env!("CARGO_PKG_VERSION")).to_owned(),
            extra_roots: Vec::new(),
        }
    }
}

/// Builds drivers for `rtsp://` and `rtsps://` sources.
#[derive(Debug, Clone)]
pub struct RtspFactory {
    config: RtspClientConfig,
    tls: Arc<Tls>,
}

impl RtspFactory {
    /// A factory with this configuration. A `default_stall_timeout` outside
    /// the range `#timeout=N` accepts (1 s to [`MAX_TIMEOUT_SECS`]) is clamped
    /// into it, and a `user_agent` that is not a valid header value (control or
    /// non-ASCII characters, which retina would panic on) is replaced by the
    /// default one.
    pub fn new(mut config: RtspClientConfig) -> Self {
        config.default_stall_timeout =
            config.default_stall_timeout.clamp(Duration::from_secs(1), Duration::from_secs(MAX_TIMEOUT_SECS));
        if !is_header_value(&config.user_agent) {
            tracing::warn!("invalid RTSP user agent in the configuration; using the default one");
            config.user_agent = RtspClientConfig::default().user_agent;
        }
        let tls = Arc::new(Tls::new(std::mem::take(&mut config.extra_roots)));
        Self { config, tls }
    }
}

/// Whether retina accepts `value` as a header value: printable ASCII, inner
/// spaces and tabs allowed. Empty is accepted too (retina then sends its own).
fn is_header_value(value: &str) -> bool {
    let bytes = value.as_bytes();
    let edge = |b: Option<&u8>| b.is_none_or(|b| matches!(b, 0x21..=0x7E));
    edge(bytes.first()) && edge(bytes.last()) && bytes.iter().all(|b| matches!(b, 0x20..=0x7E | b'\t'))
}

impl SourceFactory for RtspFactory {
    fn schemes(&self) -> &[&'static str] {
        &["rtsp", "rtsps"]
    }

    fn build(&self, spec: &SourceSpec, ctx: &FactoryCtx) -> Result<Arc<dyn SourceDriver>, SpecError> {
        match target::parse(spec, &self.config.loopback_ports, self.config.default_stall_timeout)? {
            Target::Loopback { name, query } => LocalDriver::build(ctx.stream_name(), &name, &query),
            Target::Camera(camera) => {
                Ok(Arc::new(RtspDriver::new(camera, spec.redacted(), self.config.user_agent.clone(), self.tls.clone())))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_bad_user_agent_falls_back_to_the_default() {
        let default = RtspClientConfig::default().user_agent;
        for bad in ["evil\r\nX-Injected: 1", "caf\u{e9}", " padded", "tab\t", "nul\0"] {
            let factory = RtspFactory::new(RtspClientConfig { user_agent: bad.into(), ..RtspClientConfig::default() });
            assert_eq!(factory.config.user_agent, default, "{bad:?}");
        }
        for good in ["NVR/1.0 (x86_64)", "a", ""] {
            let factory = RtspFactory::new(RtspClientConfig { user_agent: good.into(), ..RtspClientConfig::default() });
            assert_eq!(factory.config.user_agent, good);
        }
    }

    #[test]
    fn the_default_stall_timeout_is_clamped() {
        for (given, kept) in [(Duration::ZERO, 1), (Duration::MAX, MAX_TIMEOUT_SECS), (Duration::from_secs(20), 20)] {
            let factory =
                RtspFactory::new(RtspClientConfig { default_stall_timeout: given, ..RtspClientConfig::default() });
            assert_eq!(factory.config.default_stall_timeout, Duration::from_secs(kept));
        }
    }
}
