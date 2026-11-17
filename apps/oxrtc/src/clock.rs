//! The time a log line starts with: the local wall clock, as go2rtc prints it
//! (`HH:MM:SS.mmm`), so that a line of oxrtc's reads against the operator's
//! other logs without converting.
//!
//! The local time zone comes from jiff, which the RTSP client's retina already
//! brings into the binary: no crate of its own for one timestamp.

use jiff::Zoned;

/// `strftime` for hours, minutes, seconds and milliseconds (cut, not rounded).
const FORMAT: &str = "%H:%M:%S%.3f";

/// The local time now, as `HH:MM:SS.mmm`. Where the system names no time zone
/// (a bare container), it is UTC.
pub fn now() -> String {
    stamp(&Zoned::now())
}

/// `at` as `HH:MM:SS.mmm`, in its own time zone.
fn stamp(at: &Zoned) -> String {
    at.strftime(FORMAT).to_string()
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing, clippy::panic)]
mod tests {
    use jiff::Timestamp;
    use jiff::tz::{TimeZone, offset};

    use super::*;

    fn at(seconds: i64, nanoseconds: i32, hours_east: i8) -> Zoned {
        Timestamp::new(seconds, nanoseconds).unwrap().to_zoned(TimeZone::fixed(offset(hours_east)))
    }

    #[test]
    fn the_stamp_is_the_zones_wall_clock_to_the_millisecond() {
        // 2026-11-17 18:04:05.006 UTC.
        let utc = 1_794_938_645;
        assert_eq!(stamp(&at(utc, 6_000_000, 0)), "18:04:05.006");
        assert_eq!(stamp(&at(utc, 6_000_000, 1)), "19:04:05.006", "an hour east");
        assert_eq!(stamp(&at(utc, 6_000_000, -8)), "10:04:05.006", "eight hours west");
        assert_eq!(stamp(&at(utc, 6_000_000, 7)), "01:04:05.006", "past midnight, hours stay two digits");
    }

    #[test]
    fn milliseconds_are_cut_and_always_three_digits() {
        let utc = 1_794_938_645;
        assert_eq!(stamp(&at(utc, 0, 0)), "18:04:05.000", "a whole second keeps its fraction");
        assert_eq!(stamp(&at(utc, 999_999_999, 0)), "18:04:05.999", "cut: the second does not change");
        assert_eq!(stamp(&at(utc, 120_000_000, 0)), "18:04:05.120", "trailing zeros stay");
    }

    #[test]
    fn now_has_the_shape_openivr_parses() {
        let time = now();
        assert_eq!(time.len(), 12, "{time}");
        for (i, c) in time.chars().enumerate() {
            match i {
                2 | 5 => assert_eq!(c, ':', "{time}"),
                8 => assert_eq!(c, '.', "{time}"),
                _ => assert!(c.is_ascii_digit(), "{time}"),
            }
        }
    }

    /// The clock is the system's local time, not UTC with another name: the
    /// same instant read through the standard library is within a second of
    /// it once the zone's offset is taken off.
    #[test]
    fn now_is_the_systems_local_time() {
        let zoned = Zoned::now();
        let since_epoch = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap();
        let local_seconds = i64::try_from(since_epoch.as_secs()).unwrap() + i64::from(zoned.offset().seconds());
        let expected = local_seconds.rem_euclid(86_400);
        let time = stamp(&zoned);
        let field = |range: std::ops::Range<usize>| time[range].parse::<i64>().unwrap();
        let shown = field(0..2) * 3600 + field(3..5) * 60 + field(6..8);
        let apart = (shown - expected).rem_euclid(86_400);
        assert!(apart <= 1 || apart >= 86_399, "{time} against {expected} s of the local day");
    }
}
