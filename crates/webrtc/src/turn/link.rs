//! How the TURN client reaches its server (D13, D21): a UDP socket of its
//! own, or one TCP connection (RFC 8656 section 3.1, `?transport=tcp`). The
//! allocation above it, [`super::Allocation`], sees only messages: a
//! datagram is one over UDP; over TCP the stream is cut into messages by
//! `stun::frame_len`. The relayed side is UDP in both.
//!
//! A TCP connection is made for each allocation, so a new process or a new
//! attempt has a new 5-tuple and never meets `437 Allocation Mismatch` with
//! its predecessor's allocation, as with UDP. It ends with the allocation:
//! a write that fails, a server that closes it or sends what is not TURN's
//! ends the allocation, and the driver makes another after the core's
//! jittered backoff.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use oxrtc_core::CancellationToken;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::{TcpStream, UdpSocket};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tracing::debug;

use super::allocation::Failure;
use super::url::{ServerUrl, Transport};
use crate::socket::{ERROR_PAUSE, RECV_BUFFER, bind_udp, per_datagram};
use crate::stun::{Framer, READ};

/// How long a TCP connection to the server may take to open.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
/// Messages waiting to be written on one connection; beyond, they are dropped,
/// as a full UDP buffer would.
pub(crate) const SEND_QUEUE: usize = 256;
/// The longest a closing connection is given to write what is queued (the
/// release of the allocation).
const FLUSH_TIMEOUT: Duration = Duration::from_millis(500);

/// Where an allocation's datagrams go; cloned into what sessions send.
#[derive(Clone)]
pub(crate) enum Link {
    Udp {
        socket: Arc<UdpSocket>,
        server: SocketAddr,
    },
    /// The queue of the connection's writer.
    Tcp(mpsc::Sender<Vec<u8>>),
}

impl Link {
    /// Sends one message.
    pub(crate) async fn send(&self, datagram: Vec<u8>) {
        match self {
            Self::Udp { socket, server } => {
                if let Err(error) = socket.send_to(&datagram, *server).await {
                    debug!(%error, "send to the TURN server failed");
                }
            }
            Self::Tcp(queue) => queue_frame(queue, datagram),
        }
    }

    /// Sends one message without waiting (the release at shutdown).
    pub(crate) fn send_now(&self, datagram: Vec<u8>) {
        match self {
            Self::Udp { socket, server } => {
                let _ = socket.try_send_to(&datagram, *server);
            }
            Self::Tcp(queue) => queue_frame(queue, datagram),
        }
    }
}

fn queue_frame(queue: &mpsc::Sender<Vec<u8>>, frame: Vec<u8>) {
    if queue.try_send(frame).is_err() {
        debug!("the TURN connection's queue is full or closed; a message dropped");
    }
}

/// What the server sends, one message at a time.
pub(crate) enum Wire {
    Udp { socket: Arc<UdpSocket>, server: SocketAddr, buf: Vec<u8> },
    Tcp(Stream),
}

impl Wire {
    /// The next message from the server: `None` for a datagram to ignore (from
    /// another address, or an error that concerns that datagram only), an
    /// error when the connection is over. Cancel safe.
    pub(crate) async fn recv(&mut self) -> Result<Option<&[u8]>, Failure> {
        match self {
            Self::Udp { socket, server, buf } => match socket.recv_from(buf).await {
                Ok((len, from)) if from == *server => Ok(buf.get(..len)),
                // Only the server speaks on this socket.
                Ok(_) => Ok(None),
                Err(error) if per_datagram(&error) => Ok(None),
                Err(error) => {
                    debug!(%error, "receive on the TURN socket failed");
                    tokio::time::sleep(ERROR_PAUSE).await;
                    Ok(None)
                }
            },
            Self::Tcp(stream) => stream.next().await.map(Some),
        }
    }
}

/// The read half of a connection and what it has read of the next messages.
pub(crate) struct Stream {
    half: OwnedReadHalf,
    framer: Framer,
}

impl Stream {
    fn new(half: OwnedReadHalf) -> Self {
        Self { half, framer: Framer::default() }
    }

    /// The next whole message, read as far as it needs. Cancel safe: what was
    /// read stays in the framer.
    async fn next(&mut self) -> Result<&[u8], Failure> {
        loop {
            match self.framer.ready() {
                Err(_) => return Err(Failure::Lost("the server sent what is not TURN")),
                Ok(Some(_)) => break,
                Ok(None) => {}
            }
            let mut chunk = [0u8; READ];
            match self.half.read(&mut chunk).await {
                Ok(0) => return Err(Failure::Lost("the server closed the connection")),
                Ok(n) => self.framer.push(chunk.get(..n).unwrap_or_default()),
                Err(error) => {
                    debug!(%error, "read from the TURN connection failed");
                    return Err(Failure::Lost("the connection to the server failed"));
                }
            }
        }
        Ok(self.framer.frame())
    }
}

/// What ends a connection: its writer, and the signal that it is over. Dropped
/// without [`Closing::finish`], the writer is aborted: the connection failed.
pub(crate) struct Closing {
    over: Option<CancellationToken>,
    writer: Option<JoinHandle<()>>,
}

impl Closing {
    /// Resolves when the connection is lost (a failed write); never over UDP.
    pub(crate) async fn lost(&self) {
        match &self.over {
            Some(over) => over.cancelled().await,
            None => std::future::pending().await,
        }
    }

    /// Ends the connection, after the writer has written what is queued.
    pub(crate) async fn finish(mut self) {
        if let Some(over) = self.over.take() {
            over.cancel();
        }
        if let Some(mut writer) = self.writer.take() {
            // A server that stopped reading leaves the writer in the middle
            // of a write: it would hold the connection open.
            if tokio::time::timeout(FLUSH_TIMEOUT, &mut writer).await.is_err() {
                writer.abort();
            }
        }
    }
}

impl Drop for Closing {
    fn drop(&mut self) {
        if let Some(over) = &self.over {
            over.cancel();
        }
        if let Some(writer) = &self.writer {
            writer.abort();
        }
    }
}

/// Reaches `server`: a UDP socket of the family's own, or a TCP connection.
pub(crate) async fn connect(url: &ServerUrl, server: SocketAddr) -> Result<(Link, Wire, Closing), Failure> {
    match url.transport {
        Transport::Udp => {
            let any =
                if server.is_ipv4() { IpAddr::V4(Ipv4Addr::UNSPECIFIED) } else { IpAddr::V6(Ipv6Addr::UNSPECIFIED) };
            let socket = match bind_udp(SocketAddr::new(any, 0)) {
                Ok(socket) => Arc::new(socket),
                Err(error) => {
                    debug!(%error, "cannot bind a socket for the TURN allocation");
                    return Err(Failure::Lost("no local socket"));
                }
            };
            let link = Link::Udp { socket: socket.clone(), server };
            let wire = Wire::Udp { socket, server, buf: vec![0u8; RECV_BUFFER] };
            Ok((link, wire, Closing { over: None, writer: None }))
        }
        Transport::Tcp => {
            let stream = match tokio::time::timeout(CONNECT_TIMEOUT, TcpStream::connect(server)).await {
                Ok(Ok(stream)) => stream,
                Ok(Err(error)) => {
                    debug!(%error, %url, "cannot connect to the TURN server");
                    return Err(Failure::Lost("cannot connect to the server"));
                }
                Err(_) => return Err(Failure::Lost("the connection to the server timed out")),
            };
            let _ = stream.set_nodelay(true);
            let (reader, writer) = stream.into_split();
            let (frames, queue) = mpsc::channel(SEND_QUEUE);
            let over = CancellationToken::new();
            let writer = tokio::spawn(write_all_frames(writer, queue, over.clone()));
            Ok((Link::Tcp(frames), Wire::Tcp(Stream::new(reader)), Closing { over: Some(over), writer: Some(writer) }))
        }
    }
}

/// Writes the queued messages in order until `over`; then what is still
/// queued (the release), briefly. A write that fails ends the connection.
async fn write_all_frames(mut half: OwnedWriteHalf, mut queue: mpsc::Receiver<Vec<u8>>, over: CancellationToken) {
    loop {
        tokio::select! {
            biased;
            () = over.cancelled() => break,
            frame = queue.recv() => match frame {
                Some(frame) => {
                    if half.write_all(&frame).await.is_err() {
                        over.cancel();
                        return;
                    }
                }
                None => return,
            },
        }
    }
    let flush = async {
        while let Ok(frame) = queue.try_recv() {
            if half.write_all(&frame).await.is_err() {
                return;
            }
        }
        let _ = half.shutdown().await;
    };
    let _ = tokio::time::timeout(FLUSH_TIMEOUT, flush).await;
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::indexing_slicing)]

    use tokio::net::TcpListener;

    use super::super::url::parse;
    use super::*;

    /// A server that takes the connection and stops reading: the writer is
    /// stuck in the middle of a write, with a full queue behind it. Ending
    /// the connection, by failure or by shutdown, must end the writer, which
    /// holds the socket open otherwise (until the kernel gives up on it).
    async fn stuck_writer() -> (Link, Wire, Closing, tokio::net::TcpStream, tokio::task::AbortHandle) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let url = parse(&format!("turn:{addr}?transport=tcp")).unwrap();
        let (link, wire, closing) = connect(&url, addr).await.unwrap();
        let (peer, _) = listener.accept().await.unwrap();
        let Link::Tcp(queue) = &link else { panic!("a TCP link") };
        // More than the two socket buffers hold, 64 kB a message.
        let started = tokio::time::Instant::now();
        while queue.capacity() > 0 {
            link.send_now(vec![0u8; 64 * 1024]);
            tokio::time::sleep(Duration::from_millis(2)).await;
            assert!(started.elapsed() < Duration::from_secs(20), "the writer never got stuck");
        }
        let writer = closing.writer.as_ref().unwrap().abort_handle();
        (link, wire, closing, peer, writer)
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_failed_allocation_does_not_leave_its_writer_stuck() {
        let (link, wire, closing, _peer, writer) = stuck_writer().await;
        assert!(!writer.is_finished());
        drop((link, wire, closing));
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(writer.is_finished(), "the writer still holds the connection");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_shutdown_does_not_leave_its_writer_stuck() {
        let (link, wire, closing, _peer, writer) = stuck_writer().await;
        closing.finish().await;
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(writer.is_finished(), "the writer still holds the connection");
        drop((link, wire));
    }
}
