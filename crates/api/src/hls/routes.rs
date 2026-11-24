//! The routes of HLS and DASH: `stream.m3u8` and `stream.mpd` open a
//! session (authenticated as every route); the session's own routes under
//! `/api/hls/` are read with its id.

use std::sync::Arc;
use std::time::{Duration, SystemTime};

use axum::Router;
use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use bytes::{Bytes, BytesMut};
use oxrtc_core::{CancellationToken, Codec, ConsumerClass, ConsumerTag, Stream, Want};
use oxrtc_mp4::HevcEntry;

use super::playlist::{self, Kind, View};
use super::session::Session;
use super::window::Settings;
use crate::ApiState;
use crate::fmp4::container::{Mp4, Ts};
use crate::fmp4::{FeedOf, Wish};
use crate::routes::{Fail, Query, Reply, text};

/// go2rtc's names for these consumers, which `/api/streams` lists.
pub const FMP4_KIND: &str = "hls/fmp4";
/// The MPEG-TS one.
pub const TS_KIND: &str = "hls/mpegts";

const M3U8: &str = "application/vnd.apple.mpegurl";
/// The longest the first playlist (or manifest) waits for something to list.
const FIRST_WAIT: Duration = Duration::from_secs(10);
/// The longest `init.mp4` waits for its segment (go2rtc's 3 s).
const INIT_WAIT: Duration = Duration::from_secs(3);
/// The longest `stream.m3u8` and `stream.mpd` wait for the stream's tracks
/// (under the slow routes' deadline).
const OPEN_WAIT: Duration = Duration::from_secs(55);
/// A part number past this is no part a session will write.
const MAX_PART: usize = 4096;

fn with_cors(mut response: Response, content_type: &'static str) -> Response {
    let headers = response.headers_mut();
    headers.insert(header::CONTENT_TYPE, HeaderValue::from_static(content_type));
    // go2rtc's: a Chromecast reads the segments from another origin.
    headers.insert(header::ACCESS_CONTROL_ALLOW_ORIGIN, HeaderValue::from_static("*"));
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    response
}

fn body(chunks: &[Bytes]) -> Body {
    let mut out = BytesMut::with_capacity(chunks.iter().map(Bytes::len).sum());
    for chunk in chunks {
        out.extend_from_slice(chunk);
    }
    Body::from(out.freeze())
}

/// What a session reader asks for: fMP4 as `stream.mp4` reads its query
/// when it names codecs, else go2rtc's MPEG-TS (H.264 or H.265, AAC).
pub(crate) fn wish_of(raw: &str) -> Result<(Kind, Wish), String> {
    let named = form_urlencoded::parse(raw.as_bytes()).any(|(k, _)| k == "mp4" || k == "video" || k == "audio");
    if named {
        return Ok((Kind::Fmp4, crate::mp4::stream_wish(raw)?));
    }
    let want = Want::preferred(&[Codec::H264, Codec::H265, Codec::Aac]);
    Ok((Kind::Ts, Wish { want, hevc: HevcEntry::Hvc1, receivers: None }))
}

/// What opening a session reads from its request (or its socket).
pub(crate) struct Asked {
    pub remote: Option<std::net::SocketAddr>,
    pub agent: Option<String>,
    pub shutdown: CancellationToken,
    pub class: ConsumerClass,
}

/// The stream a request names, and what opening its session reads.
fn asked(state: &ApiState, request: &Request) -> Result<(Arc<Stream>, Asked), Fail> {
    let query = Query::of(request.uri())?;
    let stream = query.stream(state)?;
    let raw = request.uri().query().unwrap_or_default();
    let class =
        ConsumerClass::from_query(raw).map_err(|e| text(StatusCode::BAD_REQUEST, e.to_string()))?.unwrap_or_default();
    let asked = Asked {
        remote: crate::mp4::remote(request.extensions()),
        agent: request.headers().get(header::USER_AGENT).and_then(|v| v.to_str().ok()).map(str::to_owned),
        shutdown: request
            .extensions()
            .get::<crate::ws::Shutdown>()
            .map_or_else(CancellationToken::new, |s| s.0.clone()),
        class,
    };
    Ok((stream, asked))
}

/// Opens a session of `kind` on `stream`, its feed running, and waits for
/// its codecs (`wait` at most).
pub(crate) async fn open(
    state: &ApiState,
    stream: Arc<Stream>,
    asked: Asked,
    kind: Kind,
    wish: Wish,
    wait: Duration,
) -> Result<Arc<Session>, Fail> {
    let remote = asked.remote;
    let name = if kind == Kind::Fmp4 { FMP4_KIND } else { TS_KIND };
    let mut tag = ConsumerTag::new(name).with_class(asked.class).pooled();
    if let Some(remote) = remote {
        tag = tag.with_remote(remote.to_string());
    }
    if let Some(agent) = &asked.agent {
        tag = tag.with_user_agent(agent);
    }
    let stream_name = stream.name().to_owned();
    let session = state
        .hls
        .open(remote, |id, permit| {
            Session::new(id, kind, stream_name.clone(), permit, Settings::default(), state.hls.limits.idle)
        })
        .map_err(|e| text(StatusCode::SERVICE_UNAVAILABLE, e))?;
    let shutdown = asked.shutdown;
    let epoch_wall = SystemTime::now().checked_sub(state.hub.epoch().elapsed()).unwrap_or(SystemTime::UNIX_EPOCH);
    let held = state.hub.output_memory().map(|b| b.hold());
    let (hls, task, id) = (state.hls.clone(), session.clone(), session.id.clone());
    match kind {
        Kind::Fmp4 => {
            let feed = FeedOf::<Mp4>::start(stream, wish, tag, "hls");
            tokio::spawn(async move {
                task.run(feed, epoch_wall, shutdown, held).await;
                hls.remove(&id);
            });
        }
        Kind::Ts => {
            let feed = FeedOf::<Ts>::start(stream, wish, tag, "hls");
            tokio::spawn(async move {
                task.run(feed, epoch_wall, shutdown, held).await;
                hls.remove(&id);
            });
        }
    }
    session.wait(wait, |s| s.codecs.is_some() || s.ended.is_some()).await;
    if let Some(reason) = session.read(|s| if s.codecs.is_some() { None } else { Some(s.ended.clone()) }) {
        state.hls.remove(&session.id);
        let reason = reason.unwrap_or_else(|| "the stream gave no tracks in time".to_owned());
        let fail = text(StatusCode::INTERNAL_SERVER_ERROR, reason);
        tracing::warn!(stream = %session.stream, "api: hls failed: {}", fail.message());
        return Err(fail);
    }
    tracing::debug!(stream = %session.stream, kind = name, "api: hls session started");
    Ok(session)
}

/// `GET /api/stream.m3u8`: go2rtc's master playlist of a new session.
pub(crate) async fn stream_m3u8(State(state): State<ApiState>, request: Request) -> Reply {
    let (kind, wish) =
        wish_of(request.uri().query().unwrap_or_default()).map_err(|e| text(StatusCode::BAD_REQUEST, e))?;
    let (stream, asked) = asked(&state, &request)?;
    drop(request);
    let session = open(&state, stream, asked, kind, wish, OPEN_WAIT).await?;
    let codecs = session.read(|s| s.codecs.clone().unwrap_or_default());
    Ok(with_cors(playlist::master(&session.id, &codecs).into_response(), M3U8))
}

/// `GET /api/stream.mpd`: the manifest of the query's DASH session (opened
/// by the first request, read again by every reload: a DASH client reloads
/// this URL), its segments under `hls/`, once it has a segment; its
/// `Location` names the session's own manifest, read with its id alone.
pub(crate) async fn stream_mpd(State(state): State<ApiState>, request: Request) -> Reply {
    let key = request.uri().query().unwrap_or_default().to_owned();
    let session = match state.hls.dash(&key) {
        Some(session) => session,
        None => {
            let wish = crate::mp4::stream_wish(&key).map_err(|e| text(StatusCode::BAD_REQUEST, e))?;
            let (stream, asked) = asked(&state, &request)?;
            drop(request);
            let session = open(&state, stream, asked, Kind::Fmp4, wish, OPEN_WAIT).await?;
            state.hls.set_dash(key, &session.id);
            session
        }
    };
    session.wait(FIRST_WAIT.max(session.idle()), |s| s.window.last_segment().is_some()).await;
    Ok(with_cors(manifest_of(&session, playlist::MpdAt::Api).into_response(), "application/dash+xml"))
}

fn manifest_of(session: &Session, at: playlist::MpdAt) -> String {
    let now = SystemTime::now();
    session.read(|s| {
        let codecs = s.codecs.clone().unwrap_or_default();
        playlist::mpd(&session.id, &s.window, &codecs, s.zero.unwrap_or(now), now, at)
    })
}

/// The session the query's `id` names.
fn session_of(state: &ApiState, query: &Query) -> Result<Arc<Session>, Fail> {
    let id = query.first("id").ok_or_else(|| text(StatusCode::NOT_FOUND, "hls session not found"))?;
    state.hls.get(id).ok_or_else(|| text(StatusCode::NOT_FOUND, "hls session not found"))
}

fn number(query: &Query, key: &str) -> Result<Option<u64>, Fail> {
    query
        .first(key)
        .map(|v| v.parse::<u64>().map_err(|_| text(StatusCode::BAD_REQUEST, format!("{key} is not a number"))))
        .transpose()
}

/// A wait's bound: three of the session's longest segments, at most the limit.
fn wait_bound(state: &ApiState, session: &Session) -> Duration {
    (session.read(|s| s.window.longest()) * 3).min(state.hls.limits.max_wait)
}

/// Holds a waiting slot while `ready` is awaited.
async fn wait_for(
    state: &ApiState,
    remote: Option<std::net::SocketAddr>,
    session: &Session,
    ready: impl Fn(&super::session::Shared) -> bool,
) -> Result<bool, Fail> {
    if session.read(&ready) {
        return Ok(true);
    }
    let _slot = state
        .hls
        .waiting
        .try_open(remote)
        .ok_or_else(|| text(StatusCode::SERVICE_UNAVAILABLE, "too many hls requests waiting"))?;
    let ready = session.wait(wait_bound(state, session), ready).await;
    session.touch();
    Ok(ready)
}

/// `GET /api/hls/playlist.m3u8?id=`: the media playlist; with `_HLS_msn`
/// (and `_HLS_part`), once that segment (or part) is complete.
async fn media_playlist(State(state): State<ApiState>, request: Request) -> Reply {
    let query = Query::of(request.uri())?;
    let session = session_of(&state, &query)?;
    let remote = crate::mp4::remote(request.extensions());
    drop(request);
    let msn = number(&query, "_HLS_msn")?;
    let part = number(&query, "_HLS_part")?.map(|p| usize::try_from(p).unwrap_or(usize::MAX));
    match (msn, part) {
        (None, Some(_)) => return Err(text(StatusCode::BAD_REQUEST, "_HLS_part without _HLS_msn")),
        (Some(msn), part) => {
            let next = session.read(|s| s.window.next_part().map(|(m, _)| m).or(s.window.last_segment()).unwrap_or(0));
            if msn > next.saturating_add(2) || part.is_some_and(|p| p > MAX_PART) {
                return Err(text(StatusCode::BAD_REQUEST, "_HLS_msn or _HLS_part too far ahead"));
            }
            if !wait_for(&state, remote, &session, |s| s.window.has(msn, part)).await? {
                if session.ended() {
                    return Err(text(StatusCode::NOT_FOUND, "hls session ended"));
                }
                return Err(text(StatusCode::SERVICE_UNAVAILABLE, "the segment asked for did not come in time"));
            }
        }
        (None, None) => {
            // A playlist with a complete segment: a player that reads no
            // part (FFmpeg, VLC) finds nothing to play in one without, and
            // gives up; a Low-Latency one still starts at its newest parts.
            let first = |s: &super::session::Shared| s.window.last_segment().is_some();
            let wait = FIRST_WAIT.min(state.hls.limits.max_wait);
            if !session.read(first) {
                let _slot = state
                    .hls
                    .waiting
                    .try_open(remote)
                    .ok_or_else(|| text(StatusCode::SERVICE_UNAVAILABLE, "too many hls requests waiting"))?;
                session.wait(wait, first).await;
            }
        }
    }
    let text_body = session.read(|s| {
        View { id: &session.id, kind: session.kind, window: &s.window, low_latency: session.low_latency }.media()
    });
    Ok(with_cors(text_body.into_response(), M3U8))
}

/// `GET /api/hls/init.mp4?id=`: the current init segment.
async fn init(State(state): State<ApiState>, request: Request) -> Reply {
    let query = Query::of(request.uri())?;
    let session = session_of(&state, &query)?;
    drop(request);
    session.wait(INIT_WAIT, |s| s.init.is_some()).await;
    let init = session.read(|s| s.init.clone()).ok_or_else(|| text(StatusCode::NOT_FOUND, "no init segment"))?;
    Ok(with_cors(Body::from(init).into_response(), "video/mp4"))
}

fn content_type(kind: Kind) -> &'static str {
    match kind {
        Kind::Fmp4 => "video/iso.segment",
        Kind::Ts => "video/mp2t",
    }
}

/// `GET /api/hls/segment.{m4s,ts}?id=&n=`: a complete segment; the one being
/// written once it is complete.
async fn segment(State(state): State<ApiState>, request: Request) -> Reply {
    let query = Query::of(request.uri())?;
    let session = session_of(&state, &query)?;
    let remote = crate::mp4::remote(request.extensions());
    drop(request);
    let n = number(&query, "n")?.ok_or_else(|| text(StatusCode::BAD_REQUEST, "n is required"))?;
    let soon = session.read(|s| s.window.open().is_some_and(|o| o.msn == n));
    if soon {
        wait_for(&state, remote, &session, |s| s.window.has(n, None)).await?;
    }
    let chunks = session.read(|s| s.window.segment(n)).ok_or_else(|| text(StatusCode::NOT_FOUND, "no such segment"))?;
    Ok(with_cors(body(&chunks).into_response(), content_type(session.kind)))
}

/// `GET /api/hls/part.{m4s,ts}?id=&n=&p=`: a complete part; the next one (a
/// preload hint) once it is complete.
async fn part(State(state): State<ApiState>, request: Request) -> Reply {
    let query = Query::of(request.uri())?;
    let session = session_of(&state, &query)?;
    let remote = crate::mp4::remote(request.extensions());
    drop(request);
    let n = number(&query, "n")?.ok_or_else(|| text(StatusCode::BAD_REQUEST, "n is required"))?;
    let p = number(&query, "p")?.ok_or_else(|| text(StatusCode::BAD_REQUEST, "p is required"))?;
    let p = usize::try_from(p).unwrap_or(usize::MAX);
    let soon =
        session.read(|s| s.window.next_part().is_some_and(|next| (n, p) >= next && n <= next.0 + 1 && p <= MAX_PART));
    if soon {
        wait_for(&state, remote, &session, |s| s.window.has(n, Some(p))).await?;
    }
    let bytes = session.read(|s| s.window.part(n, p)).ok_or_else(|| text(StatusCode::NOT_FOUND, "no such part"))?;
    Ok(with_cors(Body::from(bytes).into_response(), content_type(session.kind)))
}

/// `GET /api/hls/manifest.mpd?id=`: the session's MPEG-DASH manifest.
async fn manifest(State(state): State<ApiState>, request: Request) -> Reply {
    let query = Query::of(request.uri())?;
    let session = session_of(&state, &query)?;
    drop(request);
    if session.kind != Kind::Fmp4 {
        return Err(text(StatusCode::NOT_FOUND, "no manifest for an MPEG-TS session"));
    }
    Ok(with_cors(manifest_of(&session, playlist::MpdAt::Session).into_response(), "application/dash+xml"))
}

/// The session's routes: no authentication but the session's id.
pub(crate) fn router() -> Router<ApiState> {
    Router::new()
        .route("/api/hls/playlist.m3u8", get(media_playlist))
        .route("/api/hls/init.mp4", get(init))
        .route("/api/hls/segment.m4s", get(segment))
        .route("/api/hls/segment.ts", get(segment))
        .route("/api/hls/part.m4s", get(part))
        .route("/api/hls/part.ts", get(part))
        .route("/api/hls/manifest.mpd", get(manifest))
}
