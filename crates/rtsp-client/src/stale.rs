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
//! again.
//!
//! So the wait is learned per source. A source whose pending TEARDOWN has not
//! been seen answered within the bound is *silent*, and the next waits are a
//! short courtesy ([`SILENT_STALE_WAIT`]), which a camera that starts
//! answering in a few milliseconds still ends early (and is believed again). A
//! failed connection after a shortened wait takes the shortcut back: the camera
//! may have refused the new session for want of the old one's end.
//!
//! [`SessionGroup`]: retina::client::SessionGroup

use std::sync::atomic::{AtomicU8, Ordering};
use std::time::Duration;

/// Longest wait, before reconnecting, for the TEARDOWN of the previous
/// session of the same source (cameras that keep sending on a dead session
/// confuse the next one). A camera that answers does so in milliseconds; one
/// that never answers (go2rtc 1.9.15 while it plays) keeps retina's attempt,
/// and its connection, open for 5 s whatever we wait - waiting it out made
/// the next viewer wait too (1.9 s to its first answer in the bench's L03).
/// Paid once per source by a camera that does not answer ([`StaleWait`]).
pub const MAX_STALE_WAIT: Duration = Duration::from_secs(1);

/// The wait once a source's TEARDOWN has gone unanswered for [`MAX_STALE_WAIT`]:
/// long enough for a camera on a LAN to answer, short enough not to be felt.
pub const SILENT_STALE_WAIT: Duration = Duration::from_millis(100);

const UNKNOWN: u8 = 0;
const ANSWERS: u8 = 1;
const SILENT: u8 = 2;

/// What one source has taught about its camera's TEARDOWN.
#[derive(Debug, Default)]
pub(crate) struct StaleWait(AtomicU8);

impl StaleWait {
    /// How long to wait for the stale sessions now.
    pub(crate) fn budget(&self) -> Duration {
        if self.0.load(Ordering::Relaxed) == SILENT { SILENT_STALE_WAIT } else { MAX_STALE_WAIT }
    }

    /// The sessions pending at the start of a connection were waited for for
    /// `budget`: `settled` is whether they all ended within it.
    pub(crate) fn waited(&self, budget: Duration, settled: bool) {
        if settled {
            self.0.store(ANSWERS, Ordering::Relaxed);
        } else if budget >= MAX_STALE_WAIT {
            // Only the full wait proves silence: a short one that lapses on a
            // camera taking its time proves nothing new.
            self.0.store(SILENT, Ordering::Relaxed);
        }
    }

    /// A connection failed: whatever was learned is doubted again, so the next
    /// one waits in full.
    pub(crate) fn connection_failed(&self) {
        self.0.store(UNKNOWN, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_source_waits_in_full_until_its_camera_has_been_heard_not_to_answer() {
        let wait = StaleWait::default();
        assert_eq!(wait.budget(), MAX_STALE_WAIT);
        wait.waited(MAX_STALE_WAIT, false);
        assert_eq!(wait.budget(), SILENT_STALE_WAIT);
        // A short wait that lapses keeps it silent.
        wait.waited(SILENT_STALE_WAIT, false);
        assert_eq!(wait.budget(), SILENT_STALE_WAIT);
    }

    #[test]
    fn an_answered_teardown_is_believed_again_even_after_silence() {
        let wait = StaleWait::default();
        wait.waited(MAX_STALE_WAIT, false);
        wait.waited(SILENT_STALE_WAIT, true);
        assert_eq!(wait.budget(), MAX_STALE_WAIT);
        wait.waited(MAX_STALE_WAIT, true);
        assert_eq!(wait.budget(), MAX_STALE_WAIT);
    }

    #[test]
    fn a_failed_connection_takes_the_shortcut_back() {
        let wait = StaleWait::default();
        wait.waited(MAX_STALE_WAIT, false);
        wait.connection_failed();
        assert_eq!(wait.budget(), MAX_STALE_WAIT);
        // Silence is learned again by the next full wait.
        wait.waited(MAX_STALE_WAIT, false);
        assert_eq!(wait.budget(), SILENT_STALE_WAIT);
    }

    #[test]
    fn the_short_wait_is_shorter_than_the_full_one() {
        assert!(SILENT_STALE_WAIT < MAX_STALE_WAIT);
    }
}
