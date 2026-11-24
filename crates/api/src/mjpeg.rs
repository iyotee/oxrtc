//! Motion JPEG (M5 F5, D46): `GET /api/stream.mjpeg` (go2rtc's `mpjpeg`, for
//! an `<img src>`, VLC, ffplay, FFmpeg, Home Assistant's MJPEG camera) and
//! the pictures of `{"type":"mjpeg"}` on `/api/ws` (go2rtc's player in
//! `mjpeg` mode), both read through [`Pictures`].
//!
//! The pictures are the stream's own when one of its sources is Motion JPEG
//! (an MJPEG camera over RTSP, depacketised by RFC 2435, or over HTTP; a
//! configured `ffmpeg:<stream>#video=mjpeg`): passed on as they came, never
//! decoded. A stream with none is encoded by FFmpeg: its derived stream
//! `ffmpeg:<stream>#video=mjpeg` (`oxrtc_core::derived`), one process for
//! every Motion JPEG reader of the stream, started by the first and stopped
//! after a transcode's idle grace when the last leaves (go2rtc 1.9.15 serves
//! a stream with no JPEG source nothing: an empty `200`). A picture that
//! carries no Huffman tables (an `AVI1` webcam's) is sent with the standard
//! ones written in, for the decoders that do not assume them (`oxrtc_core::jpeg`).
//!
//! Each reader's subscription is pooled: its queue is bounded by its share
//! of the output memory budget (D45), and a reader slower than the stream
//! loses pictures - every one stands alone, so it resumes at the next - with
//! the consumer's `dropped` counter and a log line saying so.
//!
//! The response is go2rtc's, byte for byte: `multipart/x-mixed-replace;
//! boundary=frame`, each part `--frame`, `Content-Type: image/jpeg` and its
//! `Content-Length`, the picture, a line end. Each part is written with the
//! next part's delimiter after it rather than before it: the same bytes, but
//! a browser shows a picture once the delimiter after it has come (Chromium
//! issue 527446), and go2rtc's picture waits for the next one to be shown.

use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::IntoResponse;
use bytes::{Bytes, BytesMut};
use oxrtc_core::jpeg::with_huffman_tables;
use oxrtc_core::{
    CancellationToken, Codec, ConsumerTag, Event, Hub, MediaWant, Refusals, Stream, SubscribeError, Subscription, Want,
};

use crate::ApiState;
use crate::fmp4::LOSS_REPORT_EVERY;
use crate::routes::{Fail, Query, Reply, text};
use crate::slots::Slot;

/// go2rtc's name for these consumers, which `/api/streams` lists.
pub const CONSUMER_KIND: &str = "mjpeg";

/// The `Content-Type` of `stream.mjpeg`, go2rtc's.
pub const CONTENT_TYPE: &str = "multipart/x-mixed-replace; boundary=frame";

/// The key a stream's Motion JPEG transcode is kept under.
pub const DERIVED_KEY: &str = "mjpeg";

/// The delimiter of a part, line end included.
const DELIMITER: &[u8] = b"--frame\r\n";

/// The source that encodes a stream with no JPEG of its own.
#[must_use]
pub fn transcode_source(stream: &str) -> String {
    format!("ffmpeg:{stream}#video=mjpeg")
}

/// A reader's wish: Motion JPEG, no sound.
fn jpeg() -> Want {
    Want { video: MediaWant::OneOf(vec![Codec::Jpeg]), audio: MediaWant::Skip }
}

/// One part of `stream.mjpeg`, the delimiter of the next one after it.
#[must_use]
pub fn part(picture: &[u8]) -> Bytes {
    let head = format!("Content-Type: image/jpeg\r\nContent-Length: {}\r\n\r\n", picture.len());
    let mut out = BytesMut::with_capacity(head.len() + picture.len() + 2 + DELIMITER.len());
    out.extend_from_slice(head.as_bytes());
    out.extend_from_slice(picture);
    out.extend_from_slice(b"\r\n");
    out.extend_from_slice(DELIMITER);
    out.freeze()
}

/// One reader's pictures.
pub(crate) struct Pictures {
    subscription: Subscription,
    stream: String,
    remote: String,
    /// The losses, said at most once per [`LOSS_REPORT_EVERY`].
    losses: Refusals,
}

/// go2rtc's words where it has them.
fn subscribe_error(error: &SubscribeError) -> String {
    match error {
        SubscribeError::NoMatchingSource | SubscribeError::NothingWanted => "codecs not matched".to_owned(),
        other => format!("streams: {other}"),
    }
}

impl Pictures {
    /// The stream's own JPEG, else its transcode.
    pub(crate) async fn start(hub: &Hub, stream: Arc<Stream>, tag: ConsumerTag) -> Result<Self, String> {
        let name = stream.name().to_owned();
        let remote = tag.remote.clone().unwrap_or_else(|| "-".to_owned());
        let subscription = match stream.subscribe(&jpeg(), tag.clone()).await {
            Ok(subscription) => subscription,
            Err(SubscribeError::NoMatchingSource) => {
                let derived = hub.derived(&stream, DERIVED_KEY, &transcode_source(&name)).map_err(|error| {
                    tracing::debug!(stream = %name, %error, "api: mjpeg: no transcode");
                    "codecs not matched".to_owned()
                })?;
                tracing::debug!(stream = %name, "api: mjpeg: no JPEG source, encoded by FFmpeg");
                derived.subscribe(&jpeg(), tag).await.map_err(|e| subscribe_error(&e))?
            }
            Err(error) => return Err(subscribe_error(&error)),
        };
        Ok(Self { subscription, stream: name, remote, losses: Refusals::every(LOSS_REPORT_EVERY) })
    }

    /// The next picture, with the standard Huffman tables when it has none.
    /// Cancel-safe: an event is handled whole between two awaits.
    pub(crate) async fn next(&mut self) -> Result<Bytes, String> {
        loop {
            match self.subscription.recv().await {
                Ok(Event::Frame { frame, .. }) => return Ok(with_huffman_tables(&frame.data)),
                Ok(Event::Tracks(_)) => {}
                Ok(Event::Discontinuity) => {
                    if let Some(losses) = self.losses.refused() {
                        tracing::info!(
                            stream = %self.stream,
                            remote = %self.remote,
                            losses,
                            "api: mjpeg reader fell behind the stream: pictures dropped, it resumes at the next one"
                        );
                    }
                }
                Err(end) => return Err(format!("streams: {end}")),
            }
        }
    }
}

/// The consumer tag of a request: its address and `User-Agent`, and the
/// queue class its `queue` names.
fn tag_of(request: &Request) -> Result<ConsumerTag, Fail> {
    let raw = request.uri().query().unwrap_or_default();
    let class = oxrtc_core::ConsumerClass::from_query(raw)
        .map_err(|e| text(StatusCode::BAD_REQUEST, e.to_string()))?
        .unwrap_or_default();
    let mut tag = ConsumerTag::new(CONSUMER_KIND).with_class(class).pooled();
    if let Some(remote) = crate::mp4::remote(request.extensions()) {
        tag = tag.with_remote(remote.to_string());
    }
    if let Some(agent) = request.headers().get(header::USER_AGENT).and_then(|v| v.to_str().ok()) {
        tag = tag.with_user_agent(agent);
    }
    Ok(tag)
}

fn failed(name: &str, reason: &str) -> Fail {
    let fail = text(StatusCode::INTERNAL_SERVER_ERROR, reason);
    tracing::warn!(stream = %name, "api: stream.mjpeg failed: {}", fail.message());
    fail
}

/// `GET /api/stream.mjpeg`: answered once the first picture is there, so
/// that a stream that has none is an error, not an empty stream.
pub(crate) async fn stream_mjpeg(State(state): State<ApiState>, request: Request) -> Reply {
    let query = Query::of(request.uri())?;
    let stream = query.stream(&state)?;
    let tag = tag_of(&request)?;
    let remote = crate::mp4::remote(request.extensions());
    let slot = state
        .mp4
        .try_open_as(remote, "mjpeg response")
        .ok_or_else(|| text(StatusCode::SERVICE_UNAVAILABLE, "too many mjpeg responses"))?;
    let shutdown =
        request.extensions().get::<crate::ws::Shutdown>().map_or_else(CancellationToken::new, |s| s.0.clone());
    let name = stream.name().to_owned();
    let first = async {
        let mut pictures = Pictures::start(&state.hub, stream, tag).await?;
        let picture = pictures.next().await?;
        Ok::<_, String>((pictures, picture))
    };
    let (pictures, picture) = tokio::select! {
        () = shutdown.cancelled() => return Err(failed(&name, "the server is stopping")),
        made = first => made.map_err(|reason| failed(&name, &reason))?,
    };
    tracing::debug!(stream = %name, "api: stream.mjpeg started");
    let mut opening = BytesMut::from(DELIMITER);
    opening.extend_from_slice(&part(&picture));
    let body = Parts {
        pictures,
        opening: Some(opening.freeze()),
        silence: state.mp4_limits.silence,
        shutdown,
        stream: name,
        _slot: slot,
    };
    let chunks = futures_util::stream::unfold(body, Parts::next);
    let mut response = Body::from_stream(chunks).into_response();
    let headers = response.headers_mut();
    headers.insert(header::CONTENT_TYPE, HeaderValue::from_static(CONTENT_TYPE));
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    headers.insert(header::PRAGMA, HeaderValue::from_static("no-cache"));
    Ok(response)
}

/// A `stream.mjpeg` body. Dropped (the client gone, the server stopping), it
/// ends the subscription.
struct Parts {
    pictures: Pictures,
    /// The first delimiter and part, sent first.
    opening: Option<Bytes>,
    silence: Duration,
    shutdown: CancellationToken,
    stream: String,
    _slot: Slot,
}

impl Parts {
    async fn next(mut self) -> Option<(Result<Bytes, Infallible>, Self)> {
        if let Some(opening) = self.opening.take() {
            return Some((Ok(opening), self));
        }
        let picture = tokio::select! {
            () = self.shutdown.cancelled() => return None,
            next = tokio::time::timeout(self.silence, self.pictures.next()) => match next {
                Ok(Ok(picture)) => picture,
                Ok(Err(reason)) => return self.end(&reason),
                Err(_) => return self.end("nothing to write for too long"),
            },
        };
        Some((Ok(part(&picture)), self))
    }

    fn end<T>(self, why: &str) -> Option<T> {
        tracing::debug!(stream = %self.stream, "api: stream.mjpeg ended: {why}");
        None
    }
}
