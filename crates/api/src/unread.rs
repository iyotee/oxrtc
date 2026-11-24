//! Query values this server cannot read, answered as go2rtc 1.9.15 answers
//! them (M5 D2, D46 amended): the request is served as if the value were not
//! there, or held to the nearest value it allows, and the log says which
//! parameter and value were set aside.
//!
//! go2rtc reads `frame.jpeg`'s `width`, `height` and `stream.mp4`'s
//! `duration` with `strconv.Atoi` (what is not a whole number is 0, "not
//! asked"), `rotate` against a list (anything else no turn) and `cache` with
//! `time.ParseDuration` (what fails no cache), and says nothing. A client
//! written against go2rtc that sends such a value gets its picture from both
//! servers; here the operator also learns why the picture is not the one asked.
//! The warning is spaced as the other warnings a client can cause at will (one
//! per [`REPORT_EVERY`] per route, with how many requests it stands for): a
//! dashboard polling `frame.jpeg?rotate=45` every second is not a line a second.

use std::fmt;
use std::time::Duration;

use oxrtc_core::refusals::Refusals;

use crate::routes::Query;

/// Shortest time between two warnings of one route.
pub const REPORT_EVERY: Duration = Duration::from_secs(10);

/// Longest value a warning quotes, in characters.
const MAX_QUOTED: usize = 64;

/// One value set aside: its parameter, the value as sent, and what was done
/// with it (`"ignored"`, `"read as 7680"`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Ignored {
    /// The parameter as the query named it (`w` or `width`).
    pub key: String,
    /// The value as sent.
    pub value: String,
    /// What the server did instead, and what it reads.
    pub instead: String,
}

/// A count as go2rtc's `core.Atoi` (Go's `strconv.Atoi`) reads it: digits,
/// with no sign or a `+` (`+160` is 160, as Go reads it), held to
/// [`u64::MAX`] when longer; `None` for what Go reads as 0 or below or cannot
/// read at all (`-2`, `1.5`, `abc`, `+`, ` 5`), which every caller here
/// takes as "not asked". The one reading of a whole number of this module's
/// routes: `frame.jpeg`'s sides and `stream.mp4`'s `duration`.
pub(crate) fn whole(value: &str) -> Option<u64> {
    let digits = value.strip_prefix('+').unwrap_or(value);
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    Some(digits.parse::<u64>().unwrap_or(u64::MAX))
}

/// `stream.mp4`'s `rotate` and `scale`, which go2rtc 1.9.15 reads
/// (`internal/mp4/mp4.go`: the init segment's matrix turned by 90, 180 or 270
/// degrees, its `pasp` set to `<x>:<y>`) and this server does not do yet
/// (F4, ROADMAP): the file is the picture as the source sends it, and the
/// operator is told why it is not turned. `rotate=0` turns nothing on either.
pub(crate) fn mp4_not_done(query: &Query) -> Vec<Ignored> {
    let mut ignored = Vec::new();
    if let Some(value) = query.first("rotate").filter(|v| whole(v) != Some(0)) {
        ignored.push(Ignored::new("rotate", value, "not read (F4): the picture as the source sends it"));
    }
    if let Some(value) = query.first("scale") {
        ignored.push(Ignored::new("scale", value, "not read (F4): the picture's own aspect"));
    }
    ignored
}

impl Ignored {
    pub(crate) fn new(key: &str, value: &str, instead: impl Into<String>) -> Self {
        Self { key: key.to_owned(), value: value.to_owned(), instead: instead.into() }
    }
}

impl fmt::Display for Ignored {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Quoted with escapes: a value is the client's text and must not
        // write a line of its own into the log.
        let value: String = self.value.chars().take(MAX_QUOTED).collect();
        let cut = if self.value.chars().count() > MAX_QUOTED { "..." } else { "" };
        write!(f, "{}={:?}{cut} {}", self.key, value, self.instead)
    }
}

/// The spacing of the warnings, one per route. Cloning shares it.
#[derive(Clone)]
pub(crate) struct Unread {
    frame_jpeg: Refusals,
    stream_mp4: Refusals,
}

/// A route whose query values may be set aside.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Route {
    FrameJpeg,
    StreamMp4,
}

impl Route {
    fn name(self) -> &'static str {
        match self {
            Self::FrameJpeg => "frame.jpeg",
            Self::StreamMp4 => "stream.mp4",
        }
    }
}

impl Unread {
    pub(crate) fn new() -> Self {
        Self { frame_jpeg: Refusals::every(REPORT_EVERY), stream_mp4: Refusals::every(REPORT_EVERY) }
    }

    /// Counts a request of `route` on `stream` that set `ignored` aside, and
    /// warns when the route's interval allows (nothing when `ignored` is empty).
    pub(crate) fn report(&self, route: Route, stream: &str, ignored: &[Ignored]) {
        if ignored.is_empty() {
            return;
        }
        let spacing = match route {
            Route::FrameJpeg => &self.frame_jpeg,
            Route::StreamMp4 => &self.stream_mp4,
        };
        if let Some(requests) = spacing.refused() {
            let list: Vec<String> = ignored.iter().map(ToString::to_string).collect();
            tracing::warn!(
                stream = %stream,
                requests,
                "api: {}: a value that cannot be read, answered as go2rtc answers it: {}",
                route.name(),
                list.join(", ")
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_whole_number_is_read_as_gos_atoi_reads_it() {
        assert_eq!(whole("160"), Some(160));
        assert_eq!(whole("0160"), Some(160));
        assert_eq!(whole("+160"), Some(160), "Go's Atoi takes a plus sign");
        assert_eq!(whole("0"), Some(0));
        assert_eq!(whole("99999999999999999999999"), Some(u64::MAX), "held");
        for not in ["", "+", "++1", "-2", "-0", "1.5", "abc", " 5", "5 ", "1_000", "١"] {
            assert_eq!(whole(not), None, "{not:?}");
        }
    }

    #[test]
    fn stream_mp4s_rotate_and_scale_are_named_as_not_done() {
        let query = |text: &str| Query::of(&format!("/api/stream.mp4?{text}").parse().unwrap()).unwrap();
        let keys = |text: &str| -> Vec<String> { mp4_not_done(&query(text)).into_iter().map(|i| i.key).collect() };
        assert_eq!(keys("src=x&rotate=90&scale=4:3"), ["rotate", "scale"]);
        assert_eq!(keys("src=x&rotate=abc"), ["rotate"]);
        assert!(keys("src=x").is_empty());
        assert!(keys("src=x&rotate=0&rotate=90").is_empty(), "go2rtc reads the first: 0, no turn");
        assert!(keys("src=x&rotate=&scale=").is_empty(), "empty is none, as go2rtc's Get");
    }

    #[test]
    fn a_value_is_quoted_escaped_and_cut() {
        let line = Ignored::new("rotate", "45", "ignored (90, 180, 270 or -90)").to_string();
        assert_eq!(line, r#"rotate="45" ignored (90, 180, 270 or -90)"#);
        let forged = Ignored::new("cache", "1\nWARN forged", "ignored").to_string();
        assert!(!forged.contains('\n'), "{forged}");
        let long = Ignored::new("w", &"9".repeat(500), "ignored").to_string();
        assert!(long.len() < 100 && long.contains("..."), "{long}");
    }
}
