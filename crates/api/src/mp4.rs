//! `GET /api/stream.mp4` and `GET /api/frame.mp4` (M5 F4), as go2rtc serves
//! them: a stream as one progressive fragmented MP4 over HTTP (chunked), for
//! a `<video src>`, VLC, ffplay, FFmpeg, or a Home Assistant service that
//! records a few seconds; and one keyframe as a short MP4.
//!
//! Both are written by [`crate::fmp4`]'s feed, the one MSE uses: the same
//! init segment, one fragment per frame from a keyframe on (the cached group
//! of pictures' when the source runs), the media moved to start at zero,
//! the subscription's live queue as the reader's (a reader slower than the
//! stream loses frames and resumes at a keyframe, `dropped` in the listing).
//!
//! The query, as go2rtc reads it ([`stream_wish`]): `src`; `mp4` (H.264 or
//! H.265 and AAC), `mp4=flac` (and the PCM family, as FLAC), `mp4=all` or
//! any other value (and Opus); without `mp4`, go2rtc's `video` and `audio`
//! filters, and with neither its default, H.264 or H.265 and AAC;
//! `duration` (seconds: the response ends after that much media, or that
//! long and [`LATE_GRACE`] after its first fragment); `filename` (`Content-Disposition`); and
//! oxrtc's `queue` (`live` or `recorder`, as an RTSP reader names it).
//!
//! Each response holds a slot of its own bound, in all and per address
//! ([`Mp4Limits`]); one that writes nothing for [`Mp4Limits::silence`] ends,
//! and every one ends when the server stops or the client goes away (the
//! body is dropped, and its subscription with it).

use std::collections::VecDeque;
use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::extract::{ConnectInfo, Request, State};
use axum::http::{Extensions, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use bytes::{Bytes, BytesMut};
use oxrtc_core::{CancellationToken, Codec, ConsumerClass, ConsumerTag, MediaWant, Stream, Want};
use oxrtc_mp4::HevcEntry;
use tokio::time::Instant;

use crate::ApiState;
use crate::fmp4::muxer::Out;
use crate::fmp4::{Feed, Step, Wish};
use crate::listener::PeerAddr;
use crate::routes::{Fail, Query, Reply, text};
use crate::slots::{Slot, Slots};
use crate::ws::Shutdown;

/// go2rtc's name for these consumers, which `/api/streams` lists.
pub const CONSUMER_KIND: &str = "mp4";

/// How much later than `duration` on the wall clock a `stream.mp4` ends, if
/// its media has not reached `duration` by then: a live picture arrives a
/// little after its time, and the last ones must not be cut for it.
pub const LATE_GRACE: Duration = Duration::from_secs(1);

/// Longest `filename` kept, in characters.
pub const MAX_FILENAME_CHARS: usize = 128;

/// The bounds of `stream.mp4` and `frame.mp4`, which share them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Mp4Limits {
    /// Responses open at once, every address together: a quarter of the
    /// listener's connections, so that they never take those of plain
    /// requests or of the WebSocket API.
    pub max_responses: usize,
    /// Responses one address keeps open at once (an IPv6 peer by its `/64`).
    pub max_responses_per_ip: usize,
    /// A `stream.mp4` that writes nothing for this long ends (a source gone
    /// silent): the listener's idle rule, for a response.
    pub silence: Duration,
    /// Longest a `frame.mp4` waits for its keyframe, from the request: past
    /// the 28 s a subscription waits for a source still starting (D27).
    pub frame_timeout: Duration,
}

/// Default [`Mp4Limits::max_responses`].
pub const MAX_RESPONSES: usize = crate::MAX_CONNECTIONS / 4;
/// Default [`Mp4Limits::max_responses_per_ip`].
pub const MAX_RESPONSES_PER_IP: usize = 16;

impl Default for Mp4Limits {
    fn default() -> Self {
        Self {
            max_responses: MAX_RESPONSES,
            max_responses_per_ip: MAX_RESPONSES_PER_IP,
            silence: crate::IDLE_TIMEOUT,
            frame_timeout: Duration::from_secs(30),
        }
    }
}

/// The responses open, against those bounds.
pub(crate) fn responses(limits: &Mp4Limits) -> Slots {
    Slots::new("mp4 response", limits.max_responses, limits.max_responses_per_ip)
}

/// Every codec the writer takes, in the order every egress shares.
fn writable() -> Want {
    Want::preferred(&[Codec::H264, Codec::H265, Codec::Aac, Codec::Opus, Codec::Pcma, Codec::Pcmu, Codec::L16])
}

/// What a `stream.mp4` query asks for, as go2rtc reads it: `mp4` (its first
/// value) chooses among its three codec sets; without it, `video` and
/// `audio` filter as for any reader, among the codecs the writer takes;
/// with neither, go2rtc's default. The sources are then chosen in the order
/// every egress shares (a camera's own H.265 before an H.264 transcode).
pub fn stream_wish(query: &str) -> Result<Wish, String> {
    let pairs: Vec<(String, String)> = form_urlencoded::parse(query.trim_start_matches('?').as_bytes())
        .take(crate::routes::MAX_QUERY_PAIRS)
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect();
    let mut sound = vec![Codec::Aac];
    let mp4 = pairs.iter().find(|(k, _)| k == "mp4").map(|(_, v)| v.as_str());
    let want = match mp4 {
        Some(mode) => {
            if !mode.is_empty() {
                sound.extend([Codec::Pcma, Codec::Pcmu, Codec::L16]);
            }
            if !mode.is_empty() && mode != "flac" {
                sound.push(Codec::Opus);
            }
            Want::preferred(&[[Codec::H264, Codec::H265].as_slice(), &sound].concat())
        }
        None if !pairs.iter().any(|(k, _)| k == "video" || k == "audio") => {
            Want::preferred(&[Codec::H264, Codec::H265, Codec::Aac])
        }
        None => {
            let media: Vec<String> = pairs
                .iter()
                .filter(|(k, _)| k == "video" || k == "audio")
                .map(|(k, v)| if v.is_empty() { k.clone() } else { format!("{k}={v}") })
                .collect();
            let asked = Want::from_query(&media.join("&")).map_err(|e| e.to_string())?;
            let can = writable();
            Want { video: asked.video.narrow(&can.video), audio: asked.audio.narrow(&can.audio) }
        }
    };
    Ok(Wish { want, hevc: HevcEntry::Hvc1 })
}

/// `frame.mp4`'s wish: the picture alone, H.264 or H.265 (go2rtc's).
fn frame_wish() -> Wish {
    Wish {
        want: Want { video: MediaWant::OneOf(vec![Codec::H265, Codec::H264]), audio: MediaWant::Skip },
        hevc: HevcEntry::Hvc1,
    }
}

/// `duration`: whole seconds; 0 is none, as go2rtc reads it.
pub fn duration(value: Option<&str>) -> Result<Option<Duration>, String> {
    match value.map(str::parse::<u32>) {
        None | Some(Ok(0)) => Ok(None),
        Some(Ok(seconds)) => Ok(Some(Duration::from_secs(u64::from(seconds)))),
        Some(Err(_)) => Err("invalid duration".to_owned()),
    }
}

/// `Content-Disposition: attachment` for `filename`: the name in quotes with
/// what cannot stand there (a quote, a backslash, a slash, a control or
/// non-ASCII character) replaced by `_`, and RFC 6266's `filename*` beside it
/// when the name is not ASCII. At most [`MAX_FILENAME_CHARS`] characters.
pub fn disposition(filename: &str) -> HeaderValue {
    let name: String =
        filename.chars().filter(|c| !c.is_control() && !matches!(c, '/' | '\\')).take(MAX_FILENAME_CHARS).collect();
    let plain: String =
        name.chars().map(|c| if c == ' ' || (c.is_ascii_graphic() && c != '"') { c } else { '_' }).collect();
    let mut value = format!("attachment; filename=\"{plain}\"");
    if !name.is_ascii() {
        value.push_str("; filename*=UTF-8''");
        for byte in name.bytes() {
            if byte.is_ascii_alphanumeric() || b"!#$&+-.^_`|~".contains(&byte) {
                value.push(char::from(byte));
            } else {
                value.push_str(&format!("%{byte:02X}"));
            }
        }
    }
    HeaderValue::from_str(&value).unwrap_or_else(|_| HeaderValue::from_static("attachment"))
}

/// The peer the listener accepted the request from.
pub(crate) fn remote(extensions: &Extensions) -> Option<SocketAddr> {
    extensions
        .get::<ConnectInfo<PeerAddr>>()
        .map(|c| c.0.0)
        .or_else(|| extensions.get::<ConnectInfo<SocketAddr>>().map(|c| c.0))
}

/// What both routes read from their request.
struct Asked {
    stream: Arc<Stream>,
    tag: ConsumerTag,
    remote: Option<SocketAddr>,
    filename: Option<HeaderValue>,
    shutdown: CancellationToken,
}

fn asked(state: &ApiState, request: &Request, query: &Query) -> Result<Asked, Fail> {
    let stream = query.stream(state)?;
    let remote = remote(request.extensions());
    let class = ConsumerClass::from_query(request.uri().query().unwrap_or_default())
        .map_err(|e| text(StatusCode::BAD_REQUEST, e.to_string()))?
        .unwrap_or_default();
    let mut tag = ConsumerTag::new(CONSUMER_KIND).with_class(class);
    if let Some(remote) = remote {
        tag = tag.with_remote(remote.to_string());
    }
    if let Some(agent) = request.headers().get(header::USER_AGENT).and_then(|v| v.to_str().ok()) {
        tag = tag.with_user_agent(agent);
    }
    let filename = query.first("filename").map(disposition);
    let shutdown = request.extensions().get::<Shutdown>().map_or_else(CancellationToken::new, |s| s.0.clone());
    Ok(Asked { stream, tag, remote, filename, shutdown })
}

fn slot(state: &ApiState, remote: Option<SocketAddr>) -> Result<Slot, Fail> {
    state.mp4.try_open(remote).ok_or_else(|| text(StatusCode::SERVICE_UNAVAILABLE, "too many mp4 responses"))
}

/// The feed until its first fragment, with the MIME type and the init
/// segment that fragment is written under. Nothing is sent before it, so
/// tracks that come again first replace both: a camera whose parameter sets
/// in band are not its description's has its track list published again at
/// its first keyframe, and a file headed by the description's would be
/// that init segment alone (another init segment ends a file) or, for
/// `frame.mp4`, a picture under parameter sets it does not use.
async fn head(feed: &mut Feed) -> Result<(String, Bytes, Bytes), String> {
    let (mut mime, mut init) = (None, None);
    loop {
        let out = match feed.next().await {
            Step::Failed(reason) => return Err(reason),
            Step::Send(out) => out,
        };
        for item in out {
            match item {
                Out::Mime(told) => {
                    mime = Some(told);
                    feed.untell();
                }
                Out::Init(segment) => init = Some(segment),
                Out::Binary(fragment) => {
                    if let (Some(mime), Some(init)) = (mime.take(), init.take()) {
                        return Ok((mime, init, fragment));
                    }
                }
            }
        }
    }
}

fn headers(mut response: Response, mime: &str, filename: Option<HeaderValue>) -> Response {
    let headers = response.headers_mut();
    if let Ok(mime) = HeaderValue::from_str(mime) {
        headers.insert(header::CONTENT_TYPE, mime);
    }
    if let Some(filename) = filename {
        headers.insert(header::CONTENT_DISPOSITION, filename);
    }
    response
}

fn failed(name: &str, what: &str, reason: &str) -> Fail {
    let fail = text(StatusCode::INTERNAL_SERVER_ERROR, reason);
    tracing::warn!(stream = %name, "api: {what} failed: {}", fail.message());
    fail
}

/// `GET /api/stream.mp4`.
pub(crate) async fn stream_mp4(State(state): State<ApiState>, request: Request) -> Reply {
    let query = Query::of(request.uri())?;
    let asked = asked(&state, &request, &query)?;
    let raw = request.uri().query().unwrap_or_default();
    let wish = stream_wish(raw).map_err(|e| text(StatusCode::BAD_REQUEST, e))?;
    let until = duration(query.first("duration")).map_err(|e| text(StatusCode::BAD_REQUEST, e))?;
    let slot = slot(&state, asked.remote)?;
    let name = asked.stream.name().to_owned();
    let mut feed = Feed::start(asked.stream, wish, asked.tag, "mp4");
    let (mime, init, first) = head(&mut feed).await.map_err(|reason| failed(&name, "stream.mp4", &reason))?;
    tracing::debug!(stream = %name, %mime, "api: stream.mp4 started");
    let mut body = Progressive {
        feed,
        queued: VecDeque::from([init]),
        until,
        ends_at: None,
        silence: state.mp4_limits.silence,
        shutdown: asked.shutdown,
        stream: name,
        _slot: slot,
    };
    // The first fragment is at zero, before any `duration`.
    body.fragment(first);
    let chunks = futures_util::stream::unfold(body, Progressive::next);
    Ok(headers(Body::from_stream(chunks).into_response(), &mime, asked.filename))
}

/// A `stream.mp4` body: the init segment, then fragments as the feed gives
/// them. Dropped (the client gone, the server stopping), it ends the
/// subscription.
struct Progressive {
    feed: Feed,
    queued: VecDeque<Bytes>,
    /// `duration`: the media this response carries at most.
    until: Option<Duration>,
    /// `duration` again on the wall clock, from the first fragment, with
    /// [`LATE_GRACE`]: for a stream whose media runs slower than the clock.
    ends_at: Option<Instant>,
    silence: Duration,
    shutdown: CancellationToken,
    stream: String,
    _slot: Slot,
}

impl Progressive {
    async fn next(mut self) -> Option<(Result<Bytes, Infallible>, Self)> {
        loop {
            if let Some(bytes) = self.queued.pop_front() {
                return Some((Ok(bytes), self));
            }
            let ends_at = self.ends_at;
            let ended = async move {
                match ends_at {
                    Some(at) => tokio::time::sleep_until(at).await,
                    None => std::future::pending().await,
                }
            };
            let step = tokio::select! {
                () = self.shutdown.cancelled() => return None,
                () = ended => return self.end("its duration passed"),
                step = tokio::time::timeout(self.silence, self.feed.next()) => match step {
                    Ok(step) => step,
                    Err(_) => return self.end("nothing to write for too long"),
                },
            };
            match step {
                Step::Failed(reason) => return self.end(&reason),
                Step::Send(out) => {
                    for item in out {
                        match item {
                            Out::Binary(fragment) => {
                                if !self.fragment(fragment) {
                                    return self.end("its duration was written");
                                }
                            }
                            // One file has one init segment: tracks that
                            // change end it, as a whole file.
                            Out::Init(_) | Out::Mime(_) => return self.end("the stream's tracks changed"),
                        }
                    }
                }
            }
        }
    }

    /// Queues `fragment`, the feed's latest; `false` when it is at or past
    /// `duration`, which the response then ends before.
    fn fragment(&mut self, fragment: Bytes) -> bool {
        if let Some(until) = self.until {
            if self.feed.latest() >= until {
                return false;
            }
            self.ends_at.get_or_insert_with(|| Instant::now() + until + LATE_GRACE);
        }
        self.queued.push_back(fragment);
        true
    }

    fn end<T>(self, why: &str) -> Option<T> {
        tracing::debug!(stream = %self.stream, "api: stream.mp4 ended: {why}");
        None
    }
}

/// `GET /api/frame.mp4`: the stream's latest keyframe (at once from the
/// cached group of pictures when the source runs, else its next one) as an
/// init segment and one fragment, with its length.
pub(crate) async fn frame_mp4(State(state): State<ApiState>, request: Request) -> Reply {
    let query = Query::of(request.uri())?;
    let asked = asked(&state, &request, &query)?;
    let _slot = slot(&state, asked.remote)?;
    let name = asked.stream.name().to_owned();
    let mut feed = Feed::start(asked.stream, frame_wish(), asked.tag, "mp4");
    let wait = state.mp4_limits.frame_timeout;
    let made = tokio::time::timeout(wait, head(&mut feed)).await;
    drop(feed);
    let (mime, init, fragment) = match made {
        Ok(Ok(made)) => made,
        Ok(Err(reason)) => return Err(failed(&name, "frame.mp4", &reason)),
        Err(_) => return Err(failed(&name, "frame.mp4", &format!("no keyframe within {wait:?}"))),
    };
    let mut file = BytesMut::with_capacity(init.len() + fragment.len());
    file.extend_from_slice(&init);
    file.extend_from_slice(&fragment);
    Ok(headers(file.freeze().into_response(), &mime, asked.filename))
}

#[cfg(test)]
#[path = "mp4_tests.rs"]
mod tests;
