//! The log lines `GET /api/log` answers: the newest ones, in go2rtc's JSON
//! lines, within a byte budget.

use std::collections::VecDeque;
use std::sync::{Mutex, MutexGuard, PoisonError};

/// The budget go2rtc keeps (16 chunks of 64 KiB).
pub const LOG_RING_BYTES: usize = 1024 * 1024;

/// One event, as the binary's logger hands it over.
#[derive(Debug, Clone, Copy)]
pub struct LogEvent<'a> {
    /// go2rtc's level name: `trace`, `debug`, `info`, `warn`, `error`.
    pub level: &'a str,
    /// Milliseconds since the Unix epoch.
    pub time_ms: u64,
    /// The module (`api`, `rtsp`, `webrtc`, `ffmpeg`, `streams`...).
    pub module: &'a str,
    /// The message.
    pub message: &'a str,
    /// The other fields, in order.
    pub fields: &'a [(&'a str, String)],
}

/// The newest lines, oldest first, at most [`LogRing::max_bytes`] of them
/// (each line's newline counted). A line larger than the whole budget is
/// dropped.
#[derive(Debug)]
pub struct LogRing {
    lines: Mutex<(VecDeque<Box<str>>, usize)>,
    max_bytes: usize,
}

impl Default for LogRing {
    fn default() -> Self {
        Self::new(LOG_RING_BYTES)
    }
}

impl LogRing {
    /// A ring of at most `max_bytes`.
    pub fn new(max_bytes: usize) -> Self {
        Self { lines: Mutex::new((VecDeque::new(), 0)), max_bytes }
    }

    fn lock(&self) -> MutexGuard<'_, (VecDeque<Box<str>>, usize)> {
        self.lines.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Its budget, in bytes.
    pub fn max_bytes(&self) -> usize {
        self.max_bytes
    }

    /// Adds one line (without its newline), dropping the oldest to fit.
    pub fn push(&self, line: &str) {
        let size = line.len() + 1;
        if size > self.max_bytes {
            return;
        }
        let mut guard = self.lock();
        let (lines, held) = &mut *guard;
        while *held + size > self.max_bytes {
            match lines.pop_front() {
                Some(old) => *held -= old.len() + 1,
                None => break,
            }
        }
        lines.push_back(line.into());
        *held += size;
    }

    /// Adds one event as go2rtc's JSON line: `{"level", "time", "module",
    /// <fields>, "message"}` (go2rtc's `log.html` shows `time`, `level` and
    /// `message`, then the other keys as `key=value`), every text redacted
    /// (no line here ever carries a camera's credential: a field holding a
    /// URL is redacted again).
    pub fn push_event(&self, event: &LogEvent<'_>) {
        let mut out = serde_json::Map::new();
        out.insert("level".into(), event.level.into());
        out.insert("time".into(), event.time_ms.into());
        out.insert("module".into(), oxrtc_core::spec::redact(event.module).into());
        for (name, value) in event.fields {
            if !matches!(*name, "level" | "time" | "module" | "message") {
                out.insert((*name).to_owned(), oxrtc_core::spec::redact(value).into());
            }
        }
        out.insert("message".into(), oxrtc_core::spec::redact(event.message).into());
        if let Ok(line) = serde_json::to_string(&out) {
            self.push(&line);
        }
    }

    /// Every line held, oldest first, each ended by a newline.
    pub fn text(&self) -> String {
        let guard = self.lock();
        let mut out = String::with_capacity(guard.1);
        for line in &guard.0 {
            out.push_str(line);
            out.push('\n');
        }
        out
    }

    /// Bytes held, newlines included.
    pub fn len(&self) -> usize {
        self.lock().1
    }

    /// Whether it holds nothing.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Forgets every line (`DELETE /api/log`).
    pub fn clear(&self) {
        let mut guard = self.lock();
        guard.0.clear();
        guard.1 = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_event_is_go2rtcs_json_line_redacted() {
        let ring = LogRing::default();
        let fields =
            [("stream", "cam".to_owned()), ("url", "rtsp://admin:pw@cam/x".to_owned()), ("message", "x".to_owned())];
        let event = LogEvent {
            level: "warn",
            time_ms: 1_795_508_766_644,
            module: "streams",
            message: "rtsp://a:b@c/d stopped",
            fields: &fields,
        };
        ring.push_event(&event);
        let text = ring.text();
        let line: serde_json::Value = serde_json::from_str(text.trim_end()).unwrap_or_default();
        assert_eq!(line["level"], "warn");
        assert_eq!(line["time"], 1_795_508_766_644u64);
        assert_eq!(line["module"], "streams");
        assert_eq!(line["stream"], "cam");
        assert_eq!(line["url"], "rtsp://***@cam/x");
        assert_eq!(line["message"], "rtsp://***@c/d stopped", "a field named message does not replace it");
        assert!(!text.contains("pw") && text.ends_with('\n') && text.matches('\n').count() == 1, "{text}");
    }

    #[test]
    fn the_newest_lines_within_the_budget() {
        let ring = LogRing::new(10);
        ring.push("aaa");
        ring.push("bbb");
        assert_eq!(ring.text(), "aaa\nbbb\n");
        ring.push("cc");
        assert_eq!(ring.text(), "bbb\ncc\n", "the oldest goes");
        assert_eq!(ring.len(), 7);
        ring.push("0123456789");
        assert_eq!(ring.text(), "bbb\ncc\n", "a line over the budget is dropped");
        ring.clear();
        assert!(ring.is_empty() && ring.text().is_empty());
        for n in 0..10_000 {
            ring.push(&n.to_string());
            assert!(ring.len() <= ring.max_bytes());
        }
        assert_eq!(ring.text(), "9998\n9999\n");
    }
}
