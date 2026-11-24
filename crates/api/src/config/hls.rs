//! `hls:` (oxrtc only, D47): the bounds of HLS and DASH sessions, each left
//! out at the server's default ([`crate::HlsLimits::default`]).
//!
//! go2rtc 1.9.15 has no such section (its session's 5 s `keepalive` is a
//! constant, and it reads a file that carries one unchanged); MediaMTX
//! 1.21.3's nearest keys are `hlsMuxerCloseAfter` (60 s, its idle time) and
//! nothing for sessions or waits, which it does not bound.

use std::time::Duration;

use serde_norway::Value;

use super::ConfigError;
use super::yaml::{bounded, count};
use crate::HlsLimits;

/// The longest `hls.max_wait_seconds`: a waiting request must be answered
/// before the slow routes' deadline ([`crate::SLOW_TIMEOUT`], 60 s) cuts it.
pub const MAX_HLS_WAIT_SECONDS: usize = 55;

/// The longest `hls.idle_seconds`: an hour, as the other times.
pub const MAX_HLS_IDLE_SECONDS: usize = super::MAX_SECONDS;

/// `hls:`; `None` is the server's default.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct HlsSection {
    /// `hls.max_sessions`: HLS and DASH sessions at once, every client together.
    pub max_sessions: Option<usize>,
    /// `hls.max_sessions_per_ip`: sessions one address holds at once.
    pub max_sessions_per_ip: Option<usize>,
    /// `hls.max_waiting`: requests waiting at once (a blocking reload, a
    /// part or segment not complete yet, a first playlist).
    pub max_waiting: Option<usize>,
    /// `hls.max_waiting_per_ip`: requests waiting at once from one address.
    pub max_waiting_per_ip: Option<usize>,
    /// `hls.max_wait_seconds`: the longest a request waits, whatever the
    /// segments' length (1 to [`MAX_HLS_WAIT_SECONDS`]).
    pub max_wait_seconds: Option<usize>,
    /// `hls.idle_seconds`: a session no request has touched for this long
    /// ends (go2rtc's `keepalive`), or for three of its longest segments if
    /// that is longer (1 to [`MAX_HLS_IDLE_SECONDS`]).
    pub idle_seconds: Option<usize>,
}

impl HlsSection {
    /// The bounds the server runs with: each key over its default.
    pub fn limits(&self) -> HlsLimits {
        let defaults = HlsLimits::default();
        let seconds = |s: usize| Duration::from_secs(u64::try_from(s).unwrap_or(u64::MAX));
        HlsLimits {
            max_sessions: self.max_sessions.unwrap_or(defaults.max_sessions),
            max_sessions_per_ip: self.max_sessions_per_ip.unwrap_or(defaults.max_sessions_per_ip),
            max_waiting: self.max_waiting.unwrap_or(defaults.max_waiting),
            max_waiting_per_ip: self.max_waiting_per_ip.unwrap_or(defaults.max_waiting_per_ip),
            max_wait: self.max_wait_seconds.map_or(defaults.max_wait, seconds),
            idle: self.idle_seconds.map_or(defaults.idle, seconds),
        }
    }
}

/// Every key of `hls:`, in the order the documents list them.
pub const HLS_KEYS: [&str; 6] =
    ["max_sessions", "max_sessions_per_ip", "max_waiting", "max_waiting_per_ip", "max_wait_seconds", "idle_seconds"];

/// Reads `hls.<key>` into `hls`; whether it is one of its keys.
pub(super) fn key(hls: &mut HlsSection, key: &str, value: &Value, path: &str) -> Result<bool, ConfigError> {
    match key {
        "max_sessions" => hls.max_sessions = count(value, path)?,
        "max_sessions_per_ip" => hls.max_sessions_per_ip = count(value, path)?,
        "max_waiting" => hls.max_waiting = count(value, path)?,
        "max_waiting_per_ip" => hls.max_waiting_per_ip = count(value, path)?,
        "max_wait_seconds" => hls.max_wait_seconds = bounded(value, path, MAX_HLS_WAIT_SECONDS)?,
        "idle_seconds" => hls.idle_seconds = bounded(value, path, MAX_HLS_IDLE_SECONDS)?,
        _ => return Ok(false),
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_empty_section_is_the_servers_default() {
        assert_eq!(HlsSection::default().limits(), HlsLimits::default());
    }

    #[test]
    fn each_key_reaches_its_bound_alone() {
        let section = HlsSection { max_wait_seconds: Some(12), idle_seconds: Some(60), ..HlsSection::default() };
        let limits = section.limits();
        assert_eq!(limits.max_wait, Duration::from_secs(12));
        assert_eq!(limits.idle, Duration::from_secs(60));
        assert_eq!(limits.max_sessions, HlsLimits::default().max_sessions);
    }

    #[test]
    fn the_longest_wait_is_answered_before_the_slow_routes_deadline() {
        let seconds = u64::try_from(MAX_HLS_WAIT_SECONDS).unwrap_or(u64::MAX);
        assert!(Duration::from_secs(seconds) < crate::SLOW_TIMEOUT);
    }
}
