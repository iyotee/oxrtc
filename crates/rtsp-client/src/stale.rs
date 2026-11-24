//! How long a new session waits for the previous one's TEARDOWN.
//!
//! retina keeps the session a dropped one left in a [`SessionGroup`] until its
//! TEARDOWN is answered (or, for a camera that never answers, until its own
//! 5 s attempt ends). Waiting for that answer is cheap and useful with a camera
//! that gives one: it answers in milliseconds, and the new session then meets a
//! camera that has let go of the old one (some cameras ration their sessions,
//! others keep sending on a dead one). With a camera that never answers (go2rtc
//! 1.9.15 while it plays) the wait always ends at its bound and buys nothing:
//! the second viewer of L03 paid all of [`MAX_STALE_WAIT`] for it, again and
//! again (R35), and later the one second it still paid once per source made
//! one cold start in twenty miss the camera's keyframe and wait a whole group
//! of pictures more (M4 fix round 1: 4.9 s where go2rtc, which never waits,
//! took 0.1 s; the whole of L03's lost row).
//!
//! So the wait is learned per source, and the first one is short. A source
//! whose camera has not answered within the wait is *silent*, and is not
//! waited for again: a camera that answers does so within
//! [`SILENT_STALE_WAIT`], and one that does not lets the old session go when
//! its connection closes. A source whose camera has answered waits in full,
//! which it ends in milliseconds. A connection that fails after a wait shorter
//! than the full one takes the full wait back: the camera may have refused
//! the new session for want of the old one's end.
//!
//! [`SessionGroup`]: retina::client::SessionGroup

use std::sync::atomic::{AtomicU8, Ordering};
use std::time::Duration;

/// Longest wait, before reconnecting, for the TEARDOWN of the previous
/// session of the same source (cameras that keep sending on a dead session
/// confuse the next one): for a camera that has answered one, or refused a
/// session after a shorter wait. A camera that answers does so in
/// milliseconds; one that never answers (go2rtc 1.9.15 while it plays) keeps
/// retina's attempt, and its connection, open for 5 s whatever we wait.
pub const MAX_STALE_WAIT: Duration = Duration::from_secs(1);

/// The first wait of a source, before anything is known of its camera: long
/// enough for a camera on a LAN to answer, short enough not to be felt. A
/// camera that does not answer within it is not waited for again.
pub const SILENT_STALE_WAIT: Duration = Duration::from_millis(100);

// 0, the default: nothing learned yet.
const ANSWERS: u8 = 1;
const SILENT: u8 = 2;

/// What one source has taught about its camera's TEARDOWN.
#[derive(Debug, Default)]
pub(crate) struct StaleWait(AtomicU8);

impl StaleWait {
    /// How long to wait for the stale sessions now: none for a silent camera.
    pub(crate) fn budget(&self) -> Duration {
        match self.0.load(Ordering::Relaxed) {
            ANSWERS => MAX_STALE_WAIT,
            SILENT => Duration::ZERO,
            // Nothing learned yet.
            _ => SILENT_STALE_WAIT,
        }
    }

    /// The sessions pending at the start of a connection were waited for:
    /// `settled` is whether they all ended within the budget.
    pub(crate) fn waited(&self, settled: bool) {
        self.0.store(if settled { ANSWERS } else { SILENT }, Ordering::Relaxed);
    }

    /// A connection failed after a wait shorter than the full one: the next
    /// one waits in full.
    pub(crate) fn connection_failed(&self) {
        self.0.store(ANSWERS, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_source_waits_briefly_first_and_not_at_all_once_its_camera_is_silent() {
        let wait = StaleWait::default();
        assert_eq!(wait.budget(), SILENT_STALE_WAIT);
        wait.waited(false);
        assert_eq!(wait.budget(), Duration::ZERO);
    }

    #[test]
    fn a_camera_that_answers_is_waited_for_in_full() {
        let wait = StaleWait::default();
        wait.waited(true);
        assert_eq!(wait.budget(), MAX_STALE_WAIT);
        // A full wait that lapses proves silence too.
        wait.waited(false);
        assert_eq!(wait.budget(), Duration::ZERO);
    }

    #[test]
    fn a_failed_connection_takes_the_full_wait_back() {
        let wait = StaleWait::default();
        wait.waited(false);
        wait.connection_failed();
        assert_eq!(wait.budget(), MAX_STALE_WAIT);
        // Silence is learned again by the next wait that lapses.
        wait.waited(false);
        assert_eq!(wait.budget(), Duration::ZERO);
    }

    #[test]
    fn the_short_wait_is_shorter_than_the_full_one() {
        assert!(SILENT_STALE_WAIT < MAX_STALE_WAIT);
    }
}
