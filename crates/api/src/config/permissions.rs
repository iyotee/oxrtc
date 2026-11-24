//! `api.allow_*` (oxrtc only, D48): what the API may change in the server.
//!
//! Set in the file alone: no route can turn one on (a configuration written
//! through the API keeps the file's values, `crate::control`).

use serde_norway::Value;

use super::ConfigError;

/// Which of the API's routes that change the server answer; the others are
/// refused with `403`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Permissions {
    /// `api.allow_stream_changes` (default `true`): `PUT`, `PATCH`, `POST`
    /// and `DELETE /api/streams`. On by default because openIVR and Home
    /// Assistant both add their cameras that way, and a stream's sources are
    /// held to the same rules as the file's (no scheme runs a command or reads
    /// a file).
    pub stream_changes: bool,
    /// `api.allow_config_read`: `GET /api/config`, the file as written -
    /// camera and API credentials included.
    pub config_read: bool,
    /// `api.allow_config_write`: `POST` and `PATCH /api/config` (and the
    /// read, which an editor needs).
    pub config_write: bool,
    /// `api.allow_restart`: `POST /api/restart`.
    pub restart: bool,
    /// `api.allow_exit`: `POST /api/exit`.
    pub exit: bool,
}

impl Default for Permissions {
    fn default() -> Self {
        Self { stream_changes: true, config_read: false, config_write: false, restart: false, exit: false }
    }
}

impl Permissions {
    /// Whether `GET /api/config` answers.
    pub fn reads_config(&self) -> bool {
        self.config_read || self.config_write
    }
}

/// `api.reported_version` (oxrtc only, D48): what `GET /api`'s `version` says.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ReportedVersion {
    /// `go2rtc` (the default): the go2rtc API this server answers, the release
    /// as build metadata (`1.9.15+oxrtc.0.2.0`), which go2rtc's clients check.
    #[default]
    Go2rtc,
    /// `oxrtc`: the release alone (`0.2.0`), as before 0.3.0, for a tool that
    /// compares `version` with oxrtc's releases (`oxrtc_version` says it in
    /// both cases).
    Oxrtc,
}

/// Reads `api.reported_version`.
pub(super) fn reported_version(value: &Value, path: &str) -> Result<ReportedVersion, ConfigError> {
    match value {
        Value::Null => Ok(ReportedVersion::default()),
        Value::String(s) if s == "go2rtc" => Ok(ReportedVersion::Go2rtc),
        Value::String(s) if s == "oxrtc" => Ok(ReportedVersion::Oxrtc),
        _ => Err(ConfigError::Invalid(format!("{path}: expected go2rtc or oxrtc"))),
    }
}

/// Every `api.` key of a permission, in the order the documents list them.
pub const PERMISSION_KEYS: [&str; 5] =
    ["allow_stream_changes", "allow_config_read", "allow_config_write", "allow_restart", "allow_exit"];

/// Reads `api.<key>` into `permissions` when it is a permission; whether it was.
pub(super) fn key(permissions: &mut Permissions, key: &str, value: &Value, path: &str) -> Result<bool, ConfigError> {
    let slot = match key {
        "allow_stream_changes" => &mut permissions.stream_changes,
        "allow_config_read" => &mut permissions.config_read,
        "allow_config_write" => &mut permissions.config_write,
        "allow_restart" => &mut permissions.restart,
        "allow_exit" => &mut permissions.exit,
        _ => return Ok(false),
    };
    match value {
        Value::Bool(b) => *slot = *b,
        // An empty value keeps the default.
        Value::Null => {}
        _ => return Err(ConfigError::Invalid(format!("{path}: expected true or false"))),
    }
    Ok(true)
}
