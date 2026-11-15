//! Walks the YAML tree into a [`Config`], warning about unknown keys.
//!
//! Errors name the key, never its value: a value may be a source with a
//! camera password in it.

use std::path::PathBuf;

use serde_norway::{Mapping, Value};

use super::{ApiConfig, Config, ConfigError, FfmpegSection, IceServer, LogSection, RtspSection, WebRtcSection};

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

/// A string or a list of strings (flow or block style).
fn string_list(value: &Value, path: &str) -> Result<Vec<String>, ConfigError> {
    match value {
        Value::Sequence(items) => {
            items.iter().enumerate().filter_map(|(i, item)| scalar(item, &format!("{path}[{i}]")).transpose()).collect()
        }
        other => Ok(scalar(other, path)?.into_iter().collect()),
    }
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
                // A file that protects the RTSP server must not be served open
                // with a warning nobody reads: refused until RTSP
                // authentication exists (ROADMAP R28).
                "username" | "password" if optional(value, &format!("rtsp.{key}"))?.is_some() => {
                    return Err(ConfigError::Invalid(format!(
                        "rtsp.{key}: RTSP authentication is not implemented, and oxrtc will not serve open a \
                         server this file protects (remove rtsp.username and rtsp.password, or keep go2rtc)"
                    )));
                }
                "username" | "password" => {}
                other => self.warn_unknown("rtsp.", other),
            }
        }
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
