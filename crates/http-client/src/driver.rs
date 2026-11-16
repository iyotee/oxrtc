//! One webcam run: the response head says what it serves, then pictures
//! flow into the core until the camera fails, stalls, or the run is cancelled.
//!
//! A `multipart/x-mixed-replace` response is the camera's stream: its parts
//! are the pictures. A response that is one picture is polled: one request
//! after the other on the same connection, [`Target::interval`] apart.

use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use http::header::CONTENT_TYPE;
use http::{HeaderValue, Response};
use http_body_util::BodyExt;
use hyper::body::Incoming;
use oxrtc_core::timeline::ticks;
use oxrtc_core::{
    BoxFuture, CancellationToken, Codec, Declared, Frame, MediaKind, ProducerSink, PublishError, SourceDriver,
    SourceError, TrackInfo,
};
use tokio::time::Instant;
use tracing::{debug, warn};

use crate::fetch::Fetcher;
use crate::multipart::{Body, Parser, body_of, printable};
use crate::target::Target;
use crate::tls::Tls;

/// Largest picture accepted (the core's own default bound for a frame).
pub const MAX_PICTURE_BYTES: usize = 8 * 1024 * 1024;

/// A picture starts with this (JPEG's start of image).
const SOI: [u8; 2] = [0xff, 0xd8];

/// Runs one webcam.
pub(crate) struct HttpDriver {
    target: Target,
    /// The source, redacted: what the logs name.
    label: String,
    user_agent: HeaderValue,
    tls: Arc<Tls>,
}

impl HttpDriver {
    pub(crate) fn new(target: Target, label: String, user_agent: HeaderValue, tls: Arc<Tls>) -> Self {
        Self { target, label, user_agent, tls }
    }

    async fn session(&self, sink: &ProducerSink) -> Result<(), SourceError> {
        let mut fetcher = Fetcher::new(
            self.target.url.clone(),
            self.target.creds.clone(),
            self.user_agent.clone(),
            self.target.stall_timeout,
            self.tls.clone(),
            self.target.insecure,
        );
        let started = Instant::now();
        let response = fetcher.get().await?;
        let status = response.status();
        if !status.is_success() {
            return Err(SourceError::Unreachable(format!("HTTP {}", status.as_u16())));
        }
        let content_type = response.headers().get(CONTENT_TYPE).and_then(|v| v.to_str().ok());
        let body = body_of(content_type);
        debug!(source = %self.label, ?body, "HTTP response");
        if self.target.insecure && self.target.url.scheme() == "https" {
            warn!(source = %self.label, "https: the certificate is not checked (#insecure)");
        }
        match body {
            Body::Other(media) => Err(SourceError::Protocol(format!("{media} is not a Motion JPEG source"))),
            Body::Parts(boundary) => {
                sink.set_tracks(vec![TrackInfo::video(Codec::Jpeg)]).map_err(refused)?;
                self.parts(sink, response, boundary.as_deref(), started).await
            }
            Body::Image => {
                sink.set_tracks(vec![TrackInfo::video(Codec::Jpeg)]).map_err(refused)?;
                self.poll(sink, fetcher, response, started).await
            }
        }
    }

    /// The camera's stream, part by part.
    async fn parts(
        &self,
        sink: &ProducerSink,
        response: Response<Incoming>,
        boundary: Option<&str>,
        started: Instant,
    ) -> Result<(), SourceError> {
        let mut body = response.into_body();
        let mut parser = Parser::new(boundary, MAX_PICTURE_BYTES);
        let stall = self.target.stall_timeout;
        // The clock runs from the last picture, not from the last byte: a
        // camera that trickles bytes without ever finishing one has stalled.
        let mut deadline = Instant::now() + stall;
        loop {
            let frame = tokio::time::timeout_at(deadline, body.frame()).await.map_err(|_| SourceError::Stalled)?;
            let frame = match frame {
                None => return Err(SourceError::Ended),
                Some(Err(error)) => return Err(SourceError::Unreachable(oxrtc_core::spec::redact(&error.to_string()))),
                Some(Ok(frame)) => frame,
            };
            if let Ok(data) = frame.into_data() {
                parser.push(&data);
            }
            while let Some(part) = parser.next_part().map_err(|e| SourceError::Protocol(e.to_string()))? {
                if publish(sink, started, part.data, &self.label)? {
                    deadline = Instant::now() + stall;
                }
            }
            if parser.is_done() {
                return Err(SourceError::Ended);
            }
        }
    }

    /// A camera that serves one picture per request: the first is the
    /// response in hand, the others are asked for, `interval` apart.
    async fn poll(
        &self,
        sink: &ProducerSink,
        mut fetcher: Fetcher,
        mut response: Response<Incoming>,
        started: Instant,
    ) -> Result<(), SourceError> {
        let mut last_picture = Instant::now();
        loop {
            let asked = Instant::now();
            if !response.status().is_success() {
                return Err(SourceError::Unreachable(format!("HTTP {}", response.status().as_u16())));
            }
            let picture = tokio::time::timeout(self.target.stall_timeout, read_picture(response.into_body()))
                .await
                .map_err(|_| SourceError::Stalled)??;
            if publish(sink, started, picture, &self.label)? {
                last_picture = Instant::now();
            } else if last_picture.elapsed() >= self.target.stall_timeout {
                // Answers, but not pictures: an error page, a login form.
                return Err(SourceError::Stalled);
            }
            tokio::time::sleep_until(asked + self.target.interval).await;
            response = fetcher.get().await?;
        }
    }
}

/// The whole body, bounded.
async fn read_picture(mut body: Incoming) -> Result<Bytes, SourceError> {
    let mut data = Vec::new();
    while let Some(frame) = body.frame().await {
        let frame = frame.map_err(|e| SourceError::Unreachable(oxrtc_core::spec::redact(&e.to_string())))?;
        if let Ok(chunk) = frame.into_data() {
            if data.len() + chunk.len() > MAX_PICTURE_BYTES {
                return Err(SourceError::Protocol(format!("a picture over {MAX_PICTURE_BYTES} bytes")));
            }
            data.extend_from_slice(&chunk);
        }
    }
    Ok(Bytes::from(data))
}

fn refused(e: PublishError) -> SourceError {
    SourceError::Refused(e.to_string())
}

/// Hands a picture to the core. Whether it was one: a body that does not
/// start as a JPEG (an error page, a login form) is skipped, and so is a
/// picture the core refuses; the camera stays up.
fn publish(sink: &ProducerSink, started: Instant, picture: Bytes, label: &str) -> Result<bool, SourceError> {
    if !picture.starts_with(&SOI) {
        debug!(source = %label, first = %printable(&String::from_utf8_lossy(picture.get(..16).unwrap_or(&picture)), 16), "not a JPEG, skipped");
        return Ok(false);
    }
    let pts = ticks(started.elapsed(), 90_000);
    match sink.publish(Frame::new(0, pts, true, picture)) {
        Ok(()) => Ok(true),
        Err(e @ (PublishError::TooLarge { .. } | PublishError::Malformed(_))) => {
            debug!(source = %label, error = %e, "picture refused");
            Ok(false)
        }
        Err(e) => Err(refused(e)),
    }
}

impl SourceDriver for HttpDriver {
    fn declared(&self) -> Option<Declared> {
        Some(vec![(MediaKind::Video, Codec::Jpeg)])
    }

    fn stall_timeout(&self) -> Option<Duration> {
        Some(self.target.stall_timeout)
    }

    fn run(
        self: Arc<Self>,
        sink: ProducerSink,
        cancel: CancellationToken,
    ) -> BoxFuture<'static, Result<(), SourceError>> {
        Box::pin(async move {
            tokio::select! {
                biased;
                () = cancel.cancelled() => Ok(()),
                result = self.session(&sink) => result,
            }
        })
    }
}
