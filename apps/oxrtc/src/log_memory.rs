//! The log kept in memory for `GET /api/log`: every event the filter lets
//! through, as go2rtc's JSON line ([`oxrtc_api::LogRing::push_event`]).

use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use oxrtc_api::{LogEvent, LogRing};
use tracing::{Event, Level, Subscriber};
use tracing_log::NormalizeEvent;
use tracing_subscriber::layer::{Context, Layer};

use crate::logging::{Fields, named};

/// go2rtc's level name in its JSON lines.
pub fn level_name(level: Level) -> &'static str {
    match level {
        Level::TRACE => "trace",
        Level::DEBUG => "debug",
        Level::INFO => "info",
        Level::WARN => "warn",
        Level::ERROR => "error",
    }
}

/// The layer that keeps the lines.
#[derive(Debug, Clone)]
pub struct MemoryLayer(Arc<LogRing>);

impl MemoryLayer {
    /// Keeps every event in `ring`.
    pub fn new(ring: Arc<LogRing>) -> Self {
        Self(ring)
    }
}

impl<S: Subscriber> Layer<S> for MemoryLayer {
    fn on_event(&self, event: &Event<'_>, _ctx: Context<'_, S>) {
        let normalized = event.normalized_metadata();
        let metadata = normalized.as_ref().unwrap_or_else(|| event.metadata());
        let mut fields = Fields::default();
        event.record(&mut fields);
        let (module, message) = named(metadata.target(), &fields.message);
        let time_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX));
        self.0.push_event(&LogEvent {
            level: level_name(*metadata.level()),
            time_ms,
            module,
            message,
            fields: &fields.rest,
        });
    }
}

#[cfg(test)]
mod tests {
    use tracing_subscriber::layer::SubscriberExt;

    use super::*;

    #[test]
    fn every_event_is_kept_as_go2rtcs_json_line() {
        let ring = Arc::new(LogRing::default());
        let subscriber = tracing_subscriber::registry().with(MemoryLayer::new(ring.clone()));
        tracing::subscriber::with_default(subscriber, || {
            tracing::info!(target: "oxrtc_api::routes", stream = "cam", sources = 2, "api: stream set");
            tracing::warn!(target: "oxrtc::app", addr = "127.0.0.1:1", "rtsp: listen");
        });
        let text = ring.text();
        let lines: Vec<serde_json::Value> = text.lines().map(|l| serde_json::from_str(l).unwrap()).collect();
        assert_eq!(lines.len(), 2, "{text}");
        assert_eq!(lines[0]["level"], "info");
        assert_eq!(lines[0]["module"], "api");
        assert_eq!(lines[0]["message"], "stream set");
        assert_eq!(lines[0]["stream"], "cam");
        assert_eq!(lines[0]["sources"], "2");
        assert!(lines[0]["time"].as_u64().unwrap() > 1_700_000_000_000);
        assert_eq!((lines[1]["level"].as_str(), lines[1]["module"].as_str()), (Some("warn"), Some("rtsp")));
    }
}
