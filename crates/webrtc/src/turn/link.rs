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

/// What ends a connection: its writer, and the signal that it is over.
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
        if let Some(writer) = self.writer.take() {
            let _ = tokio::time::timeout(FLUSH_TIMEOUT, writer).await;
        }
    }
}

impl Drop for Closing {
    fn drop(&mut self) {
        if let Some(over) = &self.over {
            over.cancel();
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
