//! Wiring: one hub, its drivers, the three listeners, the configured streams.

use std::ffi::OsString;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use oxrtc_api::config::Config;
use oxrtc_api::{ApiState, Auth};
use oxrtc_core::{CancellationToken, Drivers, Hub, Limits};
use oxrtc_ffmpeg::{FfmpegConfig, FfmpegFactory, Stills};
use oxrtc_rtsp_client::{RtspClientConfig, RtspFactory};
use oxrtc_rtsp_server::{RtspServer, RtspServerConfig};
use oxrtc_webrtc::{WebRtcConfig, WebRtcServer};
use tokio::net::TcpListener;
use tokio_util::task::TaskTracker;

use crate::backends::{Answerer, LateFactory, ServerTarget, Snapshots};
use crate::listen;

/// How long [`App::shutdown`] waits for the servers' tasks.
pub const SHUTDOWN_DEADLINE: Duration = Duration::from_secs(4);

/// The version `GET /api` answers.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Why the server cannot start.
#[derive(Debug, thiserror::Error)]
pub enum StartError {
    /// The configuration cannot be used as it is.
    #[error("{0}")]
    Config(String),
    /// A listener could not be bound (the port is in use, the address is not
    /// local...).
    #[error("cannot listen on {addr} ({what}): {source}")]
    Bind {
        /// `api.listen`, `rtsp.listen` or `webrtc.listen`.
        what: &'static str,
        /// The address.
        addr: SocketAddr,
        /// What the system said.
        source: std::io::Error,
    },
}

/// The addresses actually bound (port 0 resolved).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Addresses {
    /// The HTTP API (TCP).
    pub api: SocketAddr,
    /// The RTSP server (TCP).
    pub rtsp: SocketAddr,
    /// WebRTC media (UDP).
    pub webrtc: SocketAddr,
}

/// A running server.
pub struct App {
    hub: Arc<Hub>,
    api: ApiState,
    webrtc: Arc<WebRtcServer>,
    cancel: CancellationToken,
    tasks: TaskTracker,
    addrs: Addresses,
}

impl std::fmt::Debug for App {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("App").field("addrs", &self.addrs).field("hub", &self.hub).finish_non_exhaustive()
    }
}

/// The FFmpeg executable: `ffmpeg.bin`, else `$OXRTC_FFMPEG`, else `ffmpeg`
/// from the `PATH` (go2rtc's default).
pub fn ffmpeg_bin(configured: Option<&Path>, env: Option<OsString>) -> PathBuf {
    match (configured, env) {
        (Some(path), _) => path.to_owned(),
        (None, Some(env)) if !env.is_empty() => PathBuf::from(env),
        _ => PathBuf::from("ffmpeg"),
    }
}

/// Adds every configured stream; a refused one is logged (redacted) and
/// skipped.
fn add_streams(hub: &Hub, config: &Config) {
    let mut added = 0usize;
    for (name, sources) in &config.streams {
        match hub.upsert(name, sources) {
            Ok(_) => added += 1,
            Err(error) => {
                let reason = oxrtc_core::spec::redact(&error.to_string());
                tracing::warn!(stream = %name, "streams: refused: {reason}");
            }
        }
    }
    tracing::info!(added, configured = config.streams.len(), "streams: configured");
}

fn bind_error(what: &'static str, addr: SocketAddr) -> impl FnOnce(std::io::Error) -> StartError {
    move |source| StartError::Bind { what, addr, source }
}

impl App {
    /// Checks the configuration, binds every listener, then serves and adds
    /// the configured streams. Nothing is served unless every listener could
    /// be bound. `ffmpeg_env` is `$OXRTC_FFMPEG`.
    pub async fn start(config: &Config, ffmpeg_env: Option<OsString>) -> Result<Self, StartError> {
        let auth = Auth::from_config(&config.api).map_err(|e| StartError::Config(e.to_string()))?;
        if auth == Auth::Unauthenticated {
            // D7: an open API is an explicit opt-in, and a logged one.
            tracing::warn!("api: no credentials (api.allow_unauthenticated: true): every caller controls the server");
        }
        let api_addr = listen::parse("api.listen", &config.api.listen).map_err(StartError::Config)?;
        let rtsp_addr = listen::parse("rtsp.listen", &config.rtsp.listen).map_err(StartError::Config)?;
        let webrtc_addr = listen::parse("webrtc.listen", &config.webrtc.listen).map_err(StartError::Config)?;

        let rtsp_factory = LateFactory::new(&["rtsp", "rtsps"]);
        let ffmpeg_factory = LateFactory::new(&["ffmpeg"]);
        let mut drivers = Drivers::new();
        drivers.register(rtsp_factory.clone());
        drivers.register(ffmpeg_factory.clone());
        let hub = Hub::new(drivers, Limits::default());

        let cancel = CancellationToken::new();
        let api_listener = TcpListener::bind(api_addr).await.map_err(bind_error("api.listen", api_addr))?;
        let rtsp_config = RtspServerConfig { listen: rtsp_addr, ..RtspServerConfig::default() };
        let rtsp = RtspServer::bind(rtsp_config, hub.clone()).await.map_err(bind_error("rtsp.listen", rtsp_addr))?;
        let webrtc_config = WebRtcConfig {
            listen: webrtc_addr,
            candidates: config.webrtc.candidates.clone(),
            ..WebRtcConfig::default()
        };
        let webrtc = WebRtcServer::bind(webrtc_config, cancel.clone())
            .await
            .map_err(bind_error("webrtc.listen", webrtc_addr))?;
        let webrtc = Arc::new(webrtc);
        let addrs = Addresses {
            api: api_listener.local_addr().map_err(bind_error("api.listen", api_addr))?,
            rtsp: rtsp.local_addr(),
            webrtc: webrtc.local_addr(),
        };

        // The factories, now that the RTSP server's port and tokens exist.
        let rtsp_client = RtspClientConfig { loopback_ports: vec![addrs.rtsp.port()], ..RtspClientConfig::default() };
        let ffmpeg = FfmpegConfig {
            bin: ffmpeg_bin(config.ffmpeg.bin.as_deref(), ffmpeg_env),
            templates: config.ffmpeg.templates.clone(),
            relay: listen::reachable(addrs.rtsp).to_string(),
            ..FfmpegConfig::default()
        };
        let set = rtsp_factory.set(Arc::new(RtspFactory::new(rtsp_client)))
            & ffmpeg_factory
                .set(Arc::new(FfmpegFactory::new(ffmpeg.clone(), Arc::new(ServerTarget(rtsp.publishers())))));
        if !set {
            return Err(StartError::Config("source factories set twice".into()));
        }

        let api = ApiState::new(hub.clone(), VERSION, auth)
            .with_webrtc(Arc::new(Answerer(webrtc.clone())))
            .with_snapshots(Arc::new(Snapshots(Arc::new(Stills::new(ffmpeg)))));
        if !config.webrtc.ice_servers.is_empty() {
            tracing::info!("webrtc.ice_servers is not used yet (TURN is on the roadmap); ignored");
        }

        // The configured streams exist before the first request is served: a
        // client that sees `GET /api` answer can use them at once.
        add_streams(&hub, config);

        let tasks = TaskTracker::new();
        let token = cancel.child_token();
        tasks.spawn(async move {
            if let Err(error) = rtsp.run(token).await {
                tracing::error!("rtsp: server stopped: {error}");
            }
        });
        let router = oxrtc_api::router(api.clone());
        let token = cancel.child_token();
        tasks.spawn(async move {
            if let Err(error) = oxrtc_api::serve(api_listener, router, token).await {
                tracing::error!("api: server stopped: {error}");
            }
        });
        tracing::info!(addr = %addrs.api, "api: listen");
        tracing::info!(addr = %addrs.rtsp, "rtsp: listen");
        // The WebRTC server logs its own "webrtc listening" line.

        Ok(Self { hub, api, webrtc, cancel, tasks, addrs })
    }

    /// The bound addresses.
    pub fn addresses(&self) -> Addresses {
        self.addrs
    }

    /// The hub (for tests and diagnostics).
    pub fn hub(&self) -> &Arc<Hub> {
        &self.hub
    }

    /// Stops everything: cancels the root token, releases the preloads,
    /// closes every stream, and waits for the servers' tasks at most
    /// [`SHUTDOWN_DEADLINE`]. Returns whether they all ended in time.
    pub async fn shutdown(self) -> bool {
        self.cancel.cancel();
        self.api.release_preloads();
        self.hub.shutdown();
        self.webrtc.shutdown();
        self.tasks.close();
        let in_time = tokio::time::timeout(SHUTDOWN_DEADLINE, self.tasks.wait()).await.is_ok();
        if !in_time {
            tracing::warn!("tasks still running {SHUTDOWN_DEADLINE:?} after shutdown; exiting anyway");
        }
        in_time
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_ffmpeg_binary_is_the_configured_one_then_the_environment_then_the_path() {
        let env = || Some(OsString::from("C:/env/ffmpeg.exe"));
        assert_eq!(ffmpeg_bin(Some(Path::new("/opt/ffmpeg")), env()), PathBuf::from("/opt/ffmpeg"));
        assert_eq!(ffmpeg_bin(None, env()), PathBuf::from("C:/env/ffmpeg.exe"));
        assert_eq!(ffmpeg_bin(None, Some(OsString::new())), PathBuf::from("ffmpeg"));
        assert_eq!(ffmpeg_bin(None, None), PathBuf::from("ffmpeg"));
    }
}
