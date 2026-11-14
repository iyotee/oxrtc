//! Serving the router with a bounded number of open connections.

use std::future::{Future, IntoFuture};
use std::io;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use axum::Router;
use oxrtc_core::{CancellationToken, PerIpLimit, PerIpPermit};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio::time::{Instant, Sleep};

/// Most connections [`serve`] keeps open at once; further clients wait in the
/// kernel's accept queue until one closes.
pub const MAX_CONNECTIONS: usize = 256;

/// Most connections one peer address keeps open at once (an IPv6 peer counts
/// for its `/64`; loopback is not exempt): half of [`MAX_CONNECTIONS`], so that
/// one address never takes every slot. A connection beyond it is closed as
/// soon as it is accepted.
pub const MAX_CONNECTIONS_PER_IP: usize = 128;

/// A connection that neither reads nor writes for this long is closed. Longer
/// than the slowest request deadline, so that a waiting request is never cut;
/// short enough that idle or trickling clients cannot hold every slot.
pub const IDLE_TIMEOUT: Duration = Duration::from_secs(75);

/// Once cancelled, [`serve`] lets requests in flight finish for this long at
/// most: a preload waits up to 30 s and a WebRTC answer up to 60 s, while the
/// process must stop within 5 s of `SIGTERM` (docs/COMPATIBILITY.md).
pub const SHUTDOWN_GRACE: Duration = Duration::from_secs(3);

/// The bounds of [`serve_with_limits`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ServeLimits {
    /// Most connections open at once (at least 1).
    pub max_connections: usize,
    /// Most connections one peer address keeps open at once (at least 1).
    pub max_connections_per_ip: usize,
    /// A connection idle for this long is closed.
    pub idle_timeout: Duration,
    /// Once cancelled, how long requests in flight may still finish before
    /// [`serve_with_limits`] returns anyway.
    pub shutdown_grace: Duration,
}

impl Default for ServeLimits {
    fn default() -> Self {
        Self {
            max_connections: MAX_CONNECTIONS,
            max_connections_per_ip: MAX_CONNECTIONS_PER_IP,
            idle_timeout: IDLE_TIMEOUT,
            shutdown_grace: SHUTDOWN_GRACE,
        }
    }
}

/// Pause after a failed `accept` (out of file descriptors, typically).
const ACCEPT_RETRY: Duration = Duration::from_millis(100);

/// A TCP listener that accepts only while it holds a free permit, and keeps
/// a connection only if its peer address is under its own bound.
struct Bounded {
    inner: TcpListener,
    permits: Arc<Semaphore>,
    per_ip: PerIpLimit,
    idle_timeout: Duration,
}

/// The peer of a connection accepted by [`serve`], as `ConnectInfo<PeerAddr>`.
#[derive(Debug, Clone, Copy)]
pub(crate) struct PeerAddr(pub(crate) SocketAddr);

impl axum::extract::connect_info::Connected<axum::serve::IncomingStream<'_, Bounded>> for PeerAddr {
    fn connect_info(stream: axum::serve::IncomingStream<'_, Bounded>) -> Self {
        Self(*stream.remote_addr())
    }
}

/// A connection, holding its permit until it is dropped, and failing its
/// reads once it has been idle for the timeout.
pub(crate) struct Counted {
    stream: TcpStream,
    idle: Pin<Box<Sleep>>,
    idle_timeout: Duration,
    _address: PerIpPermit,
    _permit: OwnedSemaphorePermit,
}

impl Counted {
    fn active(&mut self) {
        self.idle.as_mut().reset(Instant::now() + self.idle_timeout);
    }

    fn expired(&mut self, cx: &mut Context<'_>) -> bool {
        self.idle.as_mut().poll(cx).is_ready()
    }

    /// A write's outcome: progress keeps the connection alive; a peer that
    /// stops reading (the write stays pending) is cut once idle for the
    /// timeout, like one that stops writing.
    fn wrote(&mut self, result: Poll<io::Result<usize>>, cx: &mut Context<'_>) -> Poll<io::Result<usize>> {
        match result {
            Poll::Ready(Ok(n)) if n > 0 => {
                self.active();
                Poll::Ready(Ok(n))
            }
            Poll::Pending if self.expired(cx) => Poll::Ready(Err(idle_error())),
            other => other,
        }
    }
}

fn idle_error() -> io::Error {
    io::Error::new(io::ErrorKind::TimedOut, "idle connection")
}

impl axum::serve::Listener for Bounded {
    type Io = Counted;
    type Addr = SocketAddr;

    async fn accept(&mut self) -> (Self::Io, Self::Addr) {
        let Ok(permit) = self.permits.clone().acquire_owned().await else {
            // The semaphore is never closed.
            return std::future::pending().await;
        };
        loop {
            match self.inner.accept().await {
                Ok((stream, addr)) => {
                    let Some(address) = self.per_ip.try_acquire(addr.ip()) else {
                        if let Some(refused) = self.per_ip.refused() {
                            tracing::warn!(
                                peer = %addr,
                                limit = self.per_ip.max_per_ip(),
                                refused,
                                "api: per-address connection limit reached, closing"
                            );
                        }
                        drop(stream);
                        continue;
                    };
                    let _ = stream.set_nodelay(true);
                    let idle = Box::pin(tokio::time::sleep(self.idle_timeout));
                    let idle_timeout = self.idle_timeout;
                    return (Counted { stream, idle, idle_timeout, _address: address, _permit: permit }, addr);
                }
                Err(error) => {
                    tracing::warn!("api: accept failed: {error}");
                    tokio::time::sleep(ACCEPT_RETRY).await;
                }
            }
        }
    }

    fn local_addr(&self) -> io::Result<Self::Addr> {
        self.inner.local_addr()
    }
}

impl AsyncRead for Counted {
    fn poll_read(self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &mut ReadBuf<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        let before = buf.filled().len();
        match Pin::new(&mut this.stream).poll_read(cx, buf) {
            Poll::Ready(result) => {
                if buf.filled().len() > before {
                    this.active();
                }
                Poll::Ready(result)
            }
            Poll::Pending if this.expired(cx) => Poll::Ready(Err(idle_error())),
            Poll::Pending => Poll::Pending,
        }
    }
}

impl AsyncWrite for Counted {
    fn poll_write(self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &[u8]) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        let result = Pin::new(&mut this.stream).poll_write(cx, buf);
        this.wrote(result, cx)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        match Pin::new(&mut this.stream).poll_flush(cx) {
            Poll::Pending if this.expired(cx) => Poll::Ready(Err(idle_error())),
            other => other,
        }
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().stream).poll_shutdown(cx)
    }

    fn poll_write_vectored(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[io::IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        let result = Pin::new(&mut this.stream).poll_write_vectored(cx, bufs);
        this.wrote(result, cx)
    }

    fn is_write_vectored(&self) -> bool {
        self.stream.is_write_vectored()
    }
}

/// Serves `router` on `listener` until `cancel` fires, with at most
/// [`MAX_CONNECTIONS`] connections open ([`MAX_CONNECTIONS_PER_IP`] per peer
/// address), each closed after [`IDLE_TIMEOUT`]
/// of silence; returns at most [`SHUTDOWN_GRACE`] after `cancel`.
pub async fn serve(listener: TcpListener, router: Router, cancel: CancellationToken) -> io::Result<()> {
    serve_with_limits(listener, router, cancel, ServeLimits::default()).await
}

/// [`serve`] with other bounds.
pub async fn serve_with_limits(
    listener: TcpListener,
    router: Router,
    cancel: CancellationToken,
    limits: ServeLimits,
) -> io::Result<()> {
    let bounded = Bounded {
        inner: listener,
        permits: Arc::new(Semaphore::new(limits.max_connections.max(1))),
        per_ip: PerIpLimit::new(limits.max_connections_per_ip),
        idle_timeout: limits.idle_timeout,
    };
    let service = router.into_make_service_with_connect_info::<PeerAddr>();
    let stop = cancel.clone();
    let server = axum::serve(bounded, service).with_graceful_shutdown(async move { stop.cancelled().await });
    let grace = limits.shutdown_grace;
    tokio::select! {
        result = server.into_future() => result,
        () = async move {
            cancel.cancelled().await;
            tokio::time::sleep(grace).await;
        } => {
            tracing::warn!("api: requests still in flight {grace:?} after shutdown; not waiting for them");
            Ok(())
        }
    }
}
