//! Serving the router with a bounded number of open connections.

use std::future::{Future, IntoFuture};
use std::io;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::task::{Context, Poll};
use std::time::Duration;

use axum::Router;
use axum::extract::{ConnectInfo, Request};
use axum::middleware::Next;
use axum::response::Response;
use oxrtc_core::{CancellationToken, PerIpLimit, Refusals};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Semaphore;
use tokio::time::{Instant, Sleep};

use crate::linger::{self, Held, LINGER_TIMEOUT};

/// Most connections [`serve`] keeps open at once; further clients wait in the
/// kernel's accept queue until one closes (reaching it is logged, at most once
/// per [`oxrtc_core::refusals::REPORT_EVERY`]).
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

/// A request's head (request line and headers) must arrive within this long
/// of its first byte. The idle timeout restarts at every byte, so without it
/// a peer trickling headers holds its connection for as long as hyper's
/// header buffer lasts (hundreds of KiB); the body and the answer are under
/// the routes' own deadlines ([`crate::QUICK_TIMEOUT`], [`crate::SLOW_TIMEOUT`]).
pub const HEAD_TIMEOUT: Duration = Duration::from_secs(10);

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
    /// A request whose head takes longer than this to arrive closes its
    /// connection.
    pub head_timeout: Duration,
    /// Once cancelled, how long requests in flight may still finish before
    /// [`serve_with_limits`] returns anyway.
    pub shutdown_grace: Duration,
    /// How long a connection the API is done with is still read, so that its
    /// answer is not lost to a reset, before its socket is closed
    /// ([`LINGER_TIMEOUT`], `linger.rs`).
    pub linger: Duration,
}

impl Default for ServeLimits {
    fn default() -> Self {
        Self {
            max_connections: MAX_CONNECTIONS,
            max_connections_per_ip: MAX_CONNECTIONS_PER_IP,
            idle_timeout: IDLE_TIMEOUT,
            head_timeout: HEAD_TIMEOUT,
            shutdown_grace: SHUTDOWN_GRACE,
            linger: LINGER_TIMEOUT,
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
    /// The bound `permits` holds, for the log.
    max: usize,
    /// Times every permit was taken, reported once per interval.
    full: Refusals,
    per_ip: PerIpLimit,
    idle_timeout: Duration,
    head_timeout: Duration,
    linger: Duration,
    /// Fires when the server stops: no closed connection lingers past it.
    cancel: CancellationToken,
}

/// The peer of a connection accepted by [`serve`], as `ConnectInfo<PeerAddr>`,
/// and where the connection's request in flight is.
#[derive(Debug, Clone)]
pub(crate) struct PeerAddr(pub(crate) SocketAddr, Arc<Progress>);

impl PeerAddr {
    /// The connection is now a WebSocket: its bytes are frames, which no
    /// request-head clock times.
    pub(crate) fn upgraded(&self) {
        self.1.upgraded.store(true, Ordering::Release);
    }
}

impl axum::extract::connect_info::Connected<axum::serve::IncomingStream<'_, Bounded>> for PeerAddr {
    fn connect_info(stream: axum::serve::IncomingStream<'_, Bounded>) -> Self {
        Self(*stream.remote_addr(), stream.io().progress.clone())
    }
}

/// What the router has done with a connection's request in flight, which the
/// connection's reads and writes cannot see themselves.
#[derive(Debug, Default)]
pub(crate) struct Progress {
    /// The router has the request: its head is complete.
    served: AtomicBool,
    /// The router has its answer: what is written now is the answer, not an
    /// interim `100 Continue`.
    answered: AtomicBool,
    /// The connection was handed to a WebSocket (`/api/ws`): it carries no
    /// more requests.
    upgraded: AtomicBool,
}

/// The first layer a request meets: hyper calls the router once a request's
/// head is complete, which stops that request's head clock; the answer the
/// router returns is what hyper writes next.
async fn track(request: Request, next: Next) -> Response {
    let progress = request.extensions().get::<ConnectInfo<PeerAddr>>().map(|ConnectInfo(peer)| peer.1.clone());
    if let Some(progress) = &progress {
        progress.served.store(true, Ordering::Release);
    }
    let response = next.run(request).await;
    if let Some(progress) = progress {
        progress.answered.store(true, Ordering::Release);
    }
    response
}

/// Where a connection is between two requests.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    /// Nothing read since the last answer.
    Idle,
    /// A request's first bytes read, its head not yet complete: the head
    /// clock runs.
    Head,
    /// The router has the request; its routes' deadlines bound the rest.
    Served,
}

/// A connection, failing its reads once it has been idle for the timeout, or
/// once a request's head has taken longer than [`ServeLimits::head_timeout`]
/// to arrive. Dropped, it hands its socket and its slots to `linger.rs`, which
/// closes it in two steps.
pub(crate) struct Counted {
    /// The socket and its slots: there until the drop.
    held: Option<Held>,
    linger: Duration,
    cancel: CancellationToken,
    idle: Pin<Box<Sleep>>,
    idle_timeout: Duration,
    head: Pin<Box<Sleep>>,
    head_timeout: Duration,
    phase: Phase,
    /// Set by [`track`] for the request in flight.
    progress: Arc<Progress>,
}

impl Drop for Counted {
    fn drop(&mut self) {
        if let Some(held) = self.held.take() {
            linger::close(held, self.linger, self.cancel.clone());
        }
    }
}

impl Counted {
    /// Polls the socket through `poll`.
    fn socket<T>(&mut self, poll: impl FnOnce(Pin<&mut TcpStream>) -> Poll<io::Result<T>>) -> Poll<io::Result<T>> {
        match self.held.as_mut() {
            Some(held) => poll(Pin::new(&mut held.stream)),
            None => Poll::Ready(Err(io::Error::new(io::ErrorKind::NotConnected, "connection closed"))),
        }
    }

    fn active(&mut self) {
        self.idle.as_mut().reset(Instant::now() + self.idle_timeout);
    }

    fn expired(&mut self, cx: &mut Context<'_>) -> bool {
        self.idle.as_mut().poll(cx).is_ready()
    }

    /// Bytes read: the first ones after an answer start a request's head
    /// clock.
    fn read_some(&mut self) {
        self.active();
        if self.phase == Phase::Idle && !self.progress.upgraded.load(Ordering::Acquire) {
            self.phase = Phase::Head;
            self.progress.served.store(false, Ordering::Release);
            self.progress.answered.store(false, Ordering::Release);
            self.head.as_mut().reset(Instant::now() + self.head_timeout);
        }
    }

    /// Whether the request in progress has taken too long to show its head.
    fn head_too_slow(&mut self, cx: &mut Context<'_>) -> bool {
        if self.phase != Phase::Head {
            return false;
        }
        if self.progress.served.load(Ordering::Acquire) {
            self.phase = Phase::Served;
            return false;
        }
        self.head.as_mut().poll(cx).is_ready()
    }

    /// A write's outcome: progress keeps the connection alive; a peer that
    /// stops reading (the write stays pending) is cut once idle for the
    /// timeout, like one that stops writing.
    fn wrote(&mut self, result: Poll<io::Result<usize>>, cx: &mut Context<'_>) -> Poll<io::Result<usize>> {
        match result {
            Poll::Ready(Ok(n)) if n > 0 => {
                self.active();
                // An answer: what is read next is another request. A
                // `100 Continue` is written before the router answers, and
                // what follows it is the same request's body.
                if self.progress.answered.load(Ordering::Acquire) {
                    self.phase = Phase::Idle;
                }
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

fn head_error() -> io::Error {
    io::Error::new(io::ErrorKind::TimedOut, "request head too slow")
}

impl axum::serve::Listener for Bounded {
    type Io = Counted;
    type Addr = SocketAddr;

    async fn accept(&mut self) -> (Self::Io, Self::Addr) {
        let permit = match self.permits.clone().try_acquire_owned() {
            Ok(permit) => permit,
            Err(_) => {
                // Nothing is refused: further clients wait in the accept
                // queue. Said like the refusals, so that a saturated API is
                // seen in the log without one line per connection.
                if let Some(reached) = self.full.refused() {
                    tracing::warn!(limit = self.max, reached, "api: connection limit reached, new connections wait");
                }
                let Ok(permit) = self.permits.clone().acquire_owned().await else {
                    // The semaphore is never closed.
                    return std::future::pending().await;
                };
                permit
            }
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
                    let counted = Counted {
                        held: Some(Held { stream, address, permit }),
                        linger: self.linger,
                        cancel: self.cancel.clone(),
                        idle: Box::pin(tokio::time::sleep(self.idle_timeout)),
                        idle_timeout: self.idle_timeout,
                        head: Box::pin(tokio::time::sleep(self.head_timeout)),
                        head_timeout: self.head_timeout,
                        phase: Phase::Idle,
                        progress: Arc::new(Progress::default()),
                    };
                    return (counted, addr);
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
        match this.socket(|stream| stream.poll_read(cx, buf)) {
            Poll::Ready(result) => {
                if buf.filled().len() > before {
                    this.read_some();
                }
                Poll::Ready(result)
            }
            Poll::Pending if this.head_too_slow(cx) => Poll::Ready(Err(head_error())),
            Poll::Pending if this.expired(cx) => Poll::Ready(Err(idle_error())),
            Poll::Pending => Poll::Pending,
        }
    }
}

impl AsyncWrite for Counted {
    fn poll_write(self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &[u8]) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        let result = this.socket(|stream| stream.poll_write(cx, buf));
        this.wrote(result, cx)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        match this.socket(|stream| stream.poll_flush(cx)) {
            Poll::Pending if this.expired(cx) => Poll::Ready(Err(idle_error())),
            other => other,
        }
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.get_mut().socket(|stream| stream.poll_shutdown(cx))
    }

    fn poll_write_vectored(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[io::IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        let result = this.socket(|stream| stream.poll_write_vectored(cx, bufs));
        this.wrote(result, cx)
    }

    fn is_write_vectored(&self) -> bool {
        self.held.as_ref().is_some_and(|held| held.stream.is_write_vectored())
    }
}

/// Serves `router` on `listener` until `cancel` fires, with at most
/// [`MAX_CONNECTIONS`] connections open ([`MAX_CONNECTIONS_PER_IP`] per peer
/// address), each closed after [`IDLE_TIMEOUT`] of silence or when a request's
/// head takes more than [`HEAD_TIMEOUT`], in two steps ([`LINGER_TIMEOUT`]);
/// returns at most [`SHUTDOWN_GRACE`] after `cancel`.
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
        max: limits.max_connections.max(1),
        full: Refusals::new(),
        per_ip: PerIpLimit::new(limits.max_connections_per_ip),
        idle_timeout: limits.idle_timeout,
        head_timeout: limits.head_timeout,
        linger: limits.linger,
        cancel: cancel.clone(),
    };
    let router =
        router.layer(axum::middleware::from_fn(track)).layer(axum::Extension(crate::ws::Shutdown(cancel.clone())));
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
