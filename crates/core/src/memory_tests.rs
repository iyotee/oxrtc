#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing, clippy::panic)]

use std::sync::{Arc, Mutex};

use bytes::Bytes;
use tokio::time::Instant;

use super::*;
use crate::channel::Item;
use crate::frame::Frame;
use crate::host_memory::{Fixed, HostReading, MemorySource};
use crate::queue::{ConsumerQueue, QueueBudget};

const MIB: u64 = 1024 * 1024;
const GIB: u64 = 1024 * MIB;

fn reading(limit: u64, available: u64) -> HostReading {
    HostReading { limit, available, source: MemorySource::Meminfo }
}

/// A host whose memory the test changes: no gigabyte is ever allocated.
struct Injected(Mutex<HostReading>);

impl HostMemory for Arc<Injected> {
    fn read(&self) -> Option<HostReading> {
        Some(*self.0.lock().unwrap())
    }
}

fn frame(key: bool, len: usize) -> Item {
    Item::Frame(Arc::new(Frame::new(0, 0, key, Bytes::from(vec![0; len]))))
}

const LIVE: QueueBudget = QueueBudget { max_bytes: 8 << 20, max_age: Duration::from_secs(3600) };
const RECORDER: QueueBudget = QueueBudget { max_bytes: 32 << 20, max_age: Duration::from_secs(3600) };

#[test]
fn the_default_is_an_eighth_of_what_the_process_may_take() {
    let fraction = BudgetSize::default();
    // A 4 GiB Raspberry Pi with openIVR beside: about 2.3 GiB available.
    assert_eq!(size_from(fraction, Some(reading(3_950 * MIB, 2_360 * MIB)), 0), 295 * MIB);
    // An 8 GiB one: about 6.6 GiB.
    assert_eq!(size_from(fraction, Some(reading(7_900 * MIB, 6_600 * MIB)), 0), 825 * MIB);
    // What the budget holds is counted as takeable: its own bytes are not taken from it twice.
    assert_eq!(size_from(fraction, Some(reading(8 * GIB, 6 * GIB)), 2 * GIB), GIB);
    // Never more than the share of the limit, never less than the floor.
    assert_eq!(size_from(fraction, Some(reading(GIB, GIB)), 4 * GIB), 128 * MIB);
    assert_eq!(size_from(fraction, Some(reading(GIB, 10 * MIB)), 0), MIN_BUDGET);
    assert_eq!(size_from(fraction, None, 0), MIN_BUDGET, "no reading: the floor");
}

#[test]
fn a_size_given_is_kept_unless_the_process_cannot_take_it() {
    assert_eq!(size_from(BudgetSize::Bytes(256 * MIB), Some(reading(8 * GIB, 6 * GIB)), 0), 256 * MIB);
    assert_eq!(size_from(BudgetSize::Bytes(256 * MIB), Some(reading(8 * GIB, 100 * MIB)), 0), 100 * MIB);
    assert_eq!(size_from(BudgetSize::Bytes(256 * MIB), None, 0), 256 * MIB);
    assert_eq!(size_from(BudgetSize::Bytes(MIB), Some(reading(8 * GIB, 0)), 0), MIB, "a small size stays small");
    assert_eq!(size_from(BudgetSize::Fraction(f64::NAN), Some(reading(8 * GIB, 8 * GIB)), 0), GIB);
}

#[test]
fn the_budget_shrinks_under_memory_pressure_and_grows_back() {
    let host = Arc::new(Injected(Mutex::new(reading(8 * GIB, 6 * GIB))));
    let budget = MemoryBudget::new(BudgetSize::default(), Box::new(host.clone()));
    assert_eq!(budget.snapshot().budget, 768 * MIB);
    *host.0.lock().unwrap() = reading(8 * GIB, 800 * MIB);
    assert_eq!(budget.refresh(), 100 * MIB, "pressure: an eighth of what is left");
    *host.0.lock().unwrap() = reading(8 * GIB, 6 * GIB);
    assert_eq!(budget.refresh(), 768 * MIB, "and back");
    assert_eq!(budget.snapshot().reading, Some(reading(8 * GIB, 6 * GIB)));
}

#[test]
fn each_reader_s_share_is_the_budget_over_its_readers_never_below_one_group() {
    let budget = MemoryBudget::new(BudgetSize::Bytes(64 * MIB), Box::new(Fixed(reading(8 * GIB, 8 * GIB))));
    assert_eq!(budget.share(), 64 << 20, "no reader yet: all of it");
    let queues: Vec<_> = (0..16).map(|_| ConsumerQueue::pooled(RECORDER, budget.clone())).collect();
    assert_eq!(budget.share(), 4 << 20);
    assert_eq!(budget.snapshot().readers, 16);
    drop(queues);
    assert_eq!(budget.snapshot().readers, 0, "a reader gone is no longer counted");
}

#[test]
fn many_stalled_readers_never_hold_more_than_the_budget() {
    let budget = MemoryBudget::new(BudgetSize::Bytes(MIN_BUDGET), Box::new(Fixed(reading(8 * GIB, 8 * GIB))));
    // 20 readers of `queue=recorder` (32 MiB each alone), a 4 Mbit/s camera
    // with 2 s groups (1 MB), none of them reading.
    let queues: Vec<_> = (0..20).map(|_| ConsumerQueue::pooled(RECORDER, budget.clone())).collect();
    let now = Instant::now();
    let group = 1 << 20;
    for n in 0..2_000 {
        let key = n % 50 == 0;
        for queue in &queues {
            queue.push_within(frame(key, 20_000), Some(0), group, now);
        }
        assert!(budget.snapshot().held <= MIN_BUDGET, "frame {n}: {:?}", budget.snapshot());
    }
    // Alone, they would hold 20 x 32 MiB.
    assert!(budget.snapshot().held > MIN_BUDGET / 2, "{:?}", budget.snapshot());
    drop(queues);
    assert_eq!(budget.snapshot().held, 0, "a queue gone gives its bytes back");
}

#[test]
fn when_the_budget_is_full_the_reader_furthest_behind_skips_and_a_reader_that_keeps_up_never() {
    let budget = MemoryBudget::new(BudgetSize::Bytes(MIN_BUDGET), Box::new(Fixed(reading(8 * GIB, 8 * GIB))));
    // Recorders whose stream's groups (12 MiB) are larger than their share
    // (8 MiB): together they would hold more than the budget.
    let stalled: Vec<_> = (0..3).map(|_| ConsumerQueue::pooled(RECORDER, budget.clone())).collect();
    let reader = ConsumerQueue::pooled(LIVE, budget.clone());
    let start = Instant::now();
    let mut lost_by_reader = 0;
    for n in 0..3_000u64 {
        let now = start + Duration::from_millis(n * 10);
        let key = n % 100 == 0;
        // The first stalled reader started earliest: it is the furthest behind.
        for queue in stalled.iter().chain([&reader]) {
            queue.push_within(frame(key, 30_000), Some(0), 12 << 20, now);
        }
        // The reader that keeps up takes what came.
        while let Some(Some(next)) = futures_util::FutureExt::now_or_never(reader.recv()) {
            if next.is_err() {
                lost_by_reader += 1;
            }
        }
    }
    assert_eq!(lost_by_reader, 0, "a reader that keeps up never skips");
    assert!(budget.snapshot().skips > 0, "{:?}", budget.snapshot());
    assert!(budget.snapshot().held <= MIN_BUDGET);
    // Each stalled one lost frames, and resumes at a keyframe.
    for queue in &stalled {
        let first = futures_util::FutureExt::now_or_never(queue.recv()).flatten();
        assert!(matches!(first, Some(Err(_))), "{first:?}");
    }
}

#[test]
fn a_queue_with_no_pool_keeps_its_class_s_bound() {
    let queue = ConsumerQueue::new(QueueBudget { max_bytes: 1 << 20, max_age: Duration::from_secs(60) });
    let now = Instant::now();
    for n in 0..100 {
        queue.push_within(frame(n % 10 == 0, 50_000), Some(0), 0, now);
    }
    assert!(queue.waiting_since().is_some());
}

#[test]
fn bytes_held_outside_a_queue_are_counted_and_given_back() {
    let budget = MemoryBudget::new(BudgetSize::Bytes(64 * MIB), Box::new(Fixed(reading(4 * GIB, 2 * GIB))));
    let mut window = budget.hold();
    window.set(10 << 20);
    assert_eq!(budget.snapshot().held, 10 * MIB);
    window.set(4 << 20);
    assert_eq!(budget.snapshot().held, 4 * MIB);
    assert_eq!(window.share(), 64 << 20, "no reader: the whole budget");
    let other = budget.hold();
    drop(window);
    assert_eq!(budget.snapshot().held, 0);
    drop(other);
    assert_eq!(budget.snapshot().held, 0);
}
