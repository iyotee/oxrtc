//! Wiring: one hub, its drivers, the three listeners, the configured streams.

use std::ffi::OsString;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use oxrtc_api::config::{Config, QueueLimits};
use oxrtc_api::{ApiState, Auth, ServeLimits};
use oxrtc_core::{CancellationToken, Drivers, Hub, Limits, QueueBudget};
use oxrtc_ffmpeg::{FfmpegConfig, FfmpegFactory, Stills};
use oxrtc_http_client::{HttpClientConfig, HttpFactory};
use oxrtc_rtsp_client::{RtspClientConfig, RtspFactory};
use oxrtc_rtsp_server::{ReadAuth, RtspServer, RtspServerConfig};
use oxrtc_webrtc::{WebRtcConfig, WebRtcServer};
use tokio::net::TcpListener;
use tokio_util::task::TaskTracker;

use crate::backends::{Answerer, LateFactory, ServerTarget, Snapshots, ice_servers};
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

/// The RTSP server's settings: `rtsp.listen` and the oxrtc-only limits.
fn rtsp_config(config: &Config, listen: SocketAddr) -> RtspServerConfig {
    let defaults = RtspServerConfig::default();
    RtspServerConfig {
        listen,
        max_connections_per_ip: config.rtsp.max_connections_per_ip.unwrap_or(defaults.max_connections_per_ip),
        // Both halves or none: the configuration refused anything else.
        auth: match (&config.rtsp.username, &config.rtsp.password) {
            (Some(user), Some(password)) => Some(ReadAuth::new(user, password).local(config.rtsp.local_auth)),
            _ => None,
        },
        // What the transcodes' FFmpeg waits on its input: a transcode's
        // answer is never held for the pace of its pictures past it (D36).
        transcode_timeout: oxrtc_ffmpeg::input_timeout(&config.ffmpeg.templates),
        ..defaults
    }
}

/// The WebRTC server's settings: `webrtc.listen`, `webrtc.candidates`,
/// `webrtc.ice_servers` and the oxrtc-only limits.
fn webrtc_config(config: &Config, listen: SocketAddr) -> WebRtcConfig {
    let defaults = WebRtcConfig::default();
    WebRtcConfig {
        listen,
        candidates: config.webrtc.candidates.clone(),
        ice_servers: ice_servers(&config.webrtc.ice_servers),
        max_sessions: config.webrtc.max_sessions.unwrap_or(defaults.max_sessions),
        max_sessions_per_ip: config.webrtc.max_sessions_per_ip.unwrap_or(defaults.max_sessions_per_ip),
        playout: playout(config, defaults.playout),
        ..defaults
    }
}

/// `webrtc.lip_sync`, `webrtc.max_sound_late_ms` and `webrtc.sound_lag_ms`
/// (D30): `0` sends all the sound, or dates it when it arrived; an absent key
/// keeps the server's value.
fn playout(config: &Config, defaults: oxrtc_webrtc::Playout) -> oxrtc_webrtc::Playout {
    let late = match config.webrtc.max_sound_late_ms {
        Some(0) => None,
        Some(ms) => Some(std::time::Duration::from_millis(ms)),
        None => defaults.sound_late_max,
    };
    let sound_lag = config.webrtc.sound_lag_ms.map_or(defaults.sound_lag, std::time::Duration::from_millis);
    oxrtc_webrtc::Playout { lip_sync: config.webrtc.lip_sync, sound_late_max: late, sound_lag, ..defaults }
}

/// A MiB count in bytes (the parser bounds it: `config::MAX_MIB`).
fn mib(count: usize) -> usize {
    count.saturating_mul(1024 * 1024)
}

/// A queue budget: the configured parts over the default's.
fn queue_budget(configured: QueueLimits, default: QueueBudget) -> QueueBudget {
    QueueBudget {
        max_bytes: configured.max_mib.map_or(default.max_bytes, mib),
        max_age: configured.max_seconds.map_or(default.max_age, |s| Duration::from_secs(s as u64)),
    }
}

/// The hub's limits: the oxrtc-only keys of `api:` (D16) and the queue
/// budgets of `rtsp:` and `webrtc:` (D15), each over its default.
fn hub_limits(config: &Config) -> Limits {
    let defaults = Limits::default();
    let streams = config.api.streams;
    Limits {
        max_streams: streams.max_streams.unwrap_or(defaults.max_streams),
        max_consumers_per_stream: streams.max_consumers_per_stream.unwrap_or(defaults.max_consumers_per_stream),
        max_frame_bytes: streams.max_frame_mib.map_or(defaults.max_frame_bytes, mib),
        gop_cache_bytes: streams.gop_cache_mib.map_or(defaults.gop_cache_bytes, mib),
        idle_grace: streams.idle_grace_ms.map_or(defaults.idle_grace, Duration::from_millis),
        transcode_idle_grace: streams
            .transcode_idle_grace_ms
            .map_or(defaults.transcode_idle_grace, Duration::from_millis),
        live_queue: queue_budget(config.webrtc.queue, defaults.live_queue),
        recorder_queue: queue_budget(config.rtsp.queue, defaults.recorder_queue),
        ..defaults
    }
}

/// The API listener's bounds.
fn serve_limits(config: &Config) -> ServeLimits {
    let defaults = ServeLimits::default();
    ServeLimits {
        max_connections_per_ip: config.api.max_connections_per_ip.unwrap_or(defaults.max_connections_per_ip),
        ..defaults
    }
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
        // Webcams need nothing the server binds: registered as they are.
        drivers.register(Arc::new(HttpFactory::new(HttpClientConfig::default())));
        let hub = Hub::new(drivers, hub_limits(config));

        let cancel = CancellationToken::new();
        let api_listener = TcpListener::bind(api_addr).await.map_err(bind_error("api.listen", api_addr))?;
        let rtsp = RtspServer::bind(rtsp_config(config, rtsp_addr), hub.clone())
            .await
            .map_err(bind_error("rtsp.listen", rtsp_addr))?;
        let webrtc = WebRtcServer::bind(webrtc_config(config, webrtc_addr), cancel.clone())
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
        let ffmpeg_defaults = FfmpegConfig::default();
        let bin = ffmpeg_bin(config.ffmpeg.bin.as_deref(), ffmpeg_env);
        let confine = crate::confine::ffmpeg(&bin, config.ffmpeg.max_memory_mib).await;
        let ffmpeg = FfmpegConfig {
            bin,
            confine,
            templates: config.ffmpeg.templates.clone(),
            relay: listen::reachable(addrs.rtsp).to_string(),
            max_processes: config.ffmpeg.max_processes.unwrap_or(ffmpeg_defaults.max_processes),
            ..ffmpeg_defaults
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
        let limits = serve_limits(config);
        tasks.spawn(async move {
            if let Err(error) = oxrtc_api::serve_with_limits(api_listener, router, token, limits).await {
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
    /// closes every stream, and waits at most [`SHUTDOWN_DEADLINE`] for the
    /// servers' tasks, the WebRTC server's own included, and for the WebRTC
    /// server to have no holder left but this one: its UDP port is bound
    /// until then. Returns whether all of it ended in time.
    pub async fn shutdown(self) -> bool {
        let Self { hub, api, webrtc, cancel, tasks, addrs: _ } = self;
        let deadline = tokio::time::Instant::now() + SHUTDOWN_DEADLINE;
        cancel.cancel();
        api.release_preloads();
        hub.shutdown();
        webrtc.shutdown();
        tasks.close();
        let left = || deadline.saturating_duration_since(tokio::time::Instant::now());
        let ended = tokio::time::timeout(left(), tasks.wait()).await.is_ok();
        // The API's state holds the WebRTC server (it answers `POST
        // /api/webrtc`): this handle on it, and every API connection still
        // being served - one whose request is in flight past the API's own
        // grace, and for an instant one that has just ended.
        drop(api);
        let in_time = ended && released(&webrtc, left()).await && webrtc.stopped(left()).await;
        if !in_time {
            tracing::warn!("tasks still running {SHUTDOWN_DEADLINE:?} after shutdown; exiting anyway");
        }
        in_time
    }
}

/// Waits, `limit` at most, until `held` has no holder but the caller's.
async fn released<T>(held: &Arc<T>, limit: Duration) -> bool {
    let deadline = tokio::time::Instant::now() + limit;
    while Arc::strong_count(held) > 1 {
        if tokio::time::Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    true
}

#[cfg(test)]
#[path = "app_tests.rs"]
mod tests;
