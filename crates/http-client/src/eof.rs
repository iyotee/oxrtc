//! A TLS stream whose end without `close_notify` is an end, not an error.
//!
//! A camera that answers `HTTP/1.0` and ends the body by closing its socket
//! (the owner's `https` webcam does, and so does every small camera that
//! sends a picture with no length) closes the TCP connection with no TLS
//! `close_notify`. rustls reports that as `UnexpectedEof`; Go, which go2rtc
//! is written in, takes it for the end of the data (it only calls it an
//! error in the middle of a record). Without the same here, a picture with
//! no length over `https` is never a picture.
//!
//! This does not weaken a response that has a length: hyper still counts
//! the bytes, and a body shorter than its `Content-Length` is an incomplete
//! message whatever ended it.

use std::io;
use std::pin::Pin;
use std::task::{Context, Poll};

use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

/// `inner`, whose reads end with `Ok(0)` where it would say `UnexpectedEof`.
pub(crate) struct CloseIsEof<I>(pub(crate) I);

impl<I: AsyncRead + Unpin> AsyncRead for CloseIsEof<I> {
    fn poll_read(mut self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &mut ReadBuf<'_>) -> Poll<io::Result<()>> {
        match Pin::new(&mut self.0).poll_read(cx, buf) {
            Poll::Ready(Err(e)) if e.kind() == io::ErrorKind::UnexpectedEof => Poll::Ready(Ok(())),
            other => other,
        }
    }
}

impl<I: AsyncWrite + Unpin> AsyncWrite for CloseIsEof<I> {
    fn poll_write(mut self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &[u8]) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.0).poll_write(cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.0).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.0).poll_shutdown(cx)
    }

    fn poll_write_vectored(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[io::IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.0).poll_write_vectored(cx, bufs)
    }

    fn is_write_vectored(&self) -> bool {
        self.0.is_write_vectored()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncReadExt;

    /// Reads `data`, then fails the way rustls does at a close without `close_notify`.
    struct Cut(&'static [u8], io::ErrorKind);

    impl AsyncRead for Cut {
        fn poll_read(mut self: Pin<&mut Self>, _: &mut Context<'_>, buf: &mut ReadBuf<'_>) -> Poll<io::Result<()>> {
            if self.0.is_empty() {
                return Poll::Ready(Err(io::Error::new(
                    self.1,
                    "peer closed connection without sending TLS close_notify",
                )));
            }
            let n = self.0.len().min(buf.remaining());
            buf.put_slice(&self.0[..n]);
            self.0 = &self.0[n..];
            Poll::Ready(Ok(()))
        }
    }

    #[tokio::test]
    async fn an_unexpected_eof_is_the_end_of_the_data() {
        let mut out = Vec::new();
        CloseIsEof(Cut(b"abc", io::ErrorKind::UnexpectedEof)).read_to_end(&mut out).await.unwrap();
        assert_eq!(out, b"abc");
    }

    #[tokio::test]
    async fn any_other_error_stays_one() {
        let mut out = Vec::new();
        let error = CloseIsEof(Cut(b"abc", io::ErrorKind::ConnectionReset)).read_to_end(&mut out).await.unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::ConnectionReset);
    }
}
