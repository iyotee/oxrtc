//! The bounded write side of a connection: what is on its way to the
//! socket, not a backlog. Responses wait for room. Media is taken from the
//! reader's subscription only while there is room ([`Outbox::has_room`]),
//! so a reader that does not read leaves its frames in the subscription's
//! queue, whose budget is the recorder's (`oxrtc_core::queue`): bounded in
//! bytes and in time, deep enough for a pause of seconds, and dropping to a
//! keyframe, logged, beyond it. The outbox itself never holds more than its
//! bound and one frame.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use bytes::Bytes;
use oxrtc_core::CancellationToken;
use tokio::io::{AsyncWrite, AsyncWriteExt};
use tokio::sync::{Notify, mpsc};

/// Items (responses, or all the packets of one frame) queued per connection.
pub(crate) const QUEUE_ITEMS: usize = 256;
/// Bytes of media on their way to the socket, beyond which the connection
/// takes no more from its subscription until some are written.
pub(crate) const QUEUE_BYTES: usize = 1024 * 1024;
/// Places kept free for responses: media is taken only while more remain.
pub(crate) const RESPONSE_ROOM: usize = 8;

/// The sending half, owned by the connection loop.
#[derive(Debug, Clone)]
pub(crate) struct Outbox {
    tx: mpsc::Sender<Bytes>,
    queued: Arc<AtomicUsize>,
    written: Arc<Notify>,
    max_bytes: usize,
}

/// The receiving half, owned by the writer task.
#[derive(Debug)]
pub(crate) struct Inbox {
    rx: mpsc::Receiver<Bytes>,
    queued: Arc<AtomicUsize>,
    written: Arc<Notify>,
}

#[cfg(test)]
impl Inbox {
    /// The next queued item, without waiting.
    pub(crate) fn try_recv_for_test(&mut self) -> Option<Bytes> {
        self.rx.try_recv().ok()
    }
}

/// A queue of `items` entries and `max_bytes` bytes of media.
pub(crate) fn outbox(items: usize, max_bytes: usize) -> (Outbox, Inbox) {
    let (tx, rx) = mpsc::channel(items.max(1));
    let queued = Arc::new(AtomicUsize::new(0));
    let written = Arc::new(Notify::new());
    (Outbox { tx, queued: queued.clone(), written: written.clone(), max_bytes }, Inbox { rx, queued, written })
}

impl Outbox {
    /// Whether to take another frame from the subscription: fewer bytes
    /// queued than the bound, and places left beyond those kept for
    /// responses.
    pub(crate) fn has_room(&self) -> bool {
        self.queued.load(Ordering::Acquire) < self.max_bytes && self.tx.capacity() > RESPONSE_ROOM
    }

    /// Resolves after the writer wrote an item (at once, at most once, if it
    /// wrote one since the last call): when [`Outbox::has_room`] may have
    /// changed.
    pub(crate) async fn written(&self) {
        self.written.notified().await;
    }

    /// Queues media below the byte bound, whatever its size, if a place is
    /// left. `false`: refused, the caller drops it.
    ///
    /// Taking a frame of any size below the bound means that a frame taken
    /// because [`Outbox::has_room`] said so is never refused for its size
    /// (a keyframe bigger than the room left, up to the core's
    /// `Limits::max_frame_bytes`): memory stays bounded by the bound plus
    /// one frame.
    pub(crate) fn try_media(&self, data: Bytes) -> bool {
        let len = data.len();
        let before = self.queued.fetch_add(len, Ordering::AcqRel);
        if before >= self.max_bytes {
            self.queued.fetch_sub(len, Ordering::AcqRel);
            return false;
        }
        match self.tx.try_send(data) {
            Ok(()) => true,
            Err(_) => {
                self.queued.fetch_sub(len, Ordering::AcqRel);
                false
            }
        }
    }

    /// Queues a response, waiting up to `wait` for room. `false`: the writer is
    /// gone or stuck.
    pub(crate) async fn send(&self, data: Bytes, wait: Duration) -> bool {
        let len = data.len();
        self.queued.fetch_add(len, Ordering::AcqRel);
        let sent = matches!(tokio::time::timeout(wait, self.tx.send(data)).await, Ok(Ok(())));
        if !sent {
            self.queued.fetch_sub(len, Ordering::AcqRel);
        }
        sent
    }

    /// Bytes queued and not yet written.
    #[cfg(test)]
    pub(crate) fn queued(&self) -> usize {
        self.queued.load(Ordering::Acquire)
    }
}

/// Writes queued items until the queue closes, a write fails, or one write
/// takes longer than `write_timeout`. Cancels `done` on the way out.
pub(crate) async fn write_loop<W: AsyncWrite + Unpin>(
    mut inbox: Inbox,
    mut writer: W,
    write_timeout: Duration,
    done: CancellationToken,
) {
    while let Some(data) = inbox.rx.recv().await {
        let len = data.len();
        let written = tokio::time::timeout(write_timeout, writer.write_all(&data)).await;
        inbox.queued.fetch_sub(len, Ordering::AcqRel);
        inbox.written.notify_one();
        if !matches!(written, Ok(Ok(()))) {
            break;
        }
    }
    let _ = writer.shutdown().await;
    done.cancel();
}

#[cfg(test)]
mod tests {
    use tokio::io::AsyncReadExt;

    use super::*;

    #[test]
    fn media_is_refused_past_the_item_bound() {
        let (out, _inbox) = outbox(4, usize::MAX);
        let accepted = (0..10).filter(|_| out.try_media(Bytes::from_static(b"x"))).count();
        assert_eq!(accepted, 4);
    }

    #[test]
    fn media_is_refused_past_the_byte_bound() {
        let (out, _inbox) = outbox(100, 1000);
        let accepted = (0..10).filter(|_| out.try_media(Bytes::from(vec![0; 300]))).count();
        // 0, 300, 600, 900 are below the bound: the fourth is taken whole.
        assert_eq!(accepted, 4);
        assert_eq!(out.queued(), 1200);
        assert!(!out.has_room());
        assert!(!out.try_media(Bytes::from(vec![0; 1])));
    }

    #[test]
    fn a_frame_larger_than_the_room_left_is_taken_whole_below_the_bound() {
        let (out, mut inbox) = outbox(100, 1000);
        assert!(out.try_media(Bytes::from(vec![0; 10])));
        assert!(out.has_room());
        // A keyframe bigger than the bound, taken because there was room:
        // refusing it would drop the frame a reader waits for.
        assert!(out.try_media(Bytes::from(vec![0; 5000])));
        assert!(!out.has_room());
        assert!(!out.try_media(Bytes::from(vec![0; 1])), "nothing behind it");
        while let Some(drained) = inbox.try_recv_for_test() {
            inbox.queued.fetch_sub(drained.len(), Ordering::AcqRel);
        }
        assert!(out.has_room());
    }

    #[test]
    fn places_are_kept_for_responses() {
        let (out, _inbox) = outbox(RESPONSE_ROOM + 2, usize::MAX);
        assert!(out.try_media(Bytes::from_static(b"a")));
        assert!(out.has_room());
        assert!(out.try_media(Bytes::from_static(b"b")));
        assert!(!out.has_room(), "the places left are the responses'");
    }

    #[tokio::test]
    async fn a_write_says_there_may_be_room_again() {
        let (mut client, server) = tokio::io::duplex(1 << 16);
        let (out, inbox) = outbox(16, 100);
        tokio::spawn(write_loop(inbox, server, Duration::from_secs(5), CancellationToken::new()));
        assert!(out.try_media(Bytes::from(vec![1; 500])));
        assert!(!out.has_room());
        tokio::time::timeout(Duration::from_secs(1), out.written()).await.unwrap();
        let mut got = vec![0; 500];
        client.read_exact(&mut got).await.unwrap();
        assert!(out.has_room());
    }

    #[tokio::test]
    async fn a_slow_reader_costs_frames_not_memory() {
        // A pipe of 64 bytes the reader never drains.
        let (client, server) = tokio::io::duplex(64);
        let (out, inbox) = outbox(8, 4096);
        let done = CancellationToken::new();
        tokio::spawn(write_loop(inbox, server, Duration::from_secs(30), done.clone()));
        let mut accepted = 0;
        for _ in 0..1000 {
            if out.try_media(Bytes::from(vec![7; 500])) {
                accepted += 1;
            }
            tokio::task::yield_now().await;
        }
        assert!(accepted <= 8 + 1, "accepted {accepted}");
        // The bound, and the one frame taken below it.
        assert!(out.queued() <= 4096 + 500, "{} bytes queued", out.queued());
        drop(client);
    }

    #[tokio::test]
    async fn written_bytes_are_released() {
        let (mut client, server) = tokio::io::duplex(1 << 16);
        let (out, inbox) = outbox(8, 4096);
        let done = CancellationToken::new();
        tokio::spawn(write_loop(inbox, server, Duration::from_secs(5), done.clone()));
        assert!(out.send(Bytes::from_static(b"RTSP/1.0 200 OK\r\n\r\n"), Duration::from_secs(1)).await);
        assert!(out.try_media(Bytes::from(vec![1; 1000])));
        let mut got = vec![0; 19 + 1000];
        client.read_exact(&mut got).await.unwrap();
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert_eq!(out.queued(), 0);
        drop(out);
        tokio::time::timeout(Duration::from_secs(1), done.cancelled()).await.unwrap();
    }
}
