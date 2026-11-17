//! Closing a connection in two steps (RFC 9112, section 9.6).
//!
//! hyper answers a request as soon as its route has an answer, and closes the
//! connection when the request's body was not read to its end: a refusal
//! (`401`, `404`, `415`...) is decided on the head alone. Closing a socket
//! whose peer is still writing makes the kernel answer those bytes with a
//! reset, and a reset that reaches the peer before it has read the answer
//! destroys the answer (on Windows always; on Linux when the reset comes
//! first). The client then sees a broken connection where a status was sent.
//!
//! So the socket is not closed with the connection: its write side is ended
//! (the peer reads the answer, then the end of it), and what the peer still
//! sends is read and dropped until it closes, [`LINGER_TIMEOUT`] passes or
//! [`LINGER_BYTES`] were dropped. The connection keeps its two slots (the
//! total and its address's) until then, so the bounds of the listener still
//! count it.

use std::time::Duration;

use oxrtc_core::{CancellationToken, PerIpPermit};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::OwnedSemaphorePermit;

/// How long a connection the API is done with is still read before its socket
/// is closed, whatever its peer does. Apache's two seconds: long enough for a
/// client to finish a body the API would have taken ([`crate::MAX_BODY_BYTES`])
/// and to read its answer, short next to the 75 s an idle peer holds its
/// connection anyway.
pub const LINGER_TIMEOUT: Duration = Duration::from_secs(2);

/// Most bytes read and dropped from a connection the API is done with: four
/// bodies of the largest size it takes. A peer that sends more is not ending a
/// request, and is cut.
pub const LINGER_BYTES: usize = 4 * crate::routes::MAX_BODY_BYTES;

/// A connection's socket with the slots it holds: the listener's total and
/// its peer address's.
pub(crate) struct Held {
    pub(crate) stream: TcpStream,
    pub(crate) address: PerIpPermit,
    pub(crate) permit: OwnedSemaphorePermit,
}

/// Ends `held` in two steps, on a task of its own: at most `wait`, at most
/// [`LINGER_BYTES`], and at once when `cancel` fires (a stopping server waits
/// for no peer).
pub(crate) fn close(held: Held, wait: Duration, cancel: CancellationToken) {
    // Dropped outside a runtime (the runtime itself is going): closed at once.
    let Ok(runtime) = tokio::runtime::Handle::try_current() else { return };
    runtime.spawn(async move {
        let Held { mut stream, address: _address, permit: _permit } = held;
        tokio::select! {
            () = cancel.cancelled() => {}
            () = tokio::time::sleep(wait) => {}
            () = drain(&mut stream) => {}
        }
    });
}

/// Ends the server's side, then reads and drops what the peer sends until it
/// closes, fails or has sent [`LINGER_BYTES`].
async fn drain(stream: &mut TcpStream) {
    if stream.shutdown().await.is_err() {
        return;
    }
    let mut scratch = [0u8; 4096];
    let mut left = LINGER_BYTES;
    while left > 0 {
        match stream.read(&mut scratch).await {
            Ok(0) | Err(_) => return,
            Ok(read) => left = left.saturating_sub(read),
        }
    }
}
