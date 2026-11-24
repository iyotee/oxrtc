//! `GET`, `POST` and `PATCH /api/config`, as go2rtc names them (D48).
//!
//! The file the server was started with: read as written (credentials
//! included) only while `api.allow_config_read` or `allow_config_write`
//! holds; written only while `allow_config_write` holds, and then only a
//! text this server's own parser takes, that still authenticates the API and
//! that keeps the keys deciding what the API may do and what runs
//! (`api.allow_*`, `api.allow_unauthenticated`, `rtsp.local_auth`, the
//! whole `ffmpeg:` section) as they are in the file. It is written to a
//! temporary file beside it, with its permissions, then renamed over it. The
//! running server does not change: `POST /api/restart` reads it again.

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};

use axum::extract::State;
use axum::http::{StatusCode, header};
use axum::response::IntoResponse;
use bytes::Bytes;
use serde_norway::Value;

use crate::config::{self, Config, MAX_CONFIG_BYTES};
use crate::routes::{Fail, Reply, text};
use crate::streams::forbidden;
use crate::{ApiState, Auth};

/// What the API may not change: what decides what the API may do and what
/// runs.
fn protected(config: &Config) -> impl PartialEq + '_ {
    (config.api.permissions, config.api.allow_unauthenticated, config.rtsp.local_auth, &config.ffmpeg)
}

fn path_of(state: &ApiState) -> Result<PathBuf, Fail> {
    // go2rtc's answer when it runs without a file.
    state.facts.config_path.clone().ok_or_else(|| text(StatusCode::GONE, "no configuration file"))
}

fn read(path: &Path) -> Result<String, Fail> {
    let missing = |_| text(StatusCode::NOT_FOUND, "configuration file not found");
    let file = std::fs::File::open(path).map_err(missing)?;
    let mut bytes = Vec::new();
    std::io::Read::read_to_end(&mut std::io::Read::take(file, MAX_CONFIG_BYTES as u64 + 1), &mut bytes)
        .map_err(missing)?;
    if bytes.len() > MAX_CONFIG_BYTES {
        return Err(text(StatusCode::INTERNAL_SERVER_ERROR, "configuration file too large"));
    }
    String::from_utf8(bytes).map_err(|_| text(StatusCode::INTERNAL_SERVER_ERROR, "configuration file is not UTF-8"))
}

/// Runs `job` off the runtime's workers, under the file's lock.
async fn blocking<T: Send + 'static>(
    lock: Arc<Mutex<()>>,
    job: impl FnOnce() -> Result<T, Fail> + Send + 'static,
) -> Result<T, Fail> {
    tokio::task::spawn_blocking(move || {
        let _held = lock.lock().unwrap_or_else(PoisonError::into_inner);
        job()
    })
    .await
    .map_err(|_| text(StatusCode::INTERNAL_SERVER_ERROR, "configuration task failed"))?
}

/// `GET /api/config`: the file as written, `application/yaml`.
pub(crate) async fn get(State(state): State<ApiState>) -> Reply {
    if !state.permissions.reads_config() {
        return Err(forbidden("allow_config_read"));
    }
    let path = path_of(&state)?;
    let body = blocking(state.config_lock.clone(), move || read(&path)).await?;
    Ok(([(header::CONTENT_TYPE, "application/yaml")], body).into_response())
}

/// `POST /api/config`: the body becomes the file (its comments with it).
pub(crate) async fn post(State(state): State<ApiState>, body: Bytes) -> Reply {
    if !state.permissions.config_write {
        return Err(forbidden("allow_config_write"));
    }
    let path = path_of(&state)?;
    let new =
        String::from_utf8(body.to_vec()).map_err(|_| text(StatusCode::BAD_REQUEST, "configuration is not UTF-8"))?;
    blocking(state.config_lock.clone(), move || {
        let current = read(&path)?;
        checked_write(&path, &current, &new)
    })
    .await?;
    tracing::warn!("api: configuration file written (POST /api/config); it applies at the next restart");
    Ok(StatusCode::OK.into_response())
}

/// `PATCH /api/config`: the body's keys merged into the file's, go2rtc's
/// merge (maps merged key by key, anything else replaced); the file is
/// written again from the merged tree, so its comments are lost, as go2rtc
/// loses them.
pub(crate) async fn patch(State(state): State<ApiState>, body: Bytes) -> Reply {
    if !state.permissions.config_write {
        return Err(forbidden("allow_config_write"));
    }
    let path = path_of(&state)?;
    let change = std::str::from_utf8(&body).map_err(|_| text(StatusCode::BAD_REQUEST, "configuration is not UTF-8"))?;
    let change: Value =
        serde_norway::from_str(change).map_err(|e| text(StatusCode::BAD_REQUEST, format!("invalid YAML: {e}")))?;
    if !matches!(change, Value::Mapping(_)) {
        return Err(text(StatusCode::BAD_REQUEST, "a configuration change is a map"));
    }
    blocking(state.config_lock.clone(), move || {
        let current = read(&path)?;
        let mut tree: Value = serde_norway::from_str(&current)
            .map_err(|_| text(StatusCode::CONFLICT, "the file on disk is not valid YAML: fix it by hand"))?;
        if tree.is_null() {
            tree = Value::Mapping(serde_norway::Mapping::new());
        }
        merge(&mut tree, change);
        let merged = serde_norway::to_string(&tree)
            .map_err(|_| text(StatusCode::INTERNAL_SERVER_ERROR, "configuration cannot be written"))?;
        checked_write(&path, &current, &merged)
    })
    .await?;
    tracing::warn!("api: configuration file written (PATCH /api/config); it applies at the next restart");
    Ok(StatusCode::OK.into_response())
}

/// go2rtc's merge: maps key by key, anything else (lists included) replaced.
pub(crate) fn merge(into: &mut Value, change: Value) {
    match (into, change) {
        (Value::Mapping(into), Value::Mapping(change)) => {
            for (key, value) in change {
                match into.get_mut(&key) {
                    Some(existing) => merge(existing, value),
                    None => {
                        into.insert(key, value);
                    }
                }
            }
        }
        (into, change) => *into = change,
    }
}

/// Checks `new` against the file's `current` text, then writes it.
pub(crate) fn checked_write(path: &Path, current: &str, new: &str) -> Result<(), Fail> {
    if new.len() > MAX_CONFIG_BYTES {
        return Err(text(StatusCode::PAYLOAD_TOO_LARGE, "configuration too large"));
    }
    // An error names a key, never a value (config::yaml); YAML's own may
    // quote a line, which `text` redacts.
    let (config, _) = config::parse(new).map_err(|e| text(StatusCode::BAD_REQUEST, e.to_string()))?;
    Auth::from_config(&config.api).map_err(|e| text(StatusCode::BAD_REQUEST, e.to_string()))?;
    let (before, _) = config::parse(current)
        .map_err(|_| text(StatusCode::CONFLICT, "the file on disk does not parse: fix it by hand"))?;
    if protected(&before) != protected(&config) {
        return Err(text(
            StatusCode::FORBIDDEN,
            "the API cannot change api.allow_*, api.allow_unauthenticated, rtsp.local_auth or the ffmpeg section: \
             edit the file by hand",
        ));
    }
    replace(path, new)
        .map_err(|e| text(StatusCode::INTERNAL_SERVER_ERROR, format!("cannot write the configuration: {e}")))
}

/// Writes `text` beside `path`, with its permissions, then renames it over
/// `path`: a reader sees the old file or the new one, never half of one.
fn replace(path: &Path, text: &str) -> std::io::Result<()> {
    let dir = path.parent().filter(|d| !d.as_os_str().is_empty()).unwrap_or(Path::new("."));
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "config".to_owned());
    let temporary = dir.join(format!(".{name}.{}.tmp", std::process::id()));
    let written = (|| {
        let mut file = std::fs::File::create(&temporary)?;
        if let Ok(metadata) = std::fs::metadata(path) {
            file.set_permissions(metadata.permissions())?;
        }
        file.write_all(text.as_bytes())?;
        file.sync_all()?;
        drop(file);
        std::fs::rename(&temporary, path)
    })();
    if written.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    written
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn go2rtcs_merge() {
        let mut tree: Value = serde_norway::from_str("a: {b: 1, c: [1, 2]}\nd: x\n").unwrap_or(Value::Null);
        let change: Value = serde_norway::from_str("a: {c: [3], e: 2}\nd: {f: 1}\n").unwrap_or(Value::Null);
        merge(&mut tree, change);
        let expected: Value = serde_norway::from_str("a: {b: 1, c: [3], e: 2}\nd: {f: 1}\n").unwrap_or(Value::Null);
        assert_eq!(tree, expected);
    }
}
