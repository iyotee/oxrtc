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

mod yaml;

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
        }
    }
}

/// `rtsp:`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RtspSection {
    /// Address of the RTSP server.
    pub listen: String,
    /// `rtsp.max_connections_per_ip` (oxrtc only): connections one peer
    /// address holds at once; `None` is the server's default.
    pub max_connections_per_ip: Option<usize>,
}

impl Default for RtspSection {
    fn default() -> Self {
        Self { listen: DEFAULT_RTSP_LISTEN.to_owned(), max_connections_per_ip: None }
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
}

impl Default for WebRtcSection {
    fn default() -> Self {
        Self {
            listen: DEFAULT_WEBRTC_LISTEN.to_owned(),
            candidates: Vec::new(),
            ice_servers: Vec::new(),
            max_sessions: None,
            max_sessions_per_ip: None,
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
