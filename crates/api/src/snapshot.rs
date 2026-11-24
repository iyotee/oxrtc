//! `GET /api/frame.jpeg` (M5 F5): the stream's current picture as one JPEG,
//! made by the [`Snapshotter`] the binary gives (FFmpeg), shaped as the query
//! asks and served from a cache when it asks for one, as go2rtc reads it:
//!
//! - `width` (or `w`) and `height` (or `h`): the picture scaled, a side given
//!   alone keeping its shape; 1 to [`MAX_SNAPSHOT_WIDTH`] pixels, anything
//!   else `400` (go2rtc ignores what it cannot read);
//! - `rotate`: `90`, `180`, `270` or `-90` degrees clockwise, after the scale;
//!   `0` none; anything else `400` (go2rtc ignores it);
//! - `cache`: a Go duration (`10s`, `1m`, `1.5s`, `500ms`): a picture made
//!   for a request that asked for a cache is kept, and served again to a
//!   request asking for a cache no shorter than its age. go2rtc keeps one
//!   per stream and serves it to a request of another size; here it is kept
//!   per stream and shape, so a picture of another size is never served for
//!   a cache, and the cache is bounded ([`MAX_CACHED`], [`MAX_CACHED_BYTES`]).
//!
//! The answer carries go2rtc's headers: `Content-Type: image/jpeg`, its
//! `Content-Length`, `Cache-Control: no-cache`, `Pragma: no-cache`.

use std::collections::VecDeque;
use std::sync::{Mutex, PoisonError};
use std::time::Duration;

use axum::extract::State;
use axum::http::{HeaderValue, StatusCode, Uri, header};
use axum::response::IntoResponse;
use bytes::Bytes;
use oxrtc_core::jpeg::{Rotate, Shape};
use tokio::time::Instant;

use crate::ApiState;
use crate::routes::{Fail, MAX_SNAPSHOT_WIDTH, Query, Reply, text};

/// Most pictures the cache keeps, every stream and shape together.
pub const MAX_CACHED: usize = 64;
/// Most bytes it keeps.
pub const MAX_CACHED_BYTES: usize = 32 * 1024 * 1024;
/// The longest `cache` read: a day (go2rtc takes any duration).
pub const MAX_CACHE_AGE: Duration = Duration::from_secs(24 * 3600);

/// What a `frame.jpeg` query asks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Asked {
    /// The picture's shape.
    pub shape: Shape,
    /// How old a cached picture may be (`cache`), when one is asked for.
    pub cache: Option<Duration>,
}

fn side(query: &Query, long: &str, short: &str) -> Result<Option<u32>, String> {
    let Some(value) = query.first(long).or_else(|| query.first(short)) else { return Ok(None) };
    match value.parse::<u32>() {
        Ok(side) if (1..=MAX_SNAPSHOT_WIDTH).contains(&side) => Ok(Some(side)),
        _ => Err(format!("invalid {long}")),
    }
}

/// Reads the query's shape and cache.
pub(crate) fn asked(query: &Query) -> Result<Asked, String> {
    let rotate = match query.first("rotate") {
        None | Some("0") => Rotate::None,
        Some("90") => Rotate::Quarter,
        Some("180") => Rotate::Half,
        Some("270" | "-90") => Rotate::ThreeQuarters,
        Some(_) => return Err("invalid rotate (90, 180, 270 or -90)".to_owned()),
    };
    let shape = Shape { width: side(query, "width", "w")?, height: side(query, "height", "h")?, rotate };
    let cache = match query.first("cache") {
        None => None,
        Some(value) => Some(go_duration(value).ok_or_else(|| "invalid cache (a duration: 10s, 1m)".to_owned())?),
    };
    Ok(Asked { shape, cache })
}

/// Go's `time.ParseDuration`: a sign, then numbers with an optional
/// fraction, each with its unit (`ns`, `us`, `µs`, `ms`, `s`, `m`, `h`);
/// `0` alone. A negative one is no age at all; at most [`MAX_CACHE_AGE`].
#[must_use]
pub fn go_duration(text: &str) -> Option<Duration> {
    let (negative, mut rest) = match text.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, text.strip_prefix('+').unwrap_or(text)),
    };
    if rest == "0" {
        return Some(Duration::ZERO);
    }
    if rest.is_empty() || rest.len() > 64 {
        return None;
    }
    let mut seconds = 0f64;
    while !rest.is_empty() {
        let number_end = rest.find(|c: char| !(c.is_ascii_digit() || c == '.')).unwrap_or(rest.len());
        let (number, after) = rest.split_at(number_end);
        if number.is_empty() || number == "." || number.matches('.').count() > 1 {
            return None;
        }
        let value: f64 = number.parse().ok()?;
        let unit_end = after.find(|c: char| c.is_ascii_digit() || c == '.').unwrap_or(after.len());
        let (unit, next) = after.split_at(unit_end);
        let scale = match unit {
            "ns" => 1e-9,
            "us" | "µs" | "μs" => 1e-6,
            "ms" => 1e-3,
            "s" => 1.0,
            "m" => 60.0,
            "h" => 3600.0,
            _ => return None,
        };
        seconds += value * scale;
        rest = next;
    }
    if negative {
        return Some(Duration::ZERO);
    }
    Some(Duration::try_from_secs_f64(seconds).ok()?.min(MAX_CACHE_AGE))
}

struct Entry {
    stream: String,
    shape: Shape,
    made: Instant,
    jpeg: Bytes,
}

/// The pictures kept for `cache`, the newest last.
#[derive(Default)]
pub(crate) struct Stills {
    entries: Mutex<VecDeque<Entry>>,
}

impl Stills {
    /// The picture of `stream` in `shape` made no longer than `age` ago.
    pub(crate) fn get(&self, stream: &str, shape: Shape, age: Duration) -> Option<Bytes> {
        let entries = self.entries.lock().unwrap_or_else(PoisonError::into_inner);
        entries
            .iter()
            .rev()
            .find(|e| e.stream == stream && e.shape == shape)
            .filter(|e| e.made.elapsed() < age)
            .map(|e| e.jpeg.clone())
    }

    /// Keeps `jpeg`, in place of the one of the same stream and shape; the
    /// oldest go first past the bounds.
    pub(crate) fn put(&self, stream: &str, shape: Shape, jpeg: Bytes) {
        if jpeg.len() > MAX_CACHED_BYTES {
            return;
        }
        let mut entries = self.entries.lock().unwrap_or_else(PoisonError::into_inner);
        entries.retain(|e| !(e.stream == stream && e.shape == shape));
        entries.push_back(Entry { stream: stream.to_owned(), shape, made: Instant::now(), jpeg });
        let mut bytes: usize = entries.iter().map(|e| e.jpeg.len()).sum();
        while entries.len() > MAX_CACHED || bytes > MAX_CACHED_BYTES {
            let Some(oldest) = entries.pop_front() else { break };
            bytes -= oldest.jpeg.len();
        }
    }
}

fn answer(jpeg: Bytes) -> axum::response::Response {
    (
        [
            (header::CONTENT_TYPE, HeaderValue::from_static("image/jpeg")),
            (header::CACHE_CONTROL, HeaderValue::from_static("no-cache")),
            (header::PRAGMA, HeaderValue::from_static("no-cache")),
        ],
        jpeg,
    )
        .into_response()
}

/// `GET /api/frame.jpeg`.
pub(crate) async fn frame_jpeg(State(state): State<ApiState>, uri: Uri) -> Reply {
    let query = Query::of(&uri)?;
    // go2rtc's `GetOrPatch`: `src` may be a source (the WebRTC Camera card's poster).
    let stream = query.stream_or_patch(&state)?;
    let asked = asked(&query).map_err(|e| text(StatusCode::BAD_REQUEST, e))?;
    let snapshotter =
        state.snapshotter.clone().ok_or_else(|| text(StatusCode::NOT_IMPLEMENTED, "snapshots are not enabled"))?;
    let name = stream.name().to_owned();
    if let Some(jpeg) = asked.cache.and_then(|age| state.stills.get(&name, asked.shape, age)) {
        return Ok(answer(jpeg));
    }
    match snapshotter.jpeg(stream, asked.shape).await {
        Ok(jpeg) => {
            if asked.cache.is_some() {
                state.stills.put(&name, asked.shape, jpeg.clone());
            }
            Ok(answer(jpeg))
        }
        Err(error) => {
            let fail: Fail = text(StatusCode::INTERNAL_SERVER_ERROR, error.to_string());
            tracing::warn!(stream = %name, "api: snapshot failed: {}", fail.message());
            Err(fail)
        }
    }
}

#[cfg(test)]
#[path = "snapshot_tests.rs"]
mod tests;
