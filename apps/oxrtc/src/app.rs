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

/// `webrtc.lip_sync` and `webrtc.max_sound_late_ms` (D30): `0` sends all
/// the sound, an absent key keeps the server's bound.
fn playout(config: &Config, defaults: oxrtc_webrtc::Playout) -> oxrtc_webrtc::Playout {
    let late = match config.webrtc.max_sound_late_ms {
        Some(0) => None,
        Some(ms) => Some(std::time::Duration::from_millis(ms)),
        None => defaults.sound_late_max,
    };
    oxrtc_webrtc::Playout { lip_sync: config.webrtc.lip_sync, sound_late_max: late, ..defaults }
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
    fn the_per_address_limits_reach_every_listener() {
        let addr = SocketAddr::from(([127, 0, 0, 1], 0));
        let (configured, _) = oxrtc_api::config::parse(
            "api:
  max_connections_per_ip: 3
rtsp:
  max_connections_per_ip: 4
webrtc:
  max_sessions: 200
  max_sessions_per_ip: 5
",
        )
        .unwrap();
        assert_eq!(serve_limits(&configured).max_connections_per_ip, 3);
        assert_eq!(rtsp_config(&configured, addr).max_connections_per_ip, 4);
        assert_eq!(webrtc_config(&configured, addr).max_sessions_per_ip, 5);
        assert_eq!(webrtc_config(&configured, addr).max_sessions, 200);
        let unset = Config::default();
        assert_eq!(serve_limits(&unset), ServeLimits::default());
        assert_eq!(rtsp_config(&unset, addr).max_connections_per_ip, oxrtc_rtsp_server::DEFAULT_MAX_CONNECTIONS_PER_IP);
        assert_eq!(webrtc_config(&unset, addr).max_sessions_per_ip, oxrtc_webrtc::DEFAULT_MAX_SESSIONS_PER_IP);
        assert_eq!(webrtc_config(&unset, addr).max_sessions, oxrtc_webrtc::DEFAULT_MAX_SESSIONS);
    }

    #[test]
    fn lip_sync_and_the_sound_bound_reach_the_webrtc_server() {
        let addr = SocketAddr::from(([127, 0, 0, 1], 0));
        let unset = webrtc_config(&Config::default(), addr).playout;
        assert_eq!(unset, oxrtc_webrtc::Playout::default());
        assert!(unset.lip_sync && !unset.pace_sound, "lip sync on, the sound not paced (W1b review)");
        assert_eq!(unset.sound_late_max, Some(oxrtc_webrtc::DEFAULT_SOUND_LATE_MAX));
        let parsed = |text: &str| webrtc_config(&oxrtc_api::config::parse(text).unwrap().0, addr).playout;
        let live = parsed("webrtc:\n  lip_sync: false\n  max_sound_late_ms: 250\n");
        assert!(!live.lip_sync);
        assert_eq!(live.sound_late_max, Some(std::time::Duration::from_millis(250)));
        assert_eq!(parsed("webrtc:\n  max_sound_late_ms: 0\n").sound_late_max, None, "0: all the sound is sent");
    }

    #[test]
    fn the_stream_limits_and_queue_budgets_reach_the_hub() {
        let (configured, warnings) = oxrtc_api::config::parse(
            "api:
  max_streams: 40
  max_consumers_per_stream: 12
  max_frame_mib: 16
  gop_cache_mib: 24
rtsp:
  queue_max_mib: 64
  queue_max_seconds: 20
webrtc:
  queue_max_seconds: 1
",
        )
        .unwrap();
        assert!(warnings.is_empty(), "{warnings:?}");
        let limits = hub_limits(&configured);
        let defaults = Limits::default();
        let expected = Limits {
            max_streams: 40,
            max_consumers_per_stream: 12,
            max_frame_bytes: 16 << 20,
            gop_cache_bytes: 24 << 20,
            recorder_queue: QueueBudget { max_bytes: 64 << 20, max_age: Duration::from_secs(20) },
            // A part left out keeps its default.
            live_queue: QueueBudget { max_age: Duration::from_secs(1), ..defaults.live_queue },
            ..defaults.clone()
        };
        assert_eq!(limits, expected);
        assert_eq!(hub_limits(&Config::default()), defaults, "a file written for go2rtc keeps every default");
    }

    #[test]
    fn the_ice_servers_reach_the_webrtc_server_as_openivr_writes_them() {
        let addr = SocketAddr::from(([127, 0, 0, 1], 0));
        // openIVR's _write_config, once Remote Access is paired.
        let (configured, _) = oxrtc_api::config::parse(
            "webrtc:
  listen: ':8555'
  candidates: [192.168.1.42]
  ice_servers:
  - urls: ['stun:relay.example.org:3478', 'turn:relay.example.org:3478?transport=udp']
    username: '1700000000:site'
    credential: c2VjcmV0
",
        )
        .unwrap();
        let webrtc = webrtc_config(&configured, addr);
        assert_eq!(webrtc.ice_servers.len(), 1);
        assert_eq!(webrtc.ice_servers[0].urls.len(), 2);
        assert_eq!(webrtc.ice_servers[0].username.as_deref(), Some("1700000000:site"));
        assert_eq!(webrtc.ice_servers[0].credential.as_deref(), Some("c2VjcmV0"));
        assert!(webrtc.tcp, "ICE-TCP on the UDP port, as go2rtc");
        assert!(!format!("{webrtc:?}").contains("c2VjcmV0"), "the credential is not printed");
    }

    #[test]
    fn the_ffmpeg_binary_is_the_configured_one_then_the_environment_then_the_path() {
        let env = || Some(OsString::from("C:/env/ffmpeg.exe"));
        assert_eq!(ffmpeg_bin(Some(Path::new("/opt/ffmpeg")), env()), PathBuf::from("/opt/ffmpeg"));
        assert_eq!(ffmpeg_bin(None, env()), PathBuf::from("C:/env/ffmpeg.exe"));
        assert_eq!(ffmpeg_bin(None, Some(OsString::new())), PathBuf::from("ffmpeg"));
        assert_eq!(ffmpeg_bin(None, None), PathBuf::from("ffmpeg"));
    }
}
