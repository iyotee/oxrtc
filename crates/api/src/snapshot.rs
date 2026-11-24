//! `GET /api/frame.jpeg` (M5 F5): the stream's current picture as one JPEG,
//! made by the [`Snapshotter`] the binary gives (FFmpeg), shaped as the query
//! asks and served from a cache when it asks for one, as go2rtc reads it:
//!
//! - `width` (or `w`) and `height` (or `h`): the picture scaled, a side given
//!   alone keeping its shape; 1 to [`MAX_SNAPSHOT_WIDTH`] pixels, a larger
//!   whole number held to it, anything else (`0`, `-2`, `1.5`, `abc`) as if
//!   not given (go2rtc's `Atoi` reads it as 0, "not asked");
//! - `rotate`: `90`, `180`, `270` or `-90` degrees clockwise, after the scale;
//!   `0` none; anything else no turn, as go2rtc;
//! - `cache`: a Go duration (`10s`, `1m`, `1.5s`, `500ms`): a picture made
//!   for a request that asked for a cache is kept, and served again to a
//!   request asking for a cache no shorter than its age. go2rtc keeps one
//!   per stream and serves it to a request of another size; here it is kept
//!   per stream and shape, so a picture of another size is never served for
//!   a cache, and the cache is bounded ([`MAX_CACHED`], [`MAX_CACHED_BYTES`]).
//!   A value that is no duration (`abc`, `10` without its unit) is no cache,
//!   as go2rtc's `time.ParseDuration` failing is;
//! - `hardware` (or `hw`): not read (R12), the picture encoded in software.
//!
//! What cannot be read is answered as go2rtc 1.9.15 answers it (measured on
//! the binary, D46 amended) and named in a spaced warning ([`crate::unread`]).
//!
//! The answer carries go2rtc's headers: `Content-Type: image/jpeg`, its
//! `Content-Length`, `Cache-Control: no-cache`, `Pragma: no-cache`.

use std::collections::VecDeque;
use std::sync::{Mutex, PoisonError};
use std::time::Duration;

use axum::extract::State;
use axum::http::{HeaderMap, HeaderValue, StatusCode, Uri, header};
use axum::response::IntoResponse;
use bytes::Bytes;
use oxrtc_core::jpeg::{Rotate, Shape};
use tokio::time::Instant;

use crate::ApiState;
use crate::routes::{Fail, MAX_SNAPSHOT_WIDTH, Query, Reply, text};
use crate::unread::{Ignored, Route, whole};

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

/// The first of `long` and `short` the query names, with its name.
fn named<'q>(query: &'q Query, long: &'static str, short: &'static str) -> Option<(&'static str, &'q str)> {
    query.first(long).map(|v| (long, v)).or_else(|| query.first(short).map(|v| (short, v)))
}

fn side(query: &Query, long: &'static str, short: &'static str, ignored: &mut Vec<Ignored>) -> Option<u32> {
    let (key, value) = named(query, long, short)?;
    // A whole number as go2rtc's Atoi reads it, however long: held to the
    // largest side allowed; 0 and what it cannot read, not asked.
    let Some(side) = whole(value).filter(|side| *side > 0) else {
        ignored.push(Ignored::new(key, value, format!("ignored ({long} is 1 to {MAX_SNAPSHOT_WIDTH} pixels)")));
        return None;
    };
    let held = u32::try_from(side).unwrap_or(u32::MAX).min(MAX_SNAPSHOT_WIDTH);
    if u64::from(held) != side {
        ignored.push(Ignored::new(key, value, format!("read as {held} (the largest {long})")));
    }
    Some(held)
}

/// Reads the query's shape and cache, with what it set aside: what cannot be
/// read is answered as go2rtc answers it, never refused.
pub(crate) fn asked(query: &Query) -> (Asked, Vec<Ignored>) {
    let mut ignored = Vec::new();
    let rotate = match query.first("rotate") {
        None | Some("0") => Rotate::None,
        Some("90") => Rotate::Quarter,
        Some("180") => Rotate::Half,
        Some("270" | "-90") => Rotate::ThreeQuarters,
        Some(value) => {
            ignored.push(Ignored::new("rotate", value, "ignored (0, 90, 180, 270 or -90)"));
            Rotate::None
        }
    };
    let width = side(query, "width", "w", &mut ignored);
    let height = side(query, "height", "h", &mut ignored);
    let cache = query.first("cache").and_then(|value| {
        let age = go_duration(value);
        if age.is_none() {
            ignored.push(Ignored::new("cache", value, "ignored, no cache (a duration: 10s, 1m, 500ms)"));
        }
        age
    });
    if let Some((key, value)) = named(query, "hardware", "hw") {
        ignored.push(Ignored::new(key, value, "not read (R12): encoded in software"));
    }
    (Asked { shape: Shape { width, height, rotate }, cache }, ignored)
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
pub(crate) async fn frame_jpeg(State(state): State<ApiState>, headers: HeaderMap, uri: Uri) -> Reply {
    let query = Query::of(&uri)?;
    // go2rtc's `GetOrPatch`: `src` may be a source (the WebRTC Camera card's poster).
    let stream = query.stream_or_patch(&state, &headers)?;
    let (asked, ignored) = asked(&query);
    let snapshotter =
        state.snapshotter.clone().ok_or_else(|| text(StatusCode::NOT_IMPLEMENTED, "snapshots are not enabled"))?;
    let name = stream.name().to_owned();
    state.unread.report(Route::FrameJpeg, &name, &ignored);
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
