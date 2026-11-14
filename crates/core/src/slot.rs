//! One source of a stream: its demand count and the supervisor that runs its
//! driver while there is demand.

use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use tokio::runtime::Handle;
use tokio::sync::watch;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use crate::backoff::Backoff;
use crate::channel::{Channel, ProducerSink};
use crate::driver::{Declared, SourceDriver, SourceError};
use crate::info::SourceState;
use crate::limits::Limits;
use crate::media::{MediaKind, TrackInfo};
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
    limits: Limits,
}

impl Slot {
    pub(crate) fn new(spec: SourceSpec, driver: Arc<dyn SourceDriver>, limits: &Limits) -> Arc<Self> {
        let (state, _) = watch::channel(SourceState::Idle);
        Arc::new(Self {
            spec,
            driver,
            channel: Channel::new(limits),
            demand: Mutex::new(Demand::default()),
            state,
            limits: limits.clone(),
        })
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
                let grace = self.limits.idle_grace;
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

async fn supervise(slot: Arc<Slot>, cancel: CancellationToken, run: u64) {
    let limits = slot.limits.clone();
    let mut backoff = Backoff::new(limits.backoff_initial, limits.backoff_max);
    let mut attempt: u32 = 0;
    let mut failed = false;
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
        if slot.channel.last_frame().is_some() && started.elapsed() >= limits.healthy_after {
            backoff.reset();
            attempt = 0;
        }
        attempt = attempt.saturating_add(1);
        let error = outcome.err().unwrap_or(SourceError::Ended);
        if !error.is_retryable() {
            tracing::warn!(source = %slot.spec, %error, "source failed and will not be retried");
            slot.set_state(SourceState::Failed { error: error.to_string() });
            failed = true;
            break;
        }
        let delay = backoff.next_delay();
        tracing::warn!(source = %slot.spec, attempt, delay_ms = delay.as_millis() as u64, %error, "source stopped, retrying");
        slot.set_state(SourceState::Retrying { attempt, error: error.to_string() });
        tokio::select! {
            biased;
            () = cancel.cancelled() => break,
            () = tokio::time::sleep(delay) => {}
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
    let connect_timeout = limits.connect_timeout.max(stall_timeout);
    let mut tick = tokio::time::interval(WATCHDOG_TICK);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut running = false;
    loop {
        tick.tick().await;
        match slot.channel.last_frame() {
            None if started.elapsed() >= connect_timeout => return SourceError::NoMedia,
            None => {}
            Some(at) => {
                if !running {
                    running = true;
                    slot.set_state(SourceState::Running);
                }
                if at.elapsed() >= stall_timeout {
                    return SourceError::Stalled;
                }
            }
        }
    }
}
