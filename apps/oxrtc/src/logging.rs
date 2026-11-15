//! Logs on stdout in go2rtc's shape, which openIVR parses (the level is the
//! second word): `HH:MM:SS.mmm LVL [module] message key=value`.
//!
//! The `log` crate (retina's) is bridged into `tracing`, and retina's warnings
//! are demoted to debug: with cheap cameras they are routine ("unexpected RTSP
//! message", "late packet") and would drown the operator's log.

use std::fmt::{self, Write as _};

use oxrtc_rtsp_client::RETINA_LOG_TARGET;
use tracing::field::{Field, Visit};
use tracing::{Event, Level, Subscriber};
use tracing_log::NormalizeEvent;
use tracing_subscriber::filter::{LevelFilter, Targets};
use tracing_subscriber::fmt::format::Writer;
use tracing_subscriber::fmt::{FmtContext, FormatEvent, FormatFields};
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::registry::LookupSpan;
use tracing_subscriber::{Registry, reload};

/// Longest message kept (bytes); the rest is cut.
const MAX_MESSAGE_BYTES: usize = 2048;
/// Most `key=value` fields written per line.
const MAX_FIELDS: usize = 16;
/// Longest field value kept (bytes).
const MAX_VALUE_BYTES: usize = 256;
/// Longest `[module]` kept (bytes).
const MAX_MODULE_BYTES: usize = 64;
/// Dependencies whose debug output is a flood: capped at `info`.
const NOISY: &[&str] = &["str0m", "dimpl", "hyper", "hyper_util", "h2", "axum", "tower", "tower_http", "mio"];

/// The modules a line of the binary may name.
const MODULES: &[&str] = &["api", "rtsp", "webrtc", "ffmpeg", "streams"];

/// go2rtc's three-letter level.
pub fn level_word(level: Level) -> &'static str {
    match level {
        Level::TRACE => "TRC",
        Level::DEBUG => "DBG",
        Level::INFO => "INF",
        Level::WARN => "WRN",
        Level::ERROR => "ERR",
    }
}

/// `log.level` (go2rtc's names): `trace`, `debug`, `info`, `warn`, `error`,
/// and `disabled`/`off`.
pub fn parse_level(text: &str) -> Option<LevelFilter> {
    Some(match text.trim().to_ascii_lowercase().as_str() {
        "trace" => LevelFilter::TRACE,
        "debug" => LevelFilter::DEBUG,
        "info" => LevelFilter::INFO,
        "warn" | "warning" => LevelFilter::WARN,
        "error" => LevelFilter::ERROR,
        "disabled" | "off" | "none" => LevelFilter::OFF,
        _ => return None,
    })
}

/// The `[module]` of a target, in go2rtc's vocabulary.
pub fn module(target: &str) -> &str {
    let krate = target.split("::").next().unwrap_or(target);
    match krate {
        "oxrtc_api" | "tower_http" | "hyper" | "hyper_util" | "axum" => "api",
        "oxrtc_rtsp_server" | "oxrtc_rtsp_client" => "rtsp",
        k if k == RETINA_LOG_TARGET => "rtsp",
        "oxrtc_webrtc" | "str0m" | "dimpl" => "webrtc",
        "oxrtc_ffmpeg" => "ffmpeg",
        "oxrtc_core" => "streams",
        "oxrtc" => "app",
        other => other,
    }
}

/// Appends `text` to `out` on one line (line breaks and other control
/// characters become spaces: one event is one line, and no source can forge
/// one), at most `limit` bytes.
fn push_line(out: &mut String, text: &str, limit: usize) {
    let mut used = 0;
    for c in text.chars() {
        used += c.len_utf8();
        if used > limit {
            out.push('~');
            return;
        }
        out.push(if c.is_control() { ' ' } else { c });
    }
}

/// A field value as go2rtc's console writer shows it: bare, or quoted when it
/// holds spaces, quotes or `=`, or is empty.
fn push_value(out: &mut String, value: &str) {
    let mut clean = String::new();
    push_line(&mut clean, value, MAX_VALUE_BYTES);
    if clean.is_empty() || clean.contains([' ', '"', '=']) {
        let _ = write!(out, "{clean:?}");
    } else {
        out.push_str(&clean);
    }
}

#[derive(Default)]
struct Fields {
    message: String,
    rest: Vec<(&'static str, String)>,
}

impl Visit for Fields {
    fn record_str(&mut self, field: &Field, value: &str) {
        self.keep(field, value.to_owned());
    }

    fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
        self.keep(field, format!("{value:?}"));
    }
}

impl Fields {
    fn keep(&mut self, field: &Field, value: String) {
        let name = field.name();
        if name == "message" {
            self.message = value;
        } else if !name.starts_with("log.") && self.rest.len() < MAX_FIELDS {
            self.rest.push((name, value));
        }
    }
}

/// One log line, without its line break.
pub fn line(time: &str, level: Level, target: &str, message: &str, fields: &[(&str, String)]) -> String {
    let mut module = module(target);
    // The binary's own lines name their module: "rtsp: listen" under [app]
    // reads "[rtsp] listen".
    if module == "app"
        && let Some(named) = MODULES.iter().find(|m| message.strip_prefix(**m).is_some_and(|r| r.starts_with(": ")))
    {
        module = named;
    }
    // "api: stream set" under [api] reads "[api] stream set".
    let message = message.strip_prefix(module).and_then(|m| m.strip_prefix(": ")).unwrap_or(message);
    let mut out = String::with_capacity(64 + message.len());
    let _ = write!(out, "{time} {} [", level_word(level));
    // A `log` target is any string a dependency chose: bounded, one line.
    push_line(&mut out, module, MAX_MODULE_BYTES);
    out.push_str("] ");
    push_line(&mut out, message, MAX_MESSAGE_BYTES);
    for (name, value) in fields {
        out.push(' ');
        push_line(&mut out, name, MAX_VALUE_BYTES);
        out.push('=');
        push_value(&mut out, value);
    }
    out
}

/// The event formatter.
#[derive(Debug, Clone, Copy, Default)]
pub struct Go2rtcFormat;

impl<S, N> FormatEvent<S, N> for Go2rtcFormat
where
    S: Subscriber + for<'a> LookupSpan<'a>,
    N: for<'a> FormatFields<'a> + 'static,
{
    fn format_event(&self, _ctx: &FmtContext<'_, S, N>, mut writer: Writer<'_>, event: &Event<'_>) -> fmt::Result {
        let normalized = event.normalized_metadata();
        let metadata = normalized.as_ref().unwrap_or_else(|| event.metadata());
        let mut fields = Fields::default();
        event.record(&mut fields);
        let time = chrono::Local::now().format("%H:%M:%S%.3f").to_string();
        writeln!(writer, "{}", line(&time, *metadata.level(), metadata.target(), &fields.message, &fields.rest))
    }
}

/// The filter for `level`: everything at it, the noisy dependencies at most
/// at `info`.
fn targets(level: LevelFilter) -> Targets {
    let mut targets = Targets::new().with_default(level);
    for noisy in NOISY {
        targets = targets.with_target(*noisy, level.min(LevelFilter::INFO));
    }
    targets
}

/// The `log` crate's bridge into `tracing`, demoting retina's warnings.
#[derive(Debug, Clone, Copy, Default)]
pub struct Bridge;

/// The level a `log` record is shown at.
pub fn bridged_level(target: &str, level: log::Level) -> log::Level {
    let retina = target == RETINA_LOG_TARGET || target.starts_with("retina::");
    if retina && level == log::Level::Warn { log::Level::Debug } else { level }
}

impl log::Log for Bridge {
    fn enabled(&self, _metadata: &log::Metadata<'_>) -> bool {
        // The tracing filter decides; `log::set_max_level` skips the rest early.
        true
    }

    fn log(&self, record: &log::Record<'_>) {
        let level = bridged_level(record.target(), record.level());
        let demoted = log::Record::builder()
            .args(*record.args())
            .level(level)
            .target(record.target())
            .module_path(record.module_path())
            .file(record.file())
            .line(record.line())
            .build();
        let _ = tracing_log::format_trace(&demoted);
    }

    fn flush(&self) {}
}

fn log_max(level: LevelFilter) -> log::LevelFilter {
    match level.into_level() {
        None => log::LevelFilter::Off,
        Some(Level::ERROR) => log::LevelFilter::Error,
        // A retina warning shown at debug must still reach the bridge.
        Some(Level::WARN) => log::LevelFilter::Warn,
        Some(Level::INFO) => log::LevelFilter::Info,
        Some(Level::DEBUG) => log::LevelFilter::Debug,
        Some(Level::TRACE) => log::LevelFilter::Trace,
    }
}

/// Changes the level of the installed logger.
pub struct LogHandle {
    reload: reload::Handle<Targets, Registry>,
    /// Writes out the queued lines when the handle is dropped, at exit.
    _flush: crate::log_writer::LogFlush,
}

impl std::fmt::Debug for LogHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("LogHandle")
    }
}

impl LogHandle {
    /// Applies `log.level`. An unknown name keeps `info` and says so.
    pub fn set_level(&self, text: &str) {
        let level = parse_level(text);
        let applied = level.unwrap_or(LevelFilter::INFO);
        if let Err(error) = self.reload.reload(targets(applied)) {
            tracing::warn!("cannot change the log level: {error}");
        }
        log::set_max_level(log_max(applied));
        if level.is_none() {
            tracing::warn!("log.level {:?} is unknown; using info", text.chars().take(32).collect::<String>());
        }
    }
}

/// Installs the logger at `info` (until [`LogHandle::set_level`]). Fails if
/// a logger is already installed.
pub fn init() -> Result<LogHandle, String> {
    let (filter, reload) = reload::Layer::new(targets(LevelFilter::INFO));
    // Never on stdout directly: a slow reader of the pipe would block the
    // runtime's workers (log_writer.rs).
    let (writer, flush) = crate::log_writer::spawn(Box::new(std::io::stdout()), crate::log_writer::QUEUE_LINES)
        .map_err(|e| format!("cannot start the log writer: {e}"))?;
    let format = tracing_subscriber::fmt::layer().event_format(Go2rtcFormat).with_writer(writer);
    let subscriber = tracing_subscriber::registry().with(filter).with(format);
    tracing::subscriber::set_global_default(subscriber).map_err(|e| e.to_string())?;
    log::set_boxed_logger(Box::new(Bridge)).map_err(|e| e.to_string())?;
    log::set_max_level(log_max(LevelFilter::INFO));
    Ok(LogHandle { reload, _flush: flush })
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use log::Log as _;
    use tracing_subscriber::fmt::MakeWriter;

    use super::*;

    #[derive(Clone, Default)]
    struct Buffer(Arc<Mutex<Vec<u8>>>);

    impl std::io::Write for Buffer {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl<'a> MakeWriter<'a> for Buffer {
        type Writer = Buffer;
        fn make_writer(&'a self) -> Self::Writer {
            self.clone()
        }
    }

    /// Runs `f` under the real formatter and filter at `level`; returns the lines.
    fn capture(level: LevelFilter, f: impl FnOnce()) -> Vec<String> {
        let buffer = Buffer::default();
        let format = tracing_subscriber::fmt::layer().event_format(Go2rtcFormat).with_writer(buffer.clone());
        let subscriber = tracing_subscriber::registry().with(targets(level)).with(format);
        tracing::subscriber::with_default(subscriber, f);
        let text = String::from_utf8(buffer.0.lock().unwrap().clone()).unwrap();
        text.lines().map(str::to_owned).collect()
    }

    fn assert_shape(line: &str, word: &str, module: &str) {
        let mut parts = line.splitn(4, ' ');
        let time = parts.next().unwrap();
        assert_eq!(time.len(), 12, "{line}");
        assert!(time.chars().enumerate().all(|(i, c)| match i {
            2 | 5 => c == ':',
            8 => c == '.',
            _ => c.is_ascii_digit(),
        }));
        assert_eq!(parts.next(), Some(word), "{line}");
        assert_eq!(parts.next(), Some(format!("[{module}]").as_str()), "{line}");
    }

    #[test]
    fn lines_have_go2rtc_shape() {
        let lines = capture(LevelFilter::TRACE, || {
            tracing::info!(target: "oxrtc_api::routes", stream = "cam", sources = 2, "api: stream set");
            tracing::warn!(target: "oxrtc_core::slot", error = "a b", "source stopped, retrying");
            tracing::error!(target: "oxrtc", "bye");
            tracing::info!(target: "oxrtc::app", addr = "127.0.0.1:1", "rtsp: listen");
            tracing::debug!(target: "oxrtc_webrtc::server", "d");
            tracing::trace!(target: "elsewhere::x", empty = "", "t");
        });
        assert_eq!(lines.len(), 6, "{lines:?}");
        assert_shape(&lines[0], "INF", "api");
        assert!(lines[0].ends_with("[api] stream set stream=cam sources=2"), "{}", lines[0]);
        assert_shape(&lines[1], "WRN", "streams");
        assert!(lines[1].ends_with("source stopped, retrying error=\"a b\""), "{}", lines[1]);
        assert_shape(&lines[2], "ERR", "app");
        assert_shape(&lines[3], "INF", "rtsp");
        assert!(lines[3].ends_with("[rtsp] listen addr=127.0.0.1:1"), "{}", lines[3]);
        assert_shape(&lines[4], "DBG", "webrtc");
        assert_shape(&lines[5], "TRC", "elsewhere");
        assert!(lines[5].ends_with("t empty=\"\""));
    }

    #[test]
    fn one_event_is_one_bounded_line() {
        let long = "x".repeat(10 * MAX_MESSAGE_BYTES);
        let lines = capture(LevelFilter::INFO, || {
            tracing::info!(target: "oxrtc", "two\nlines\r\x1b[31m");
            tracing::info!(target: "oxrtc", value = %long, "{long}");
        });
        assert_eq!(lines.len(), 2, "{lines:?}");
        assert!(lines[0].ends_with("two lines  [31m"), "{}", lines[0]);
        assert!(lines[1].len() < MAX_MESSAGE_BYTES + MAX_VALUE_BYTES + 64, "{}", lines[1].len());
        // A `log` target is free text: it cannot break the line either.
        let forged = line("00:00:00.000", Level::INFO, "evil\n00:00:00.000 ERR [api]", "m", &[]);
        assert!(!forged.contains('\n'), "{forged}");
        let long_target = "t".repeat(10 * MAX_MODULE_BYTES);
        assert!(line("00:00:00.000", Level::INFO, &long_target, "m", &[]).len() < MAX_MODULE_BYTES + 32);
    }

    #[test]
    fn the_level_filters_and_noisy_crates_are_capped() {
        let lines = capture(LevelFilter::WARN, || {
            tracing::info!(target: "oxrtc", "hidden");
            tracing::warn!(target: "oxrtc", "shown");
        });
        assert_eq!(lines.len(), 1);
        let lines = capture(LevelFilter::DEBUG, || {
            tracing::debug!(target: "str0m::rtp", "flood");
            tracing::info!(target: "str0m::rtp", "kept");
            tracing::debug!(target: "oxrtc_ffmpeg", "kept");
        });
        assert_eq!(lines.len(), 2, "{lines:?}");
    }

    #[test]
    fn retina_warnings_are_demoted_to_debug() {
        assert_eq!(bridged_level("retina", log::Level::Warn), log::Level::Debug);
        assert_eq!(bridged_level("retina::client", log::Level::Warn), log::Level::Debug);
        assert_eq!(bridged_level("retina", log::Level::Error), log::Level::Error);
        assert_eq!(bridged_level("retinal", log::Level::Warn), log::Level::Warn);
        assert_eq!(bridged_level("other", log::Level::Warn), log::Level::Warn);
        let record = |target: &'static str, level| {
            Bridge.log(&log::Record::builder().args(format_args!("late packet")).level(level).target(target).build());
        };
        let at_info = capture(LevelFilter::INFO, || {
            record("retina::client", log::Level::Warn);
            record("other", log::Level::Warn);
        });
        assert_eq!(at_info.len(), 1, "{at_info:?}");
        assert_shape(&at_info[0], "WRN", "other");
        let at_debug = capture(LevelFilter::DEBUG, || record("retina::client", log::Level::Warn));
        assert_eq!(at_debug.len(), 1, "{at_debug:?}");
        assert_shape(&at_debug[0], "DBG", "rtsp");
        assert!(at_debug[0].ends_with("late packet"), "{}", at_debug[0]);
    }

    #[test]
    fn levels() {
        assert_eq!(parse_level("INFO"), Some(LevelFilter::INFO));
        assert_eq!(parse_level("trace"), Some(LevelFilter::TRACE));
        assert_eq!(parse_level("disabled"), Some(LevelFilter::OFF));
        assert_eq!(parse_level("loud"), None);
        assert_eq!(log_max(LevelFilter::OFF), log::LevelFilter::Off);
        assert_eq!(log_max(LevelFilter::DEBUG), log::LevelFilter::Debug);
    }
}
