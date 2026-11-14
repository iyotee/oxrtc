//! The local RTSP relay of oxrtc.
//!
//! [`RtspServer`] serves the streams of a [`Hub`] to RTSP readers (FFmpeg,
//! OpenCV, a recorder) over RTP interleaved in the TCP connection, and accepts
//! publications (`ANNOUNCE`/`RECORD`) from oxrtc's own transcoders on
//! one-time path tokens handed out by [`Publishers::register`].
//!
//! Reading: `DESCRIBE rtsp://host:port/<stream>?<want>` subscribes to the stream
//! with the go2rtc query notation ([`oxrtc_core::Want::from_query`]), describes
//! the tracks of the subscription (video first), and `PLAY` packetizes every
//! frame to RTP. UDP transports are answered `461 Unsupported Transport`, which
//! makes FFmpeg fall back to TCP.
//!
//! Everything is bounded: the connection count, the request size, the write
//! queue of each reader (a slow reader loses frames up to the next keyframe,
//! it never makes memory grow), the time between `DESCRIBE` and `PLAY`, and the
//! time a connection may stay silent.
#![forbid(unsafe_code)]

mod conn;
mod depacketize;
mod outbox;
mod packetize;
mod publish;
mod reader;
mod recorder;
mod sdp;
mod wire;

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use oxrtc_core::{CancellationToken, Hub};
use tokio::net::TcpListener;
use tokio::sync::Semaphore;
use tokio_util::task::TaskTracker;

pub use publish::{PublishHandle, Publishers};

/// How long [`RtspServer::run`] waits for its connections after cancellation.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(2);

/// Settings of the server.
#[derive(Debug, Clone)]
pub struct RtspServerConfig {
    /// Where to listen.
    pub listen: SocketAddr,
    /// Most connections served at once; further ones are closed on accept.
    pub max_connections: usize,
    /// Longest time a session may stay between `DESCRIBE`/`SETUP` and `PLAY`
    /// (or `ANNOUNCE` and `RECORD`) before it is dropped.
    pub setup_play_timeout: Duration,
    /// A connection that sends nothing (no request, no RTCP) for this long is
    /// closed.
    pub session_timeout: Duration,
    /// Largest request accepted, headers and body together. A bigger one closes
    /// the connection.
    pub max_request_bytes: usize,
}

impl Default for RtspServerConfig {
    fn default() -> Self {
        Self {
            listen: SocketAddr::from(([127, 0, 0, 1], 8554)),
            max_connections: 256,
            setup_play_timeout: Duration::from_secs(5),
            session_timeout: Duration::from_secs(60),
            max_request_bytes: 64 * 1024,
        }
    }
}

/// What every connection shares.
pub(crate) struct Shared {
    pub(crate) hub: Arc<Hub>,
    pub(crate) publishers: Publishers,
    pub(crate) config: RtspServerConfig,
}

/// A bound RTSP server, ready to [`run`](Self::run).
pub struct RtspServer {
    listener: TcpListener,
    local_addr: SocketAddr,
    shared: Arc<Shared>,
}

impl std::fmt::Debug for RtspServer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RtspServer").field("local_addr", &self.local_addr).finish_non_exhaustive()
    }
}

impl RtspServer {
    /// Binds the listening socket. Port 0 picks a free port; see
    /// [`local_addr`](Self::local_addr).
    pub async fn bind(config: RtspServerConfig, hub: Arc<Hub>) -> std::io::Result<Self> {
        let listener = TcpListener::bind(config.listen).await?;
        let local_addr = listener.local_addr()?;
        let publishers = Publishers::new(local_addr);
        let shared = Arc::new(Shared { hub, publishers, config });
        Ok(Self { listener, local_addr, shared })
    }

    /// The address actually bound.
    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    /// The registry of publication tokens. Clonable, usable before and while
    /// [`run`](Self::run) runs.
    pub fn publishers(&self) -> Publishers {
        self.shared.publishers.clone()
    }

    /// Accepts and serves connections until `cancel` fires, then closes them
    /// (waiting a short grace period for their tasks).
    pub async fn run(self, cancel: CancellationToken) -> std::io::Result<()> {
        let limit = Arc::new(Semaphore::new(self.shared.config.max_connections.min(Semaphore::MAX_PERMITS)));
        let tracker = TaskTracker::new();
        loop {
            let accepted = tokio::select! {
                biased;
                () = cancel.cancelled() => break,
                accepted = self.listener.accept() => accepted,
            };
            let (socket, peer) = match accepted {
                Ok(pair) => pair,
                Err(error) => {
                    // Per-connection failures (reset before accept, descriptor
                    // pressure) must not stop the server.
                    tracing::warn!(%error, "rtsp: accept failed");
                    tokio::time::sleep(Duration::from_millis(50)).await;
                    continue;
                }
            };
            let Ok(permit) = limit.clone().try_acquire_owned() else {
                tracing::warn!(%peer, "rtsp: connection limit reached, closing");
                drop(socket);
                continue;
            };
            let _ = socket.set_nodelay(true);
            let shared = self.shared.clone();
            let token = cancel.child_token();
            tracker.spawn(async move {
                conn::serve(socket, peer, shared, token).await;
                drop(permit);
            });
        }
        tracker.close();
        let _ = tokio::time::timeout(SHUTDOWN_GRACE, tracker.wait()).await;
        Ok(())
    }
}
