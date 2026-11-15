//! R30: the streams' limits (in `api:`, D16) and the consumer queues'
//! budgets (in `rtsp:` and `webrtc:`, D15), oxrtc-only keys inside go2rtc's
//! sections.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing, clippy::panic)]

use oxrtc_api::config::{self, ConfigError, MAX_MIB, MAX_SECONDS, QueueLimits, StreamLimits};

#[test]
fn the_stream_limits_and_queue_budgets_are_read() {
    let (cfg, warnings) = config::parse(
        "api:
  max_streams: 100
  max_consumers_per_stream: 16
  max_frame_mib: 12
  gop_cache_mib: 32
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
