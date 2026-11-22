//! A WebRTC session no packet of its viewer ever reached, named as such in
//! the log. The live view of openIVR on a phone of the owner's LAN,
//! 2026-11-22: every camera retried every 30 s, each session ending `ice
//! disconnected sent=0`, while Windows' firewall had dropped every check the
//! phone sent (no rule for `oxrtc.exe`, which replaced `go2rtc.exe`); this
//! machine's own browser, which the firewall does not filter, played.

use std::sync::Mutex;
use std::time::{Duration, Instant};

/// The reason a session that never connected ended with, said as what it
/// was: str0m's `ice disconnected` or the idle timeout read as a lost
/// connection where there never was one.
pub(crate) fn reason(reason: String, connected: bool, heard: bool) -> String {
    if connected || !matches!(reason.as_str(), "ice disconnected" | "idle timeout") {
        return reason;
    }
    if heard { "ice never connected".into() } else { "no packet from the viewer reached this server".into() }
}

/// The last time a session no packet reached was warned about.
static UNREACHED_WARNED: Mutex<Option<Instant>> = Mutex::new(None);
/// At most one such warning in this long.
const UNREACHED_WARN_EVERY: Duration = Duration::from_secs(60);

/// Whether a minute passed since the last warning (and counts this one).
pub(crate) fn warn_now(now: Instant) -> bool {
    let Ok(mut last) = UNREACHED_WARNED.lock() else { return false };
    if last.is_some_and(|at| now.saturating_duration_since(at) < UNREACHED_WARN_EVERY) {
        return false;
    }
    *last = Some(now);
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_session_that_never_connected_says_why() {
        let never = |r: &str, heard| reason(r.into(), false, heard);
        assert_eq!(never("ice disconnected", false), "no packet from the viewer reached this server");
        assert_eq!(never("idle timeout", false), "no packet from the viewer reached this server");
        assert_eq!(never("ice disconnected", true), "ice never connected");
        // Other ends are what they say, and a session that connected keeps str0m's words.
        assert_eq!(never("cancelled", false), "cancelled");
        assert_eq!(reason("ice disconnected".into(), true, true), "ice disconnected");
        assert_eq!(reason("idle timeout".into(), true, true), "idle timeout");
    }

    #[test]
    fn the_warning_is_said_at_most_once_a_minute() {
        let t0 = Instant::now();
        // The first call of the process may follow another test's: start clean.
        *UNREACHED_WARNED.lock().unwrap() = None;
        assert!(warn_now(t0));
        assert!(!warn_now(t0 + Duration::from_secs(59)));
        assert!(warn_now(t0 + Duration::from_secs(60)));
    }
}
