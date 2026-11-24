//! R30: the streams' limits (in `api:`, D16) and the consumer queues'
//! budgets (in `rtsp:` and `webrtc:`, D15), oxrtc-only keys inside go2rtc's
//! sections.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing, clippy::panic)]

use std::time::Duration;

use oxrtc_api::HlsLimits;
use oxrtc_api::config::{
    self, ConfigError, HlsSection, MAX_HLS_WAIT_SECONDS, MAX_MIB, MAX_SECONDS, QueueLimits, StreamLimits,
};

#[test]
fn the_stream_limits_and_queue_budgets_are_read() {
    let (cfg, warnings) = config::parse(
        "api:
  max_streams: 100
  max_consumers_per_stream: 16
  max_frame_mib: 12
  gop_cache_mib: 32
  idle_grace_ms: 2500
  transcode_idle_grace_ms: 0
rtsp:
  queue_max_mib: 48
  queue_max_seconds: 15
webrtc:
  queue_max_mib: 4
  queue_max_seconds: 1
",
    )
    .unwrap();
    assert!(warnings.is_empty(), "{warnings:?}");
    let streams = StreamLimits {
        max_streams: Some(100),
        max_consumers_per_stream: Some(16),
        max_frame_mib: Some(12),
        gop_cache_mib: Some(32),
        idle_grace_ms: Some(2500),
        transcode_idle_grace_ms: Some(0),
    };
    assert_eq!(cfg.api.streams, streams);
    assert_eq!(cfg.rtsp.queue, QueueLimits { max_mib: Some(48), max_seconds: Some(15) });
    assert_eq!(cfg.webrtc.queue, QueueLimits { max_mib: Some(4), max_seconds: Some(1) });
    // Absent or empty: the hub's defaults; a neighbour misspelt is still warned.
    let (cfg, warnings) = config::parse("api:\n  gop_cache_mib:\nrtsp:\n  queue_max_secs: 3\n").unwrap();
    assert_eq!(cfg.api.streams, StreamLimits::default());
    assert_eq!(cfg.rtsp.queue, QueueLimits::default());
    assert_eq!(warnings, ["unknown key rtsp.queue_max_secs ignored"]);
}

#[test]
fn the_stream_limits_and_queue_budgets_are_bounded() {
    let too_big = MAX_MIB + 1;
    let too_long = MAX_SECONDS + 1;
    for (bad, why) in [
        ("api:\n  max_streams: 0\n".to_owned(), "at least 1"),
        ("api:\n  max_consumers_per_stream: -1\n".to_owned(), "at least 1"),
        ("api:\n  gop_cache_mib: 0\n".to_owned(), "from 1 to 4096"),
        (format!("api:\n  gop_cache_mib: {too_big}\n"), "from 1 to 4096"),
        ("api:\n  max_frame_mib: 1.5\n".to_owned(), "from 1 to 4096"),
        ("rtsp:\n  queue_max_mib: lots\n".to_owned(), "from 1 to 4096"),
        (format!("rtsp:\n  queue_max_seconds: {too_long}\n"), "from 1 to 3600"),
        ("webrtc:\n  queue_max_seconds: 0\n".to_owned(), "from 1 to 3600"),
        // u64::MAX MiB would overflow a byte count; refused, never wrapped.
        ("webrtc:\n  queue_max_mib: 18446744073709551615\n".to_owned(), "from 1 to 4096"),
    ] {
        let error = config::parse(&bad).unwrap_err();
        assert!(matches!(error, ConfigError::Invalid(ref text) if text.contains(why)), "{bad}: {error}");
    }
}

#[test]
fn the_hls_limits_are_read_and_reach_the_sessions() {
    let (cfg, warnings) = config::parse(
        "hls:
  max_sessions: 8
  max_sessions_per_ip: 2
  max_waiting: 40
  max_waiting_per_ip: 4
  max_wait_seconds: 20
  idle_seconds: 60
",
    )
    .unwrap();
    assert!(warnings.is_empty(), "{warnings:?}");
    let section = HlsSection {
        max_sessions: Some(8),
        max_sessions_per_ip: Some(2),
        max_waiting: Some(40),
        max_waiting_per_ip: Some(4),
        max_wait_seconds: Some(20),
        idle_seconds: Some(60),
    };
    assert_eq!(cfg.hls, section);
    let limits = HlsLimits {
        max_sessions: 8,
        max_sessions_per_ip: 2,
        max_waiting: 40,
        max_waiting_per_ip: 4,
        max_wait: Duration::from_secs(20),
        idle: Duration::from_secs(60),
    };
    assert_eq!(cfg.hls.limits(), limits);
    // Absent, empty or a section of go2rtc's file: the server's defaults; a key misspelt is warned.
    let (cfg, warnings) = config::parse(
        "hls:
  idle_seconds:
  max_session: 3
",
    )
    .unwrap();
    assert_eq!(cfg.hls.limits(), HlsLimits::default());
    assert_eq!(warnings, ["unknown key hls.max_session ignored"]);
    assert_eq!(
        config::parse(
            "api:
  token: t
"
        )
        .unwrap()
        .0
        .hls,
        HlsSection::default()
    );
}

#[test]
fn the_hls_limits_are_bounded() {
    let too_long_wait = MAX_HLS_WAIT_SECONDS + 1;
    let too_long_idle = MAX_SECONDS + 1;
    for (bad, why) in [
        ("hls:\n  max_sessions: 0\n".to_owned(), "at least 1"),
        ("hls:\n  max_sessions_per_ip: -2\n".to_owned(), "at least 1"),
        ("hls:\n  max_waiting: many\n".to_owned(), "at least 1"),
        ("hls:\n  max_waiting_per_ip: 1.5\n".to_owned(), "at least 1"),
        ("hls:\n  max_wait_seconds: 0\n".to_owned(), "from 1 to 55"),
        (format!("hls:\n  max_wait_seconds: {too_long_wait}\n"), "from 1 to 55"),
        ("hls:\n  idle_seconds: 0\n".to_owned(), "from 1 to 3600"),
        (format!("hls:\n  idle_seconds: {too_long_idle}\n"), "from 1 to 3600"),
        ("hls: [1, 2]\n".to_owned(), "hls: expected a map"),
    ] {
        let error = config::parse(&bad).unwrap_err();
        assert!(matches!(error, ConfigError::Invalid(ref text) if text.contains(why)), "{bad}: {error}");
    }
    // Every key is named by its path when refused.
    for key in config::HLS_KEYS {
        let error = config::parse(&format!("hls:\n  {key}: \"x\"\n")).unwrap_err();
        assert!(error.to_string().contains(&format!("hls.{key}")), "{error}");
    }
}

#[test]
fn every_hls_key_is_documented_with_the_others() {
    let doc = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/../../docs/COMPATIBILITY.md")).unwrap();
    for key in config::HLS_KEYS {
        assert!(doc.contains(&format!("| `hls.{key}` |")), "hls.{key} is not in COMPATIBILITY.md's table of keys");
    }
}
