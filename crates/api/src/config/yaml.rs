//! Walks the YAML tree into a [`Config`], warning about unknown keys.
//!
//! Errors name the key, never its value: a value may be a source with a
//! camera password in it.

use std::path::PathBuf;

use serde_norway::{Mapping, Value};

use super::{
    ApiConfig, Config, ConfigError, FfmpegSection, IceServer, LogSection, MAX_MIB, MAX_RTSP_CREDENTIAL_BYTES,
    MAX_SECONDS, MAX_SOUND_LAG_MS, QueueLimits, RtspSection, WebRtcSection,
};

/// Longest key name repeated in a warning.
const MAX_KEY_SHOWN: usize = 64;

struct Walker {
    warnings: Vec<String>,
}

fn invalid(path: &str, expected: &str) -> ConfigError {
    ConfigError::Invalid(format!("{path}: expected {expected}"))
}

fn shown(key: &str) -> String {
    key.chars().take(MAX_KEY_SHOWN).collect()
}

/// A mapping, or `None` for an empty (null) value.
fn mapping<'a>(value: &'a Value, path: &str) -> Result<Option<&'a Mapping>, ConfigError> {
    match value {
        Value::Null => Ok(None),
        Value::Mapping(m) => Ok(Some(m)),
        _ => Err(invalid(path, "a map")),
    }
}

/// A scalar as text, or `None` for an empty value.
fn scalar(value: &Value, path: &str) -> Result<Option<String>, ConfigError> {
    match value {
        Value::Null => Ok(None),
        Value::String(s) => Ok(Some(s.clone())),
        Value::Number(n) => Ok(Some(n.to_string())),
        Value::Bool(b) => Ok(Some(b.to_string())),
        _ => Err(invalid(path, "a string")),
    }
}

/// A non-empty string, or `None` for an empty or blank value.
fn optional(value: &Value, path: &str) -> Result<Option<String>, ConfigError> {
    Ok(scalar(value, path)?.filter(|s| !s.is_empty()))
}

/// A count of at least 1, or `None` for an empty value (the default).
fn count(value: &Value, path: &str) -> Result<Option<usize>, ConfigError> {
    match value {
        Value::Null => Ok(None),
        Value::Number(n) => match n.as_u64().and_then(|n| usize::try_from(n).ok()) {
            Some(n) if n >= 1 => Ok(Some(n)),
            _ => Err(invalid(path, "a whole number of at least 1")),
        },
        _ => Err(invalid(path, "a whole number of at least 1")),
    }
}

/// A count from 1 to `max`, or `None` for an empty value (the default).
fn bounded(value: &Value, path: &str, max: usize) -> Result<Option<usize>, ConfigError> {
    let expected = || invalid(path, &format!("a whole number from 1 to {max}"));
    match value {
        Value::Null => Ok(None),
        Value::Number(n) => match n.as_u64().and_then(|n| usize::try_from(n).ok()) {
            Some(n) if (1..=max).contains(&n) => Ok(Some(n)),
            _ => Err(expected()),
        },
        _ => Err(expected()),
    }
}

/// The longest idle grace (`api.idle_grace_ms`, `api.transcode_idle_grace_ms`): an hour.
const MAX_GRACE_MS: u64 = 3_600_000;

/// An idle grace in milliseconds, from 0 (stopped with its last consumer) to
/// an hour, or `None` for an empty value (the default).
fn grace(value: &Value, path: &str) -> Result<Option<u64>, ConfigError> {
    let expected = || invalid(path, &format!("a whole number from 0 to {MAX_GRACE_MS}"));
    match value {
        Value::Null => Ok(None),
        Value::Number(n) => n.as_u64().filter(|ms| *ms <= MAX_GRACE_MS).map(Some).ok_or_else(expected),
        _ => Err(expected()),
    }
}

/// The longest `webrtc.max_sound_late_ms` takes: a minute.
const MAX_SOUND_LATE_MS: u64 = 60_000;

/// `webrtc.max_sound_late_ms`: milliseconds from 0 (no bound) to a minute,
/// or `None` for an empty value (the default).
fn sound_late(value: &Value) -> Result<Option<u64>, ConfigError> {
    let expected = || invalid("webrtc.max_sound_late_ms", &format!("a whole number from 0 to {MAX_SOUND_LATE_MS}"));
    match value {
        Value::Null => Ok(None),
        Value::Number(n) => n.as_u64().filter(|ms| *ms <= MAX_SOUND_LATE_MS).map(Some).ok_or_else(expected),
        _ => Err(expected()),
    }
}

/// `webrtc.sound_lag_ms`: milliseconds from 0 (the sound dated when it
/// arrived) to [`MAX_SOUND_LAG_MS`], or `None` for an empty value (the
/// default).
fn sound_lag(value: &Value) -> Result<Option<u64>, ConfigError> {
    let expected = || invalid("webrtc.sound_lag_ms", &format!("a whole number from 0 to {MAX_SOUND_LAG_MS}"));
    match value {
        Value::Null => Ok(None),
        Value::Number(n) => n.as_u64().filter(|ms| *ms <= MAX_SOUND_LAG_MS).map(Some).ok_or_else(expected),
        _ => Err(expected()),
    }
}

/// A consumer queue's key (`queue_max_mib`, `queue_max_seconds`), or
/// `Ok(false)` for a key that is not one.
fn queue_key(queue: &mut QueueLimits, key: &str, value: &Value, path: &str) -> Result<bool, ConfigError> {
    match key {
        "queue_max_mib" => queue.max_mib = bounded(value, path, MAX_MIB)?,
        "queue_max_seconds" => queue.max_seconds = bounded(value, path, MAX_SECONDS)?,
        _ => return Ok(false),
    }
    Ok(true)
}

/// A string or a list of strings (flow or block style).
fn string_list(value: &Value, path: &str) -> Result<Vec<String>, ConfigError> {
    match value {
        Value::Sequence(items) => {
            items.iter().enumerate().filter_map(|(i, item)| scalar(item, &format!("{path}[{i}]")).transpose()).collect()
        }
        other => Ok(scalar(other, path)?.into_iter().collect()),
    }
}

/// Both halves or none, and a user name Basic can carry. Values are never echoed.
fn rtsp_credential(rtsp: &RtspSection) -> Result<(), ConfigError> {
    let (user, password) = match (&rtsp.username, &rtsp.password) {
        (None, None) => return Ok(()),
        (Some(user), Some(password)) => (user, password),
        (Some(_), None) => return Err(ConfigError::Invalid("rtsp.username is set without rtsp.password".into())),
        (None, Some(_)) => return Err(ConfigError::Invalid("rtsp.password is set without rtsp.username".into())),
    };
    if user.contains(':') {
        return Err(ConfigError::Invalid("rtsp.username: a ':' cannot be in it (HTTP Basic cannot carry one)".into()));
    }
    if user.len() > MAX_RTSP_CREDENTIAL_BYTES || password.len() > MAX_RTSP_CREDENTIAL_BYTES {
        return Err(ConfigError::Invalid(format!(
            "rtsp.username and rtsp.password: at most {MAX_RTSP_CREDENTIAL_BYTES} bytes each"
        )));
    }
    Ok(())
}

impl Walker {
    fn warn_unknown(&mut self, path: &str, key: &str) {
        self.warnings.push(format!("unknown key {path}{} ignored", shown(key)));
    }

    /// The entries of a section with string keys; other keys are warned about.
    fn entries<'a>(&mut self, section: Option<&'a Mapping>, path: &str) -> Vec<(&'a str, &'a Value)> {
        let mut out = Vec::new();
        for (key, value) in section.into_iter().flatten() {
            match key.as_str() {
                Some(k) => out.push((k, value)),
                None => self.warnings.push(format!("non-string key in {path} ignored")),
            }
        }
        out
    }

    fn api(&mut self, value: &Value) -> Result<ApiConfig, ConfigError> {
        let mut api = ApiConfig::default();
        for (key, value) in self.entries(mapping(value, "api")?, "api") {
            let path = format!("api.{key}");
            match key {
                "listen" => api.listen = scalar(value, &path)?.unwrap_or(api.listen),
                "username" => api.username = optional(value, &path)?,
                "password" => api.password = optional(value, &path)?,
                "token" => api.token = optional(value, &path)?,
                "max_connections_per_ip" => api.max_connections_per_ip = count(value, &path)?,
                // The streams' limits (D16).
                "max_streams" => api.streams.max_streams = count(value, &path)?,
                "max_consumers_per_stream" => api.streams.max_consumers_per_stream = count(value, &path)?,
                "max_frame_mib" => api.streams.max_frame_mib = bounded(value, &path, MAX_MIB)?,
                "gop_cache_mib" => api.streams.gop_cache_mib = bounded(value, &path, MAX_MIB)?,
                "idle_grace_ms" => api.streams.idle_grace_ms = grace(value, &path)?,
                "transcode_idle_grace_ms" => api.streams.transcode_idle_grace_ms = grace(value, &path)?,
                "allow_unauthenticated" => {
                    api.allow_unauthenticated = match value {
                        Value::Bool(b) => *b,
                        Value::Null => false,
                        _ => return Err(invalid(&path, "true or false")),
                    }
                }
                // go2rtc's opt-in to authenticate loopback callers too, which
                // openIVR writes: oxrtc always does (D7). Turning it off is
                // not followed, and said.
                "local_auth" => match value {
                    Value::Bool(true) | Value::Null => {}
                    Value::Bool(false) => self.warnings.push(
                        "api.local_auth: false is not followed: oxrtc authenticates loopback callers too (D7)".into(),
                    ),
                    _ => return Err(invalid(&path, "true or false")),
                },
                other => self.warn_unknown("api.", other),
            }
        }
        Ok(api)
    }

    fn rtsp(&mut self, value: &Value) -> Result<RtspSection, ConfigError> {
        let mut rtsp = RtspSection::default();
        for (key, value) in self.entries(mapping(value, "rtsp")?, "rtsp") {
            match key {
                "listen" => rtsp.listen = scalar(value, "rtsp.listen")?.unwrap_or(rtsp.listen),
                "max_connections_per_ip" => {
                    rtsp.max_connections_per_ip = count(value, "rtsp.max_connections_per_ip")?;
                }
                "username" => rtsp.username = optional(value, "rtsp.username")?,
                "password" => rtsp.password = optional(value, "rtsp.password")?,
                // oxrtc only (D24): go2rtc takes a loopback reader unchecked.
                "local_auth" => {
                    rtsp.local_auth = match value {
                        Value::Bool(b) => *b,
                        Value::Null => true,
                        _ => return Err(invalid("rtsp.local_auth", "true or false")),
                    }
                }
                other if queue_key(&mut rtsp.queue, other, value, &format!("rtsp.{other}"))? => {}
                other => self.warn_unknown("rtsp.", other),
            }
        }
        rtsp_credential(&rtsp)?;
        Ok(rtsp)
    }

    fn webrtc(&mut self, value: &Value) -> Result<WebRtcSection, ConfigError> {
        let mut webrtc = WebRtcSection::default();
        for (key, value) in self.entries(mapping(value, "webrtc")?, "webrtc") {
            match key {
                "listen" => webrtc.listen = scalar(value, "webrtc.listen")?.unwrap_or(webrtc.listen),
                "candidates" => webrtc.candidates = string_list(value, "webrtc.candidates")?,
                "ice_servers" => webrtc.ice_servers = self.ice_servers(value)?,
                "max_sessions" => webrtc.max_sessions = count(value, "webrtc.max_sessions")?,
                "max_sessions_per_ip" => {
                    webrtc.max_sessions_per_ip = count(value, "webrtc.max_sessions_per_ip")?;
                }
                "lip_sync" => {
                    webrtc.lip_sync = match value {
                        Value::Bool(b) => *b,
                        Value::Null => true,
                        _ => return Err(invalid("webrtc.lip_sync", "true or false")),
                    }
                }
                "max_sound_late_ms" => webrtc.max_sound_late_ms = sound_late(value)?,
                "sound_lag_ms" => webrtc.sound_lag_ms = sound_lag(value)?,
                other if queue_key(&mut webrtc.queue, other, value, &format!("webrtc.{other}"))? => {}
                other => self.warn_unknown("webrtc.", other),
            }
        }
        Ok(webrtc)
    }

    fn ice_servers(&mut self, value: &Value) -> Result<Vec<IceServer>, ConfigError> {
        let items = match value {
            Value::Null => return Ok(Vec::new()),
            Value::Sequence(items) => items,
            _ => return Err(invalid("webrtc.ice_servers", "a list")),
        };
        let mut servers = Vec::with_capacity(items.len());
        for (i, item) in items.iter().enumerate() {
            let path = format!("webrtc.ice_servers[{i}]");
            let mut server = IceServer::default();
            for (key, value) in self.entries(mapping(item, &path)?, &path) {
                let field = format!("{path}.{key}");
                match key {
                    "urls" => server.urls = string_list(value, &field)?,
                    "username" => server.username = optional(value, &field)?,
                    "credential" => server.credential = optional(value, &field)?,
                    other => self.warn_unknown(&format!("{path}."), other),
                }
            }
            servers.push(server);
        }
        Ok(servers)
    }

    fn streams(&mut self, value: &Value) -> Result<Vec<(String, Vec<String>)>, ConfigError> {
        let mut streams = Vec::new();
        for (name, value) in self.entries(mapping(value, "streams")?, "streams") {
            let sources = string_list(value, &format!("streams.{}", shown(name)))?;
            if sources.is_empty() {
                self.warnings.push(format!("stream {} has no source", shown(name)));
            }
            streams.push((name.to_owned(), sources));
        }
        Ok(streams)
    }

    fn ffmpeg(&mut self, value: &Value) -> Result<FfmpegSection, ConfigError> {
        let mut ffmpeg = FfmpegSection::default();
        for (key, value) in self.entries(mapping(value, "ffmpeg")?, "ffmpeg") {
            let path = format!("ffmpeg.{}", shown(key));
            match key {
                "bin" => ffmpeg.bin = optional(value, &path)?.map(PathBuf::from),
                "max_processes" => ffmpeg.max_processes = count(value, &path)?,
                "max_memory_mib" => ffmpeg.max_memory_mib = count(value, &path)?,
                "native_audio" => {
                    ffmpeg.native_audio = match value {
                        Value::Bool(b) => Some(*b),
                        Value::Null => None,
                        _ => return Err(invalid("ffmpeg.native_audio", "true or false")),
                    }
                }
                template => {
                    let text = scalar(value, &path)?.unwrap_or_default();
                    ffmpeg.templates.insert(template.to_owned(), text);
                }
            }
        }
        Ok(ffmpeg)
    }

    fn log(&mut self, value: &Value) -> Result<LogSection, ConfigError> {
        let mut log = LogSection::default();
        for (key, value) in self.entries(mapping(value, "log")?, "log") {
            match key {
                "level" => log.level = scalar(value, "log.level")?.unwrap_or(log.level),
                other => self.warn_unknown("log.", other),
            }
        }
        Ok(log)
    }
}

pub(super) fn parse(text: &str) -> Result<(Config, Vec<String>), ConfigError> {
    let root: Value = if text.trim().is_empty() {
        Value::Null
    } else {
        serde_norway::from_str(text).map_err(|e| ConfigError::Yaml(e.to_string()))?
    };
    let mut walker = Walker { warnings: Vec::new() };
    let mut config = Config::default();
    for (key, value) in walker.entries(mapping(&root, "the document")?, "the document") {
        match key {
            "api" => config.api = walker.api(value)?,
            "rtsp" => config.rtsp = walker.rtsp(value)?,
            "webrtc" => config.webrtc = walker.webrtc(value)?,
            "streams" => config.streams = walker.streams(value)?,
            "ffmpeg" => config.ffmpeg = walker.ffmpeg(value)?,
            "log" => config.log = walker.log(value)?,
            other => walker.warn_unknown("", other),
        }
    }
    Ok((config, walker.warnings))
}
