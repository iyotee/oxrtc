//! ICE-TCP (RFC 6544): passive host candidates on a TCP listener, for a
//! viewer whose network lets no UDP through. Each connection carries one
//! peer's STUN, DTLS and SRTP, framed by a 16-bit length (RFC 4571); str0m
//! does ICE over it as over UDP, and the framing is ours.
//!
//! Bounded like the UDP side: connections in all (`max_sessions`) and per
//! address (`max_sessions_per_ip`, loopback included, D9), a connection that
//! no session claims within [`CLAIM_TIMEOUT`] is closed, each one's send
//! queue holds [`SEND_QUEUE`] frames (beyond, frames are dropped, as a full
//! UDP buffer would), and a connection ends with the session that claimed it.

use std::collections::HashMap;
use std::io;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use oxrtc_core::{CancellationToken, PerIpLimit, PerIpPermit, Refusals};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

/// A connection no session has taken by then is closed.
pub(crate) const CLAIM_TIMEOUT: Duration = Duration::from_secs(10);
/// Frames waiting to be written on one connection.
pub(crate) const SEND_QUEUE: usize = 256;

/// Hands a frame to the sessions: `(data, peer, local address)`, the
/// session that took it.
pub(crate) type Dispatch = Arc<dyn Fn(&[u8], SocketAddr, SocketAddr) -> Option<u64> + Send + Sync>;

struct Conn {
    /// Frames with their length in front.
    frames: mpsc::Sender<Vec<u8>>,
    cancel: CancellationToken,
    owner: Option<u64>,
}

/// The ICE-TCP listener's connections.
pub(crate) struct TcpEgress {
    local: SocketAddr,
    conns: Mutex<HashMap<SocketAddr, Conn>>,
    max: usize,
    per_ip: PerIpLimit,
    /// Connections refused at `max`, reported once per interval.
    full: Refusals,
    claim_timeout: Duration,
}

impl TcpEgress {
    /// Binds the listener; the caller spawns [`TcpEgress::accept_loop`].
    pub(crate) async fn bind(
        listen: SocketAddr,
        max: usize,
        max_per_ip: usize,
    ) -> io::Result<(Arc<Self>, TcpListener)> {
        Self::bind_with(listen, max, max_per_ip, CLAIM_TIMEOUT).await
    }

    async fn bind_with(
        listen: SocketAddr,
        max: usize,
        max_per_ip: usize,
        claim_timeout: Duration,
    ) -> io::Result<(Arc<Self>, TcpListener)> {
        let listener = TcpListener::bind(listen).await?;
        let local = listener.local_addr()?;
        let conns = Mutex::new(HashMap::new());
        let per_ip = PerIpLimit::new(max_per_ip);
        let egress = Arc::new(Self { local, conns, max, per_ip, full: Refusals::new(), claim_timeout });
        Ok((egress, listener))
    }

    pub(crate) fn local_addr(&self) -> SocketAddr {
        self.local
    }

    fn conns(&self) -> MutexGuard<'_, HashMap<SocketAddr, Conn>> {
        self.conns.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Connections open now.
    #[cfg(test)]
    fn len(&self) -> usize {
        self.conns().len()
    }

    /// Queues a frame for the connection from `remote`; dropped when there
    /// is none, or its queue is full.
    pub(crate) fn send(&self, remote: SocketAddr, data: &[u8]) {
        let Ok(length) = u16::try_from(data.len()) else { return };
        if let Some(conn) = self.conns().get(&remote) {
            let mut framed = Vec::with_capacity(2 + data.len());
            framed.extend_from_slice(&length.to_be_bytes());
            framed.extend_from_slice(data);
            let _ = conn.frames.try_send(framed);
        }
    }

    /// Closes the connections the session `id` claimed.
    pub(crate) fn release(&self, id: u64) {
        for conn in self.conns().values().filter(|c| c.owner == Some(id)) {
            conn.cancel.cancel();
        }
    }

    fn claim(&self, remote: SocketAddr, id: u64) {
        if let Some(conn) = self.conns().get_mut(&remote) {
            conn.owner.get_or_insert(id);
        }
    }

    pub(crate) async fn accept_loop(
        self: Arc<Self>,
        listener: TcpListener,
        dispatch: Dispatch,
        cancel: CancellationToken,
    ) {
        info!(local = %self.local, "webrtc tcp listening");
        loop {
            let accepted = tokio::select! {
                () = cancel.cancelled() => break,
                accepted = listener.accept() => accepted,
            };
            let (stream, remote) = match accepted {
                Ok(pair) => pair,
                Err(error) => {
                    debug!(%error, "tcp accept failed");
                    tokio::time::sleep(Duration::from_millis(10)).await;
                    continue;
                }
            };
            let Some(permit) = self.admit(remote) else { continue };
            let conn_cancel = cancel.child_token();
            let (frames, queue) = mpsc::channel(SEND_QUEUE);
            self.conns().insert(remote, Conn { frames, cancel: conn_cancel.clone(), owner: None });
            tokio::spawn(self.clone().serve(stream, remote, queue, dispatch.clone(), conn_cancel, permit));
        }
        debug!("webrtc tcp listener stopped");
    }

    /// A place for a connection from `remote`, within both bounds.
    fn admit(&self, remote: SocketAddr) -> Option<PerIpPermit> {
        let conns = self.conns();
        if conns.len() >= self.max {
            drop(conns);
            // Like the per-address refusals: one warning per interval.
            if let Some(refused) = self.full.refused() {
                warn!(%remote, limit = self.max, refused, "tcp connection limit reached");
            }
            return None;
        }
        if conns.contains_key(&remote) {
            debug!(%remote, "tcp connection refused: one from that address and port already");
            return None;
        }
        drop(conns);
        let permit = self.per_ip.try_acquire(remote.ip());
        if permit.is_none()
            && let Some(refused) = self.per_ip.refused()
        {
            warn!(%remote, limit = self.per_ip.max_per_ip(), refused, "per-address tcp connection limit reached");
        }
        permit
    }

    async fn serve(
        self: Arc<Self>,
        stream: TcpStream,
        remote: SocketAddr,
        mut queue: mpsc::Receiver<Vec<u8>>,
        dispatch: Dispatch,
        cancel: CancellationToken,
        _permit: PerIpPermit,
    ) {
        let local = stream.local_addr().unwrap_or(self.local);
        let _ = stream.set_nodelay(true);
        let (mut reader, mut writer) = stream.into_split();
        let read = async {
            let mut claimed = false;
            let mut buf = vec![0u8; usize::from(u16::MAX)];
            let claim_by = tokio::time::Instant::now() + self.claim_timeout;
            loop {
                let mut length = [0u8; 2];
                let frame = async {
                    reader.read_exact(&mut length).await?;
                    let len = usize::from(u16::from_be_bytes(length));
                    let slot = buf.get_mut(..len).ok_or_else(|| io::Error::other("frame too long"))?;
                    reader.read_exact(slot).await?;
                    io::Result::Ok(len)
                };
                let len = if claimed {
                    frame.await
                } else {
                    match tokio::time::timeout_at(claim_by, frame).await {
                        Ok(read) => read,
                        Err(_) => return "no session claimed the connection",
                    }
                };
                let Ok(len) = len else { return "closed by the peer" };
                let Some(data) = buf.get(..len) else { continue };
                if let Some(id) = dispatch(data, remote, local)
                    && !claimed
                {
                    claimed = true;
                    self.claim(remote, id);
                }
            }
        };
        let write = async {
            // Each frame is queued with its length in front.
            while let Some(framed) = queue.recv().await {
                if writer.write_all(&framed).await.is_err() {
                    break;
                }
            }
        };
        let reason = tokio::select! {
            () = cancel.cancelled() => "session ended",
            reason = read => reason,
            () = write => "write failed",
        };
        self.conns().remove(&remote);
        debug!(%remote, reason, "webrtc tcp connection closed");
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    async fn egress(claim: Duration, owner: Option<u64>) -> (Arc<TcpEgress>, CancellationToken) {
        let (egress, listener) = TcpEgress::bind_with("127.0.0.1:0".parse().unwrap(), 4, 4, claim).await.unwrap();
        let cancel = CancellationToken::new();
        let dispatch: Dispatch = Arc::new(move |_, _, _| owner);
        tokio::spawn(egress.clone().accept_loop(listener, dispatch, cancel.clone()));
        (egress, cancel)
    }

    async fn closed(stream: &mut TcpStream) -> bool {
        let mut byte = [0u8; 1];
        matches!(tokio::time::timeout(Duration::from_secs(3), stream.read(&mut byte)).await, Ok(Ok(0) | Err(_)))
    }

    #[tokio::test]
    async fn refusals_at_the_total_go_through_the_throttle() {
        // Four slots, all held: every further connection is refused, and
        // counted once, so the warning is one per interval with a count.
        let (egress, _cancel) = egress(Duration::from_secs(10), Some(1)).await;
        let mut held = Vec::new();
        for _ in 0..4 {
            held.push(TcpStream::connect(egress.local_addr()).await.unwrap());
        }
        assert!(eventually(|| egress.len() == 4).await);
        for port in 1..=10u16 {
            assert!(egress.admit(SocketAddr::from(([127, 0, 0, 1], port))).is_none());
        }
        let later = std::time::Instant::now() + oxrtc_core::refusals::REPORT_EVERY * 2;
        // The first refusal was reported at once; the nine after it wait.
        assert_eq!(egress.full.refused_at(later), Some(10));
    }

    async fn eventually(check: impl Fn() -> bool) -> bool {
        for _ in 0..100 {
            if check() {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        false
    }

    #[tokio::test]
    async fn a_connection_no_session_claims_is_closed() {
        let (egress, _cancel) = egress(Duration::from_millis(200), None).await;
        let mut stream = TcpStream::connect(egress.local_addr()).await.unwrap();
        // A frame nobody takes does not keep it open.
        stream.write_all(&[0, 3, 1, 2, 3]).await.unwrap();
        assert!(closed(&mut stream).await);
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(egress.len(), 0);
    }

    #[tokio::test]
    async fn a_claimed_connection_carries_frames_and_ends_with_its_session() {
        let (egress, _cancel) = egress(Duration::from_millis(200), Some(7)).await;
        let mut stream = TcpStream::connect(egress.local_addr()).await.unwrap();
        stream.write_all(&[0, 1, 9]).await.unwrap();
        tokio::time::sleep(Duration::from_millis(400)).await;
        assert_eq!(egress.len(), 1, "claimed: kept past the claim timeout");
        let remote = stream.local_addr().unwrap();
        egress.send(remote, b"abc");
        let mut framed = [0u8; 5];
        stream.read_exact(&mut framed).await.unwrap();
        assert_eq!(framed, [0, 3, b'a', b'b', b'c']);
        egress.release(8);
        egress.release(7);
        assert!(closed(&mut stream).await);
    }
}
