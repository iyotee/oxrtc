#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing, clippy::panic)]

use std::sync::Arc;

use bytes::Bytes;

use super::*;
use crate::media::{Codec, TrackInfo};

const VIDEO: Option<u8> = Some(0);

fn frame(track: u8, key: bool, len: usize) -> Item {
    Item::Frame(Arc::new(Frame::new(track, 0, key, Bytes::from(vec![0; len]))))
}

fn tracks() -> Item {
    Item::Tracks(vec![TrackInfo::video(Codec::H264)].into())
}

fn budget(max_bytes: usize, max_age: Duration) -> QueueBudget {
    QueueBudget { max_bytes, max_age }
}

/// What the consumer reads now, without waiting: `Err(n)` for a loss,
/// `Ok((keyframe, len))` for a frame, `Ok((false, 0))` for a track list.
fn drain(queue: &ConsumerQueue) -> Vec<Result<(bool, usize), u64>> {
    let mut out = Vec::new();
    while let Some(Some(next)) = futures_util::FutureExt::now_or_never(queue.recv()) {
        out.push(next.map(|item| match item {
            Item::Frame(f) => (f.keyframe, f.data.len()),
            Item::Tracks(_) => (false, 0),
        }));
    }
    out
}

#[test]
fn within_its_budget_a_queue_loses_nothing() {
    let queue = ConsumerQueue::new(budget(1 << 20, Duration::from_secs(10)));
    let now = Instant::now();
    for i in 0..1000 {
        assert_eq!(queue.push(frame(0, i % 50 == 0, 500), VIDEO, now), 0);
    }
    let read = drain(&queue);
    assert_eq!(read.len(), 1000, "a thousand items is far beyond the old ring of 256, and within 1 MiB");
    assert!(read.iter().all(Result::is_ok));
}

#[test]
fn over_its_bytes_a_queue_resumes_at_its_latest_keyframe() {
    let item = 1000 + ITEM_OVERHEAD;
    let queue = ConsumerQueue::new(budget(10 * item, Duration::from_secs(10)));
    let now = Instant::now();
    // K d d d d K d d d d: full.
    for i in 0..10 {
        queue.push(frame(0, i % 5 == 0, 1000), VIDEO, now);
    }
    // One more: the first GOP goes, the second stays.
    assert_eq!(queue.push(frame(0, false, 1000), VIDEO, now), 5);
    let read = drain(&queue);
    assert_eq!(read[0], Err(5), "the consumer is told first");
    assert_eq!(read[1], Ok((true, 1000)), "and resumes at a keyframe");
    assert_eq!(read.len(), 1 + 6);
}

#[test]
fn a_queue_whose_oldest_item_waited_too_long_resumes_at_its_latest_keyframe() {
    let queue = ConsumerQueue::new(budget(usize::MAX, Duration::from_secs(2)));
    let start = Instant::now();
    for i in 0..20u64 {
        // 10 frames a second, a keyframe each second.
        queue.push(frame(0, i % 10 == 0, 100), VIDEO, start + Duration::from_millis(i * 100));
    }
    // The oldest waited 2.5 s: the frames before the keyframe at 1 s go.
    let dropped = queue.push(frame(0, false, 100), VIDEO, start + Duration::from_millis(2500));
    assert_eq!(dropped, 10);
    let read = drain(&queue);
    assert_eq!((read[0], read[1]), (Err(10), Ok((true, 100))));
}

#[test]
fn a_keyframe_over_the_budget_replaces_everything_queued() {
    let queue = ConsumerQueue::new(budget(4 * (100 + ITEM_OVERHEAD), Duration::from_secs(10)));
    let now = Instant::now();
    for i in 0..4 {
        queue.push(frame(0, i == 0, 100), VIDEO, now);
    }
    assert_eq!(queue.push(frame(0, true, 100), VIDEO, now), 4);
    assert_eq!(drain(&queue), vec![Err(4), Ok((true, 100))]);
}

#[test]
fn without_a_keyframe_to_resume_at_everything_queued_goes() {
    let queue = ConsumerQueue::new(budget(4 * (100 + ITEM_OVERHEAD), Duration::from_secs(10)));
    let now = Instant::now();
    for _ in 0..4 {
        queue.push(frame(0, false, 100), VIDEO, now);
    }
    assert_eq!(queue.push(frame(0, false, 100), VIDEO, now), 4);
    // The delta that came in is passed on: the subscription, told of the
    // loss, waits for a keyframe before it lets video through.
    assert_eq!(drain(&queue), vec![Err(4), Ok((false, 100))]);
}

#[test]
fn a_gop_that_cannot_fit_is_dropped_whole() {
    let item = 100 + ITEM_OVERHEAD;
    let queue = ConsumerQueue::new(budget(4 * item, Duration::from_secs(10)));
    let now = Instant::now();
    // K d d d: the keyframe is the oldest, and what follows it is the whole
    // budget, so keeping from it would not make room.
    for i in 0..4 {
        queue.push(frame(0, i == 0, 100), VIDEO, now);
    }
    assert_eq!(queue.push(frame(0, false, 100), VIDEO, now), 4);
}

#[test]
fn a_source_without_video_loses_its_oldest_frames_only() {
    let item = 100 + ITEM_OVERHEAD;
    let queue = ConsumerQueue::new(budget(10 * item, Duration::from_secs(10)));
    let now = Instant::now();
    for _ in 0..10 {
        queue.push(frame(0, true, 100), None, now);
    }
    assert_eq!(queue.push(frame(0, true, 100), None, now), 1);
    let read = drain(&queue);
    assert_eq!(read[0], Err(1));
    assert_eq!(read.len(), 1 + 10);
}

#[test]
fn a_track_list_is_never_dropped() {
    let item = 100 + ITEM_OVERHEAD;
    let queue = ConsumerQueue::new(budget(6 * item, Duration::from_secs(10)));
    let now = Instant::now();
    queue.push(tracks(), VIDEO, now);
    for i in 0..5 {
        queue.push(frame(0, i == 2, 100), VIDEO, now);
    }
    queue.push(frame(0, false, 100), VIDEO, now);
    let read = drain(&queue);
    // The loss, the track list the kept frames name, then the keyframe.
    assert_eq!(read[0], Err(2));
    assert_eq!(read[1], Ok((false, 0)));
    assert_eq!(read[2], Ok((true, 100)));
}

#[test]
fn tiny_frames_are_bounded_by_their_overhead() {
    let queue = ConsumerQueue::new(budget(64 * 1024, Duration::from_secs(3600)));
    let now = Instant::now();
    for _ in 0..1_000_000 {
        queue.push(frame(0, true, 1), None, now);
    }
    let (items, bytes) = queue.queued();
    assert!(bytes <= 64 * 1024, "{bytes} bytes queued");
    assert!(items <= 64 * 1024 / ITEM_OVERHEAD, "{items} one-byte frames queued");
}

#[test]
fn a_closed_queue_hands_over_what_it_holds_then_ends() {
    let queue = ConsumerQueue::new(budget(1 << 20, Duration::from_secs(10)));
    let now = Instant::now();
    queue.push(frame(0, true, 10), VIDEO, now);
    queue.close();
    assert_eq!(queue.push(frame(0, true, 10), VIDEO, now), 0);
    assert_eq!(drain(&queue), vec![Ok((true, 10))]);
    assert!(futures_util::FutureExt::now_or_never(queue.recv()).unwrap().is_none());
}

#[tokio::test]
async fn a_waiting_consumer_is_woken_by_a_push() {
    let queue = Arc::new(ConsumerQueue::new(budget(1 << 20, Duration::from_secs(10))));
    let reader = tokio::spawn({
        let queue = queue.clone();
        async move { queue.recv().await.map(|r| r.is_ok()) }
    });
    tokio::task::yield_now().await;
    queue.push(frame(0, true, 10), VIDEO, Instant::now());
    assert_eq!(tokio::time::timeout(Duration::from_secs(1), reader).await.unwrap().unwrap(), Some(true));
}
