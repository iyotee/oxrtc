//! One source of a stream: its demand count and the supervisor that runs its
//! driver while there is demand.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use tokio::runtime::Handle;
use tokio::sync::watch;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use crate::channel::{Channel, ProducerSink};
use crate::driver::{Declared, SourceDriver, SourceError};
use crate::info::SourceState;
use crate::limits::Limits;
use crate::media::{MediaKind, TrackInfo};
use crate::retry::{Retry, RunEnd};
use crate::spec::SourceSpec;
use crate::want::MediaWant;

const WATCHDOG_TICK: Duration = Duration::from_millis(250);

#[derive(Debug, Default)]
struct Demand {
    count: usize,
    generation: u64,
    next_run: u64,
    running: Option<(CancellationToken, u64)>,
    closed: bool,
}

/// Whether a source can serve a wish.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Offer {
    Yes,
    No,
    Unknown,
}

pub(crate) struct Slot {
    pub(crate) spec: SourceSpec,
    driver: Arc<dyn SourceDriver>,
    pub(crate) channel: Arc<Channel>,
    demand: Mutex<Demand>,
    state: watch::Sender<SourceState>,
    /// Failed attempts since the slot was built. A count, not the state: the
    /// state leaves `Retrying` again on the next attempt, and a watcher that
    /// looks between the two would miss the failure.
    failures: watch::Sender<u64>,
    /// Why the last attempt failed, until a run delivers media again: kept
    /// once the source is idle, so an operator can still read it.
    last_error: Mutex<Option<String>>,
    /// The last failed attempt was declined by the peer (`SourceError::Declined`).
    /// Set before `failures` grows, so a watcher that sees the count sees it.
    last_declined: AtomicBool,
    /// The decline last said in the log, until the source delivers: a camera
    /// with no microphone declines its sound transcode for every viewer, and
    /// that is said once, not once per viewer.
    declined_said: Mutex<Option<String>>,
    limits: Limits,
}

impl Slot {
    pub(crate) fn new(spec: SourceSpec, driver: Arc<dyn SourceDriver>, limits: &Limits, epoch: Instant) -> Arc<Self> {
        let (state, _) = watch::channel(SourceState::Idle);
        let (failures, _) = watch::channel(0);
        Arc::new(Self {
            channel: Channel::new(limits, epoch, spec.to_string()),
            spec,
            driver,
            demand: Mutex::new(Demand::default()),
            state,
            failures,
            last_error: Mutex::new(None),
            last_declined: AtomicBool::new(false),
            declined_said: Mutex::new(None),
            limits: limits.clone(),
        })
    }

    /// Whether its last failed attempt was declined by the peer: up, and
    /// saying no (a 4XX). A consumer that never got anything from it lets it
    /// go (`Subscription`), as go2rtc tries such a source once per consumer.
    pub(crate) fn declined(&self) -> bool {
        self.last_declined.load(Ordering::SeqCst)
    }

    /// Whether its last attempt was declined and it is waiting to try again:
    /// a consumer that came now would wait for the end of that backoff.
    pub(crate) fn declined_and_waiting(&self) -> bool {
        self.declined() && matches!(*self.state.borrow(), SourceState::Retrying { .. })
    }

    /// Stops the run `run` when nothing demands the source any more: retrying
    /// a source no one reads only runs it again for nothing (a sound transcode
    /// its input declines was run every few seconds through the idle grace).
    /// The next demand starts it at once. Checked and cleared under the demand
    /// lock, so a demand that arrives meanwhile starts a run of its own.
    fn retire_if_unwanted(&self, run: u64) -> bool {
        let mut demand = self.lock();
        if demand.count > 0 || !demand.running.as_ref().is_some_and(|(_, r)| *r == run) {
            return false;
        }
        demand.running = None;
        true
    }

    fn lock(&self) -> MutexGuard<'_, Demand> {
        self.demand.lock().unwrap_or_else(PoisonError::into_inner)
    }

    pub(crate) fn state(&self) -> SourceState {
        self.state.borrow().clone()
    }

    fn set_state(&self, state: SourceState) {
        self.state.send_if_modified(|current| {
            let changed = *current != state;
            *current = state;
            changed
        });
    }

    pub(crate) fn demand(&self) -> usize {
        self.lock().count
    }

    /// Its count of failed attempts, as it grows.
    pub(crate) fn watch_failures(&self) -> watch::Receiver<u64> {
        self.failures.subscribe()
    }

    /// Whether the source reads this server ([`SourceDriver::local`]).
    pub(crate) fn is_local(&self) -> bool {
        self.driver.local()
    }

    /// Why the last attempt failed, unless a run delivered media since.
    pub(crate) fn last_error(&self) -> Option<String> {
        self.last_error.lock().unwrap_or_else(PoisonError::into_inner).clone()
    }

    fn set_last_error(&self, error: Option<String>) {
        *self.last_error.lock().unwrap_or_else(PoisonError::into_inner) = error;
    }

    /// Failed attempts so far.
    pub(crate) fn failures(&self) -> u64 {
        *self.failures.borrow()
    }

    /// Whether its last attempt failed (it is waiting to retry, or gave up).
    pub(crate) fn has_failed(&self) -> bool {
        failed(&self.state.borrow())
    }

    pub(crate) fn declared(&self) -> Option<Declared> {
        self.driver.declared()
    }

    pub(crate) fn offers(&self, kind: MediaKind, want: &MediaWant) -> Offer {
        let matches = |tracks: &[TrackInfo]| tracks.iter().any(|t| t.kind == kind && want.accepts(t.codec));
        if let Some(tracks) = self.channel.tracks() {
            return if matches(&tracks) { Offer::Yes } else { Offer::No };
        }
        match self.declared() {
            Some(declared) if declared.iter().any(|(k, c)| *k == kind && want.accepts(*c)) => Offer::Yes,
            Some(_) => Offer::No,
            None => Offer::Unknown,
        }
    }

    /// Counts one more consumer, starting the source if it is not running.
    pub(crate) fn acquire(self: &Arc<Self>) -> DemandGuard {
        let mut demand = self.lock();
        demand.count += 1;
        demand.generation += 1;
        if demand.running.is_none()
            && !demand.closed
            && let Ok(handle) = Handle::try_current()
        {
            let cancel = CancellationToken::new();
            demand.next_run += 1;
            let run = demand.next_run;
            demand.running = Some((cancel.clone(), run));
            // Before the task runs: a caller watching the state must not read
            // the previous run's failure as this one's.
            self.set_state(SourceState::Connecting { attempt: 0 });
            handle.spawn(supervise(self.clone(), cancel, run));
        }
        DemandGuard { slot: self.clone() }
    }

    fn release(self: &Arc<Self>) {
        let mut demand = self.lock();
        demand.count = demand.count.saturating_sub(1);
        if demand.count > 0 {
            return;
        }
        let generation = demand.generation;
        drop(demand);
        match Handle::try_current() {
            Ok(handle) => {
                let slot = self.clone();
                let grace =
                    if self.driver.transcodes() { self.limits.transcode_idle_grace } else { self.limits.idle_grace };
                handle.spawn(async move {
                    tokio::time::sleep(grace).await;
                    slot.stop_if_idle(generation);
                });
            }
            Err(_) => self.stop_if_idle(generation),
        }
    }

    fn stop_if_idle(&self, generation: u64) {
        let mut demand = self.lock();
        if demand.count == 0
            && demand.generation == generation
            && let Some((cancel, _)) = demand.running.take()
        {
            cancel.cancel();
        }
    }

    /// Stops the source for good: the stream was removed or replaced.
    pub(crate) fn shutdown(&self) {
        let mut demand = self.lock();
        demand.closed = true;
        if let Some((cancel, _)) = demand.running.take() {
            cancel.cancel();
        }
    }

    fn finished(&self, run: u64) {
        let mut demand = self.lock();
        if demand.running.as_ref().is_some_and(|(_, r)| *r == run) {
            demand.running = None;
        }
    }
}

/// One unit of demand on a source; dropping it releases the demand.
pub(crate) struct DemandGuard {
    slot: Arc<Slot>,
}

impl Drop for DemandGuard {
    fn drop(&mut self) {
        self.slot.release();
    }
}

/// A state that says the last attempt failed.
pub(crate) fn failed(state: &SourceState) -> bool {
    matches!(state, SourceState::Retrying { .. } | SourceState::Failed { .. })
}

async fn supervise(slot: Arc<Slot>, cancel: CancellationToken, run: u64) {
    let limits = slot.limits.clone();
    let mut retry = Retry::new(&limits, slot.driver.cheap_attempts());
    // Whether a run delivered a frame since this demand began. A source that
    // did is one that works and dropped: it is retried fast, whatever said no.
    let mut delivered = false;
    let mut attempt: u32 = 0;
    let mut failed = false;
    // The retry warning is said at the first failure, when the error
    // changes, then at attempts 2, 4, 8...: a camera down for a day is not
    // 17,000 identical lines. The others are debug.
    let mut warned: Option<String> = None;
    loop {
        slot.set_state(SourceState::Connecting { attempt });
        slot.channel.reset();
        let run_cancel = cancel.child_token();
        let started = Instant::now();
        let driver_run = slot.driver.clone().run(ProducerSink::new(slot.channel.clone()), run_cancel.clone());
        let outcome = tokio::select! {
            biased;
            () = cancel.cancelled() => None,
            result = driver_run => Some(result),
            error = watchdog(&slot, &limits, started) => Some(Err(error)),
        };
        run_cancel.cancel();
        let Some(outcome) = outcome else { break };
        let ran = started.elapsed();
        let run_delivered = slot.channel.last_frame().is_some();
        let healthy = run_delivered && ran >= limits.healthy_after;
        if run_delivered {
            delivered = true;
            if healthy {
                retry.reset();
                attempt = 0;
            }
        }
        attempt = attempt.saturating_add(1);
        let error = outcome.err().unwrap_or(SourceError::Ended);
        let declined = matches!(error, SourceError::Declined(_));
        slot.last_declined.store(declined, Ordering::SeqCst);
        slot.failures.send_modify(|n| *n = n.saturating_add(1));
        slot.set_last_error(Some(error.to_string()));
        if !error.is_retryable() {
            tracing::warn!(source = %slot.spec, %error, "source failed and will not be retried");
            slot.set_state(SourceState::Failed { error: error.to_string() });
            failed = true;
            break;
        }
        let delay = retry.delay(&error, RunEnd { delivered: run_delivered, healthy, ever_delivered: delivered, ran });
        let text = error.to_string();
        let delay_ms = delay.as_millis() as u64;
        if declined {
            // Not a fault of this server or of the camera's picture: the
            // input is up and says no (a sound transcode of a camera with no
            // microphone, openIVR's de-duplicated sound for a sound it cannot
            // take). Its consumers go on without it; said once per source
            // until it delivers, at info, and readable in `sources[].error`.
            let mut said = slot.declined_said.lock().unwrap_or_else(PoisonError::into_inner);
            if said.as_deref() == Some(text.as_str()) {
                tracing::debug!(source = %slot.spec, attempt, delay_ms, %error, "source declined by its input");
            } else {
                tracing::info!(source = %slot.spec, attempt, delay_ms, %error, "source declined by its input");
                *said = Some(text.clone());
            }
        } else if warned.as_deref() != Some(text.as_str()) || attempt.is_power_of_two() {
            tracing::warn!(source = %slot.spec, attempt, delay_ms, %error, "source stopped, retrying");
            warned = Some(text);
        } else {
            tracing::debug!(source = %slot.spec, attempt, delay_ms, %error, "source stopped, retrying");
        }
        slot.set_state(SourceState::Retrying { attempt, error: error.to_string() });
        if slot.retire_if_unwanted(run) {
            break;
        }
        tokio::select! {
            biased;
            () = cancel.cancelled() => break,
            () = tokio::time::sleep(delay) => {}
        }
        if slot.retire_if_unwanted(run) {
            break;
        }
    }
    slot.channel.reset();
    if !failed {
        slot.set_state(SourceState::Idle);
    }
    slot.finished(run);
}

async fn watchdog(slot: &Slot, limits: &Limits, started: Instant) -> SourceError {
    let stall_timeout = slot.driver.stall_timeout().unwrap_or(limits.stall_timeout);
    let connect_timeout =
        limits.connect_timeout.max(stall_timeout).max(slot.driver.start_timeout().unwrap_or_default());
    let mut tick = tokio::time::interval(WATCHDOG_TICK);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut running = false;
    // A source that reads this server is silent when its input is, which is
    // judged where it is read: restarting the reader (an FFmpeg) only makes it
    // probe again once its input is back.
    let judged = !slot.driver.local();
    let mut said = None;
    loop {
        tick.tick().await;
        match slot.channel.last_frame() {
            None if started.elapsed() >= connect_timeout => return SourceError::NoMedia,
            None => {}
            Some(at) => {
                if !running {
                    running = true;
                    slot.set_state(SourceState::Running);
                    slot.set_last_error(None);
                    *slot.declined_said.lock().unwrap_or_else(PoisonError::into_inner) = None;
                }
                let (silence, longest) = if judged {
                    slot.channel.silence_after(limits.silence_floor, stall_timeout)
                } else {
                    (stall_timeout, None)
                };
                if let Some(longest) = longest
                    && said != Some(silence)
                {
                    tracing::debug!(
                        source = %slot.spec,
                        longest_gap_ms = longest.as_millis() as u64,
                        silence_ms = silence.as_millis() as u64,
                        "silence judged from the source's own cadence"
                    );
                    said = Some(silence);
                }
                let silent = at.elapsed();
                if silent >= silence {
                    return match longest {
                        Some(longest) if silence < stall_timeout => SourceError::Silent {
                            silent_ms: silent.as_millis() as u64,
                            allowed_ms: silence.as_millis() as u64,
                            longest_gap_ms: longest.as_millis() as u64,
                        },
                        _ => SourceError::Stalled,
                    };
                }
            }
        }
    }
}
