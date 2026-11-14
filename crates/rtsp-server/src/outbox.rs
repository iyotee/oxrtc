//! The bounded write side of a connection. Responses wait for room; media is
//! offered and refused when the queue is full, so a slow reader costs frames,
//! never memory.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use bytes::Bytes;
use oxrtc_core::CancellationToken;
use tokio::io::{AsyncWrite, AsyncWriteExt};
use tokio::sync::mpsc;

/// Items (responses, or all the packets of one frame) queued per connection.
pub(crate) const QUEUE_ITEMS: usize = 256;
/// Bytes of media queued per connection.
pub(crate) const QUEUE_BYTES: usize = 4 * 1024 * 1024;

/// The sending half, owned by the connection loop.
#[derive(Debug, Clone)]
pub(crate) struct Outbox {
    tx: mpsc::Sender<Bytes>,
    queued: Arc<AtomicUsize>,
    max_bytes: usize,
}

/// The receiving half, owned by the writer task.
#[derive(Debug)]
pub(crate) struct Inbox {
    rx: mpsc::Receiver<Bytes>,
    queued: Arc<AtomicUsize>,
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
    (Outbox { tx, queued: queued.clone(), max_bytes }, Inbox { rx, queued })
}

impl Outbox {
    /// Queues media if there is room, in items and in bytes. `false`: refused,
    /// the caller drops it.
    ///
    /// An item larger than the byte bound is still taken when nothing is
    /// queued: a keyframe bigger than the bound (the core admits frames up to
    /// `Limits::max_frame_bytes`) would otherwise be refused forever, and a
    /// reader waiting for a keyframe would never get one. Memory stays bounded
    /// by one such item.
    pub(crate) fn try_media(&self, data: Bytes) -> bool {
        let len = data.len();
        let before = self.queued.fetch_add(len, Ordering::AcqRel);
        if before > 0 && before.saturating_add(len) > self.max_bytes {
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
        assert_eq!(accepted, 3);
        assert_eq!(out.queued(), 900);
        assert!(out.try_media(Bytes::from(vec![0; 100])));
    }

    #[test]
    fn an_item_over_the_byte_bound_passes_only_into_an_empty_queue() {
        let (out, mut inbox) = outbox(100, 1000);
        // A keyframe bigger than the bound: refused behind anything queued...
        assert!(out.try_media(Bytes::from(vec![0; 10])));
        assert!(!out.try_media(Bytes::from(vec![0; 5000])));
        // ...taken once the queue has drained, so a reader waiting for a
        // keyframe gets one.
        let drained = inbox.try_recv_for_test().unwrap();
        inbox.queued.fetch_sub(drained.len(), Ordering::AcqRel);
        assert!(out.try_media(Bytes::from(vec![0; 5000])));
        // Nothing else fits behind it.
        assert!(!out.try_media(Bytes::from(vec![0; 1])));
        assert_eq!(out.queued(), 5000);
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
        assert!(out.queued() <= 4096);
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
