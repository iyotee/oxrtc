//! A source's silence judged from its own cadence, and a source that dropped
//! asked again on a short cadence (D34), on scripted sources and virtual time.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing, clippy::panic)]

mod common;

use std::sync::Arc;
use std::time::Duration;

use common::{Fake, FakeFactory, Script, hub_with, limits};
use oxrtc_core::{Codec, ConsumerTag, Hub, Limits, Subscription, Want};
use tokio::time::{Instant, sleep, sleep_until};

const FRAME: Duration = Duration::from_millis(40);

// A restart is seen up to a watchdog tick (250 ms) and a first delay (the
// backoff's 500 ms, +25 % jitter) after the silence that causes it: the
// checks below leave that much after each bound.

/// The defaults that matter here: a 15 s stall timeout, a 5 s floor, half a
/// second between quick attempts for two minutes, a backoff up to 5 s.
fn recovery_limits() -> Limits {
    let defaults = Limits::default();
    Limits {
        stall_timeout: defaults.stall_timeout,
        connect_timeout: defaults.connect_timeout,
        backoff_initial: defaults.backoff_initial,
        backoff_max: defaults.backoff_max,
        silence_floor: defaults.silence_floor,
        ..limits()
    }
}

/// `count` frames `FRAME` apart, keyframe every 25.
fn camera(count: usize) -> Script {
    Script::video(Codec::H264, (0..count).map(|i| (0, i % 25 == 0)).collect())
}

/// The fake, the hub that must outlive the test, its one viewer, and when it subscribed.
async fn started(script: Script) -> (Arc<Fake>, Arc<Hub>, Subscription, Instant) {
    let factory = Arc::new(FakeFactory::default());
    let fake = factory.add("cam", script);
    let hub = hub_with(factory, recovery_limits());
    let stream = hub.upsert("cam", &["fake:cam"]).unwrap();
    let t0 = Instant::now();
    let sub = stream.subscribe(&Want::all(), ConsumerTag::new("test")).await.unwrap();
    (fake, hub, sub, t0)
}

#[tokio::test(start_paused = true)]
async fn a_camera_that_goes_silent_is_restarted_after_the_floor_not_the_stall_timeout() {
    // 20 s at 25 pictures a second, then nothing.
    let (fake, hub, _sub, t0) = started(camera(500)).await;
    let silent_from = t0 + FRAME * 500;
    sleep_until(silent_from + Duration::from_millis(4_700)).await;
    assert_eq!(fake.runs(), 1, "within the 5 s floor: not restarted");
    // Cut within a watchdog tick of 5 s, then waiting at least 375 ms.
    sleep_until(silent_from + Duration::from_millis(5_300)).await;
    let source = &hub.get("cam").unwrap().snapshot().sources[0];
    let error = source.last_error.clone().unwrap_or_default();
    assert!(error.contains("beyond the 5000 ms its cadence allows (longest gap 40 ms)"), "{error:?}");
    sleep_until(silent_from + Duration::from_millis(6_000)).await;
    assert!(fake.runs() >= 2, "silent 5 s past a 40 ms cadence: restarted, where the stall timeout waited 15 s");
}

#[tokio::test(start_paused = true)]
async fn a_picture_every_two_seconds_is_never_cut_and_may_pause_eight() {
    let mut script = camera(30);
    script.interval = Duration::from_secs(2);
    let (fake, _hub, _sub, t0) = started(script).await;
    let silent_from = t0 + Duration::from_secs(60);
    sleep_until(silent_from + Duration::from_millis(7_700)).await;
    assert_eq!(fake.runs(), 1, "30 pictures 2 s apart and 7.7 s of silence: not restarted");
    sleep_until(silent_from + Duration::from_millis(9_000)).await;
    assert!(fake.runs() >= 2, "four times its longest gap: restarted");
}

#[tokio::test(start_paused = true)]
async fn a_camera_whose_rate_drops_at_night_is_not_cut() {
    // 20 s at 25 a second, then one picture a second for a minute.
    let mut script = camera(560);
    script.slow_after = Some((500, Duration::from_secs(1)));
    let (fake, _hub, _sub, t0) = started(script).await;
    sleep_until(t0 + Duration::from_secs(79)).await;
    assert_eq!(fake.runs(), 1);
}

#[tokio::test(start_paused = true)]
async fn until_its_cadence_is_known_a_source_has_its_stall_timeout() {
    let (fake, _hub, _sub, t0) = started(camera(5)).await;
    sleep_until(t0 + Duration::from_secs(14)).await;
    assert_eq!(fake.runs(), 1, "five frames teach nothing: the 15 s stall timeout holds");
    sleep_until(t0 + Duration::from_millis(16_200)).await;
    assert!(fake.runs() >= 2);
}

#[tokio::test(start_paused = true)]
async fn a_source_that_reads_this_server_keeps_its_stall_timeout() {
    let mut script = camera(500);
    script.local = true;
    let (fake, _hub, _sub, t0) = started(script).await;
    let silent_from = t0 + FRAME * 500;
    sleep_until(silent_from + Duration::from_secs(14)).await;
    assert_eq!(fake.runs(), 1, "its input is judged, not it");
    sleep_until(silent_from + Duration::from_millis(16_000)).await;
    assert!(fake.runs() >= 2);
}

/// The bench's R01: a camera that delivered drops for 10 s and comes back.
async fn outage(cheap: bool, down_for: Duration) -> (Arc<Fake>, Duration, usize) {
    let mut script = camera(500);
    script.cheap = cheap;
    let (fake, _hub, _sub, t0) = started(script).await;
    let down = t0 + FRAME * 500;
    sleep_until(down).await;
    fake.set_down(true);
    sleep_until(down + down_for).await;
    let attempts = fake.runs();
    fake.set_down(false);
    let up = Instant::now();
    sleep(Duration::from_secs(6)).await;
    let first = fake.first_frames().last().copied().unwrap();
    assert!(first > up, "a picture of the camera once it is back");
    (fake, first - up, attempts)
}

#[tokio::test(start_paused = true)]
async fn a_camera_back_from_an_outage_is_read_within_half_a_second() {
    for down_for in [Duration::from_secs(10), Duration::from_secs(20), Duration::from_secs(60)] {
        let (_fake, recovery, attempts) = outage(true, down_for).await;
        assert!(recovery <= Duration::from_millis(540), "{down_for:?} down: back after {recovery:?}");
        // Found silent after 5 s, then about two refused attempts a second.
        let expected = (down_for.as_secs_f64() - 5.0) * 2.0;
        assert!(attempts as f64 >= expected * 0.8 && attempts as f64 <= expected * 1.5 + 2.0, "{attempts}");
    }
}

#[tokio::test(start_paused = true)]
async fn a_driver_whose_attempt_starts_a_process_keeps_the_backoff() {
    let (_fake, recovery, attempts) = outage(false, Duration::from_secs(60)).await;
    // 0.5 s, 1, 2, 4, then 5 s apart (with jitter) in 55 s.
    assert!(attempts <= 20, "{attempts} attempts");
    assert!(recovery <= Duration::from_millis(5_040), "{recovery:?}");
}

#[tokio::test(start_paused = true)]
async fn the_short_cadence_ends_two_minutes_after_the_drop() {
    let mut script = camera(500);
    script.cheap = true;
    let (fake, _hub, _sub, t0) = started(script).await;
    let down = t0 + FRAME * 500;
    sleep_until(down).await;
    fake.set_down(true);
    sleep_until(down + Duration::from_secs(125)).await;
    let at_two_minutes = fake.runs();
    assert!(at_two_minutes >= 200, "{at_two_minutes} attempts in the first two minutes");
    sleep_until(down + Duration::from_secs(300)).await;
    let later = fake.runs() - at_two_minutes;
    assert!(later <= 60, "{later} attempts in the next three minutes: the backoff, not two a second");
}

#[tokio::test(start_paused = true)]
async fn refused_credentials_after_a_drop_keep_their_slow_backoff() {
    let mut script = camera(500);
    script.cheap = true;
    script.auth = true;
    let (fake, _hub, _sub, t0) = started(script).await;
    let down = t0 + FRAME * 500;
    sleep_until(down).await;
    fake.set_down(true);
    sleep_until(down + Duration::from_secs(120)).await;
    // The drop's own retry, then 30 s, 60 s (with jitter): never two a second.
    assert!(fake.runs() <= 5, "{} attempts", fake.runs());
}
