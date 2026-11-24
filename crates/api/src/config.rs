//! The configuration file: the subset of go2rtc's YAML that openIVR writes.
//!
//! The file holds camera credentials: it is read, never written back, and the
//! [`Debug`] form of every type here hides passwords, tokens, ICE credentials
//! and the user information of stream sources.

use std::collections::BTreeMap;
use std::fmt;
use std::io::Read;
use std::path::{Path, PathBuf};

use thiserror::Error;

mod output_memory;
mod permissions;
mod yaml;

pub use output_memory::OutputMemory;
pub use permissions::{PERMISSION_KEYS, Permissions, ReportedVersion};

/// Largest configuration file accepted.
pub const MAX_CONFIG_BYTES: usize = 1024 * 1024;

/// Default `api.listen`.
pub const DEFAULT_API_LISTEN: &str = "127.0.0.1:1984";
/// Default `rtsp.listen`.
pub const DEFAULT_RTSP_LISTEN: &str = "127.0.0.1:8554";
/// Default `webrtc.listen`.
pub const DEFAULT_WEBRTC_LISTEN: &str = ":8555";
/// Default `log.level`.
pub const DEFAULT_LOG_LEVEL: &str = "info";
/// Largest value of a size key in MiB (`*_mib`, oxrtc only).
pub const MAX_MIB: usize = 4096;
/// Largest value of a duration key in seconds (`*_seconds`, oxrtc only).
pub const MAX_SECONDS: usize = 3600;
/// Largest `webrtc.sound_lag_ms`: the most the owner's answer of 2026-11-19
/// allows (D30; the WebRTC server's own bound - the binary's tests hold the
/// two together).
pub const MAX_SOUND_LAG_MS: u64 = 40;

/// Why a configuration cannot be used.
#[derive(Debug, Error)]
pub enum ConfigError {
    /// The file could not be read.
    #[error("cannot read {path}: {source}")]
    Io {
        /// The file.
        path: PathBuf,
        /// What failed.
        source: std::io::Error,
    },
    /// The file is larger than [`MAX_CONFIG_BYTES`].
    #[error("configuration larger than {limit} bytes")]
    TooLarge {
        /// The limit.
        limit: usize,
    },
    /// The text is not valid YAML.
    #[error("invalid YAML: {0}")]
    Yaml(String),
    /// A key holds a value of the wrong shape.
    #[error("invalid configuration: {0}")]
    Invalid(String),
    /// No credential is configured and the unauthenticated API was not asked for.
    #[error(
        "the API needs api.username/api.password or api.token (or api.allow_unauthenticated: true, \
         which leaves every local process in control of the server)"
    )]
    NoAuth,
}

/// The whole file.
#[derive(Clone, PartialEq, Eq, Default)]
pub struct Config {
    /// `api:`.
    pub api: ApiConfig,
    /// `rtsp:`.
    pub rtsp: RtspSection,
    /// `webrtc:`.
    pub webrtc: WebRtcSection,
    /// `streams:`, in file order: name and sources.
    pub streams: Vec<(String, Vec<String>)>,
    /// `ffmpeg:`.
    pub ffmpeg: FfmpegSection,
    /// `log:`.
    pub log: LogSection,
}

/// `api:`.
#[derive(Clone, PartialEq, Eq)]
pub struct ApiConfig {
    /// Address of the HTTP API.
    pub listen: String,
    /// HTTP Basic user name.
    pub username: Option<String>,
    /// HTTP Basic password.
    pub password: Option<String>,
    /// Bearer token.
    pub token: Option<String>,
    /// Explicit opt-in to an API without authentication.
    pub allow_unauthenticated: bool,
    /// `api.max_connections_per_ip` (oxrtc only): connections one peer
    /// address keeps open at once; `None` is the server's default.
    pub max_connections_per_ip: Option<usize>,
    /// `api.origin` (go2rtc's): which browser pages may open `/api/ws`.
    /// Empty or absent, a page of the request's own `Host` (of that host on
    /// any port when the `Host` names none); `*`, any page; anything else, a
    /// page of exactly the request's `Host`.
    pub origin: Option<String>,
    /// The streams' own limits (oxrtc only, D16): the hub the API manages.
    pub streams: StreamLimits,
    /// `api.output_memory` (oxrtc only, D45): the memory budget of every
    /// reader that writes over HTTP or a WebSocket; `None` is one eighth of
    /// what is available to the process.
    pub output_memory: Option<OutputMemory>,
    /// `api.allow_*` (oxrtc only, D48): the routes that change the server.
    pub permissions: Permissions,
    /// `api.reported_version` (oxrtc only, D48): what `GET /api`'s `version` says.
    pub reported_version: ReportedVersion,
}

/// The streams' limits, in `api:` (D16); `None` is the hub's default
/// (`oxrtc_core::Limits`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct StreamLimits {
    /// `api.max_streams`: streams at once.
    pub max_streams: Option<usize>,
    /// `api.max_consumers_per_stream`: viewers, readers and preloads of one stream.
    pub max_consumers_per_stream: Option<usize>,
    /// `api.max_frame_mib`: the largest frame a source may publish, in MiB.
    pub max_frame_mib: Option<usize>,
    /// `api.gop_cache_mib`: the group of pictures kept per source, in MiB.
    pub gop_cache_mib: Option<usize>,
    /// `api.idle_grace_ms`: how long a source with no consumer left keeps
    /// running (a camera's session), in milliseconds.
    pub idle_grace_ms: Option<u64>,
    /// `api.transcode_idle_grace_ms`: the same for an `ffmpeg:` source that
    /// encodes; one that only copies keeps `idle_grace_ms` (D36).
    pub transcode_idle_grace_ms: Option<u64>,
}

/// A consumer queue's budget (oxrtc only, D15): `rtsp.queue_*` for the RTSP
/// server's readers, `webrtc.queue_*` for live consumers. `None` is the
/// hub's default.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct QueueLimits {
    /// `*.queue_max_mib`: bytes one consumer's queue holds, in MiB.
    pub max_mib: Option<usize>,
    /// `*.queue_max_seconds`: how long its oldest item may wait.
    pub max_seconds: Option<usize>,
}

impl Default for ApiConfig {
    fn default() -> Self {
        Self {
            listen: DEFAULT_API_LISTEN.to_owned(),
            username: None,
            password: None,
            token: None,
            allow_unauthenticated: false,
            max_connections_per_ip: None,
            origin: None,
            streams: StreamLimits::default(),
            output_memory: None,
            permissions: Permissions::default(),
            reported_version: ReportedVersion::default(),
        }
    }
}

/// Longest `rtsp.username` or `rtsp.password`, in bytes: both ride in one
/// `Authorization` header the server reads up to 1,024 bytes of.
pub const MAX_RTSP_CREDENTIAL_BYTES: usize = 256;

/// `rtsp:`.
#[derive(Clone, PartialEq, Eq)]
pub struct RtspSection {
    /// Address of the RTSP server.
    pub listen: String,
    /// `rtsp.username`: with `password`, what every RTSP reader must give
    /// (Digest or Basic). Both or neither.
    pub username: Option<String>,
    /// `rtsp.password`.
    pub password: Option<String>,
    /// `rtsp.local_auth` (oxrtc only, default `true`): whether a reader on
    /// loopback must give it too. `false` lets loopback in unchecked, which is
    /// what go2rtc 1.9.15 does (D24).
    pub local_auth: bool,
    /// `rtsp.max_connections_per_ip` (oxrtc only): connections one peer
    /// address holds at once; `None` is the server's default.
    pub max_connections_per_ip: Option<usize>,
    /// `rtsp.queue_max_mib` / `rtsp.queue_max_seconds` (oxrtc only): the
    /// budget of an RTSP reader's queue (a recorder, D15).
    pub queue: QueueLimits,
}

impl Default for RtspSection {
    fn default() -> Self {
        Self {
            listen: DEFAULT_RTSP_LISTEN.to_owned(),
            username: None,
            password: None,
            local_auth: true,
            max_connections_per_ip: None,
            queue: QueueLimits::default(),
        }
    }
}

impl fmt::Debug for RtspSection {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RtspSection")
            .field("listen", &self.listen)
            .field("username", &self.username)
            .field("password", &secret(&self.password))
            .field("local_auth", &self.local_auth)
            .field("max_connections_per_ip", &self.max_connections_per_ip)
            .field("queue", &self.queue)
            .finish()
    }
}

/// `webrtc:`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WebRtcSection {
    /// The UDP (and TCP) address of WebRTC media.
    pub listen: String,
    /// Extra host candidates, with or without `:port`.
    pub candidates: Vec<String>,
    /// STUN/TURN servers: the server-reflexive and relayed candidates a
    /// viewer outside the LAN needs (`oxrtc_webrtc::IceServer`).
    pub ice_servers: Vec<IceServer>,
    /// `webrtc.max_sessions` (oxrtc only): WebRTC sessions at once, from every
    /// caller together; `None` is the server's default.
    pub max_sessions: Option<usize>,
    /// `webrtc.max_sessions_per_ip` (oxrtc only): WebRTC sessions requested
    /// from one address at once; `None` is the server's default.
    pub max_sessions_per_ip: Option<usize>,
    /// `webrtc.queue_max_mib` / `webrtc.queue_max_seconds` (oxrtc only): the
    /// budget of a live consumer's queue - a WebRTC viewer, and the API's
    /// stills and preloads, of the same class (D15).
    pub queue: QueueLimits,
    /// `webrtc.lip_sync` (oxrtc only): `true` (the default) lets the browser
    /// hold the picture for the sound, as go2rtc and MediaMTX do; `false`
    /// asks it to show each picture as soon as it can (the `playout-delay`
    /// header extension, D30), the sound then trailing by its own buffer.
    pub lip_sync: bool,
    /// `webrtc.max_sound_late_ms` (oxrtc only): sound further behind its
    /// stream's clock than this is not sent to a viewer (what a stalled
    /// camera delivers at once when it comes back); `0` sends all of it;
    /// `None` is the server's default.
    pub max_sound_late_ms: Option<u64>,
    /// `webrtc.sound_lag_ms` (oxrtc only): with lip sync on, how many
    /// milliseconds later than the picture that arrived with it the sound is
    /// dated, so that the browser shows the picture it holds for the sound
    /// that much sooner (D30); `0` dates both when they arrived; `None` is
    /// the server's default.
    pub sound_lag_ms: Option<u64>,
}

impl Default for WebRtcSection {
    fn default() -> Self {
        Self {
            listen: DEFAULT_WEBRTC_LISTEN.to_owned(),
            candidates: Vec::new(),
            ice_servers: Vec::new(),
            max_sessions: None,
            max_sessions_per_ip: None,
            queue: QueueLimits::default(),
            lip_sync: true,
            max_sound_late_ms: None,
            sound_lag_ms: None,
        }
    }
}

/// One entry of `webrtc.ice_servers`.
#[derive(Clone, PartialEq, Eq, Default)]
pub struct IceServer {
    /// `stun:` / `turn:` URLs.
    pub urls: Vec<String>,
    /// TURN user name.
    pub username: Option<String>,
    /// TURN credential.
    pub credential: Option<String>,
}

/// `ffmpeg:`.
#[derive(Clone, PartialEq, Eq, Default)]
pub struct FfmpegSection {
    /// `ffmpeg.bin`: the FFmpeg executable.
    pub bin: Option<PathBuf>,
    /// `ffmpeg.max_processes` (oxrtc only): FFmpeg processes at once, stills
    /// included; `None` is the default. go2rtc reads the key as a template
    /// nothing names, so a file that carries it still runs there.
    pub max_processes: Option<usize>,
    /// `ffmpeg.max_memory_mib` (oxrtc only): the address space of each FFmpeg
    /// confined on Linux, in MiB; `None` is the default (4096). Read as a
    /// template nothing names by go2rtc, like `max_processes`.
    pub max_memory_mib: Option<usize>,
    /// `ffmpeg.native_audio` (oxrtc only): a sound transcode of G.711 or L16
    /// that oxrtc can read itself is made in process, with no FFmpeg (W5,
    /// D37); `None` is the default (on). go2rtc reads it as a template
    /// nothing names.
    pub native_audio: Option<bool>,
    /// Every other key: named argument templates.
    pub templates: BTreeMap<String, String>,
}

/// `log:`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogSection {
    /// `trace`, `debug`, `info`, `warn`, `error`.
    pub level: String,
}

impl Default for LogSection {
    fn default() -> Self {
        Self { level: DEFAULT_LOG_LEVEL.to_owned() }
    }
}

fn secret(value: &Option<String>) -> Option<&'static str> {
    value.as_ref().map(|_| "***")
}

impl fmt::Debug for ApiConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ApiConfig")
            .field("listen", &self.listen)
            .field("username", &self.username)
            .field("password", &secret(&self.password))
            .field("token", &secret(&self.token))
            .field("allow_unauthenticated", &self.allow_unauthenticated)
            .field("max_connections_per_ip", &self.max_connections_per_ip)
            .field("origin", &self.origin)
            .field("streams", &self.streams)
            .field("output_memory", &self.output_memory)
            .field("permissions", &self.permissions)
            .field("reported_version", &self.reported_version)
            .finish()
    }
}

impl fmt::Debug for IceServer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let urls: Vec<String> = self.urls.iter().map(|u| oxrtc_core::spec::redact(u)).collect();
        // The TURN user name is half of the credential: openIVR's relay
        // writes its expiry and the site it bills into it.
        f.debug_struct("IceServer")
            .field("urls", &urls)
            .field("username", &secret(&self.username))
            .field("credential", &secret(&self.credential))
            .finish()
    }
}

impl fmt::Debug for FfmpegSection {
    /// Templates are redacted too: an operator may write a fixed input URL,
    /// credentials included, into one.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let templates: BTreeMap<&str, String> =
            self.templates.iter().map(|(name, text)| (name.as_str(), oxrtc_core::spec::redact(text))).collect();
        f.debug_struct("FfmpegSection")
            .field("bin", &self.bin)
            .field("max_processes", &self.max_processes)
            .field("max_memory_mib", &self.max_memory_mib)
            .field("native_audio", &self.native_audio)
            .field("templates", &templates)
            .finish()
    }
}

impl fmt::Debug for Config {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let streams: Vec<(&str, Vec<String>)> = self
            .streams
            .iter()
            .map(|(name, sources)| (name.as_str(), sources.iter().map(|s| oxrtc_core::spec::redact(s)).collect()))
            .collect();
        f.debug_struct("Config")
            .field("api", &self.api)
            .field("rtsp", &self.rtsp)
            .field("webrtc", &self.webrtc)
            .field("streams", &streams)
            .field("ffmpeg", &self.ffmpeg)
            .field("log", &self.log)
            .finish()
    }
}

/// Reads and parses a configuration file. Warnings about unknown keys are
/// logged (they never hold a value, only a key name).
pub fn load(path: &Path) -> Result<Config, ConfigError> {
    let io = |source| ConfigError::Io { path: path.to_owned(), source };
    let file = std::fs::File::open(path).map_err(io)?;
    let mut text = String::new();
    let cap = u64::try_from(MAX_CONFIG_BYTES).unwrap_or(u64::MAX).saturating_add(1);
    file.take(cap).read_to_string(&mut text).map_err(io)?;
    let (config, warnings) = parse(&text)?;
    for warning in warnings {
        tracing::warn!("config: {warning}");
    }
    Ok(config)
}

/// Parses configuration text. Returns the configuration and one warning per
/// unknown key (ignored).
pub fn parse(text: &str) -> Result<(Config, Vec<String>), ConfigError> {
    if text.len() > MAX_CONFIG_BYTES {
        return Err(ConfigError::TooLarge { limit: MAX_CONFIG_BYTES });
    }
    yaml::parse(text)
}
