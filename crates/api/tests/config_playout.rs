//! D30: how a WebRTC viewer's sound and picture are sent to be played
//! together - `webrtc.lip_sync`, `webrtc.max_sound_late_ms` and
//! `webrtc.sound_lag_ms`, oxrtc-only keys inside go2rtc's `webrtc:` section.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing, clippy::panic)]

use oxrtc_api::config::{self, ConfigError};

#[test]
fn lip_sync_and_the_sound_bound_are_read_and_checked() {
    // Absent: lip sync on, the server's bound (D30).
    let (cfg, _) = config::parse("webrtc:\n  listen: \":8555\"\n").unwrap();
    assert!(cfg.webrtc.lip_sync);
    assert_eq!(cfg.webrtc.max_sound_late_ms, None);
    let (cfg, warnings) = config::parse("webrtc:\n  lip_sync: false\n  max_sound_late_ms: 0\n").unwrap();
    assert!(warnings.is_empty(), "{warnings:?}");
    assert!(!cfg.webrtc.lip_sync);
    assert_eq!(cfg.webrtc.max_sound_late_ms, Some(0));
    let (cfg, _) = config::parse("webrtc:\n  lip_sync:\n  max_sound_late_ms: 60000\n").unwrap();
    assert!(cfg.webrtc.lip_sync, "empty: the default");
    assert_eq!(cfg.webrtc.max_sound_late_ms, Some(60_000));
    for bad in [
        "webrtc:\n  lip_sync: 'off'\n",
        "webrtc:\n  max_sound_late_ms: 60001\n",
        "webrtc:\n  max_sound_late_ms: -1\n",
        "webrtc:\n  max_sound_late_ms: 0.5\n",
        "webrtc:\n  max_sound_late_ms: soon\n",
    ] {
        assert!(matches!(config::parse(bad), Err(ConfigError::Invalid(_))), "{bad}");
    }
}

/// `webrtc.sound_lag_ms` (W1c): 0 to 40 ms, the most the owner's answer
/// allows a sound to be dated after its picture; more is a mistake in the
/// file, said at the start, not a value cut in silence.
#[test]
fn the_sound_lag_is_read_and_bounded() {
    let (cfg, _) = config::parse("webrtc:\n  listen: \":8555\"\n").unwrap();
    assert_eq!(cfg.webrtc.sound_lag_ms, None, "absent: the server's default");
    for (text, expected) in [("0", Some(0)), ("20", Some(20)), ("40", Some(40)), ("", None)] {
        let (cfg, warnings) = config::parse(&format!("webrtc:\n  sound_lag_ms: {text}\n")).unwrap();
        assert!(warnings.is_empty(), "{text:?}: {warnings:?}");
        assert_eq!(cfg.webrtc.sound_lag_ms, expected, "{text:?}");
    }
    assert_eq!(config::MAX_SOUND_LAG_MS, 40);
    for bad in ["41", "-1", "0.5", "soon", "[30]"] {
        let text = format!("webrtc:\n  sound_lag_ms: {bad}\n");
        let Err(ConfigError::Invalid(why)) = config::parse(&text) else { panic!("{bad} was taken") };
        assert!(why.contains("webrtc.sound_lag_ms") && why.contains("0 to 40"), "{why}");
    }
}
