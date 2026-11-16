//! The routes of the go2rtc-compatible API.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::extract::{ConnectInfo, DefaultBodyLimit, Request, State};
use axum::http::{HeaderValue, StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use oxrtc_core::{Stream, Want};
use serde::Serialize;
use tower_http::limit::RequestBodyLimitLayer;
use tower_http::timeout::TimeoutLayer;

use crate::ApiState;
use crate::backend::AnswerError;
use crate::info::stream_info;
use crate::listener::PeerAddr;

/// Largest request body, SDP offers included.
pub const MAX_BODY_BYTES: usize = 64 * 1024;
/// Deadline of most requests.
pub const QUICK_TIMEOUT: Duration = Duration::from_secs(10);
/// Deadline of `/api/webrtc` and `/api/preload`, which wait for sources.
pub const SLOW_TIMEOUT: Duration = Duration::from_secs(60);
/// Most query parameters looked at in one request.
const MAX_QUERY_PAIRS: usize = 64;
/// Longest stream name an answer quotes back, in characters.
const MAX_NAME_SHOWN: usize = 64;
/// Widest snapshot that may be asked for: wider answers `400` here, before any
/// work. The binary pins it to the still maker's own cap with a test.
pub const MAX_SNAPSHOT_WIDTH: u32 = 7680;

type Reply = Result<Response, Fail>;

/// A failed request: its status and a plain-text reason. The reason is always
/// passed through [`oxrtc_core::spec::redact`]: a backend's message (an FFmpeg
/// error, a driver's refusal, a codec name taken from the query) may quote a
/// URL with its user information, and no response may carry one.
#[derive(Debug)]
struct Fail {
    status: StatusCode,
    message: String,
}

impl IntoResponse for Fail {
    fn into_response(self) -> Response {
        (self.status, self.message).into_response()
    }
}

fn text(status: StatusCode, message: impl AsRef<str>) -> Fail {
    Fail { status, message: oxrtc_core::spec::redact(message.as_ref()) }
}

fn json<T: Serialize>(value: &T) -> Response {
    match serde_json::to_vec(value) {
        Ok(body) => ([(header::CONTENT_TYPE, HeaderValue::from_static("application/json"))], body).into_response(),
        Err(_) => text(StatusCode::INTERNAL_SERVER_ERROR, "serialization failed").into_response(),
    }
}

/// The decoded query, at most [`MAX_QUERY_PAIRS`] pairs.
struct Query(Vec<(String, String)>);

impl Query {
    fn of(uri: &Uri) -> Result<Self, Fail> {
        let mut pairs = Vec::new();
        for (key, value) in form_urlencoded::parse(uri.query().unwrap_or_default().as_bytes()) {
            if pairs.len() >= MAX_QUERY_PAIRS {
                return Err(text(StatusCode::BAD_REQUEST, "too many query parameters"));
            }
            pairs.push((key.into_owned(), value.into_owned()));
        }
        Ok(Self(pairs))
    }

    fn first(&self, key: &str) -> Option<&str> {
        self.0.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str()).filter(|v| !v.is_empty())
    }

    fn all(&self, key: &str) -> Vec<&str> {
        self.0.iter().filter(|(k, _)| k == key).map(|(_, v)| v.as_str()).filter(|v| !v.is_empty()).collect()
    }

    /// The media part of the query (`video`, `audio`), as go2rtc notation.
    fn media(&self) -> String {
        let parts: Vec<String> = self
            .0
            .iter()
            .filter(|(k, _)| k == "video" || k == "audio")
            .map(|(k, v)| if v.is_empty() { k.clone() } else { format!("{k}={v}") })
            .collect();
        parts.join("&")
    }

    fn stream(&self, state: &ApiState) -> Result<Arc<Stream>, Fail> {
        self.stream_or(state, |_| text(StatusCode::NOT_FOUND, "stream not found"))
    }

    /// The stream `src` names; `missing` words the refusal when there is none
    /// (`src` itself absent is always `400`).
    fn stream_or(&self, state: &ApiState, missing: impl FnOnce(&str) -> Fail) -> Result<Arc<Stream>, Fail> {
        let name = self.first("src").ok_or_else(|| text(StatusCode::BAD_REQUEST, "src is required"))?;
        state.hub.get(name).ok_or_else(|| missing(name))
    }
}

/// The API's router: every route, behind authentication, body limit and
/// request deadline.
pub fn router(state: ApiState) -> Router {
    if *state.auth == crate::Auth::Unauthenticated {
        tracing::warn!("api: authentication is disabled by configuration; any local process controls the server");
    }
    let auth = state.auth.clone();
    let quick = Router::new()
        .route("/api", get(version))
        .route("/api/streams", get(list_streams).put(put_stream).delete(delete_stream))
        .route("/api/frame.jpeg", get(frame_jpeg))
        .layer(TimeoutLayer::with_status_code(StatusCode::REQUEST_TIMEOUT, QUICK_TIMEOUT));
    let slow = Router::new()
        .route("/api/preload", get(list_preloads).put(put_preload).delete(delete_preload))
        .route("/api/webrtc", post(webrtc))
        .layer(TimeoutLayer::with_status_code(StatusCode::REQUEST_TIMEOUT, SLOW_TIMEOUT));
    quick
        .merge(slow)
        .fallback(|| async { text(StatusCode::NOT_FOUND, "not found") })
        .layer(DefaultBodyLimit::max(MAX_BODY_BYTES))
        .layer(RequestBodyLimitLayer::new(MAX_BODY_BYTES))
        .layer(axum::middleware::from_fn_with_state(auth, crate::auth::require))
        .with_state(state)
}

async fn version(State(state): State<ApiState>) -> Response {
    json(&serde_json::json!({ "version": &*state.version }))
}

async fn list_streams(State(state): State<ApiState>, uri: Uri) -> Reply {
    let query = Query::of(&uri)?;
    let info = |stream: &Arc<Stream>| stream_info(&stream.snapshot(), state.preloads.medias_of(stream).as_deref());
    if query.first("src").is_some() {
        let stream = query.stream(&state)?;
        return Ok(json(&info(&stream)));
    }
    let all: BTreeMap<String, _> = state.hub.streams().iter().map(|s| (s.name().to_owned(), info(s))).collect();
    Ok(json(&all))
}

async fn put_stream(State(state): State<ApiState>, uri: Uri) -> Reply {
    let query = Query::of(&uri)?;
    let name = query.first("name").ok_or_else(|| text(StatusCode::BAD_REQUEST, "name is required"))?;
    let sources = query.all("src");
    if sources.is_empty() {
        return Err(text(StatusCode::BAD_REQUEST, "src is required"));
    }
    match state.hub.upsert(name, &sources) {
        Ok(stream) => {
            tracing::info!(stream = stream.name(), sources = sources.len(), "api: stream set");
            Ok(StatusCode::OK.into_response())
        }
        Err(error) => {
            let fail = text(StatusCode::BAD_REQUEST, error.to_string());
            // The name is not logged: it is unvalidated input.
            tracing::warn!("api: stream refused: {}", fail.message);
            Err(fail)
        }
    }
}

async fn delete_stream(State(state): State<ApiState>, uri: Uri) -> Reply {
    let query = Query::of(&uri)?;
    let stream = query.stream(&state)?;
    state.preloads.release(stream.name());
    if state.hub.remove(stream.name()) {
        tracing::info!(stream = stream.name(), "api: stream removed");
        Ok(StatusCode::OK.into_response())
    } else {
        Err(text(StatusCode::NOT_FOUND, "stream not found"))
    }
}

async fn list_preloads(State(state): State<ApiState>) -> Response {
    json(&state.preloads.list())
}

async fn put_preload(State(state): State<ApiState>, uri: Uri) -> Reply {
    let query = Query::of(&uri)?;
    // go2rtc answers 500 `streams: stream not found: <name>` here, not 404:
    // openIVR reads a 404 from a preload as "this go2rtc has no preload"
    // (before 1.9.10) and gives up holding cameras until it restarts, so a
    // camera deleted while a hold is asked for must not look like that.
    let stream = query.stream_or(&state, |name| {
        let shown: String = name.chars().filter(|c| !c.is_control()).take(MAX_NAME_SHOWN).collect();
        text(StatusCode::INTERNAL_SERVER_ERROR, format!("streams: stream not found: {shown}"))
    })?;
    let media = query.media();
    let want = Want::from_query(&media).map_err(|e| text(StatusCode::BAD_REQUEST, e.to_string()))?;
    let name = stream.name().to_owned();
    match state.preloads.start(stream, want, media, state.preload_wait).await {
        Ok(medias) => {
            tracing::info!(stream = %name, medias = ?medias, "api: preload held");
            Ok(StatusCode::OK.into_response())
        }
        Err(error) => {
            let fail = text(StatusCode::INTERNAL_SERVER_ERROR, error.to_string());
            tracing::warn!(stream = %name, "api: preload failed: {}", fail.message);
            Err(fail)
        }
    }
}

async fn delete_preload(State(state): State<ApiState>, uri: Uri) -> Reply {
    let query = Query::of(&uri)?;
    let name = query.first("src").ok_or_else(|| text(StatusCode::BAD_REQUEST, "src is required"))?;
    if state.preloads.release(name) {
        tracing::info!(stream = name, "api: preload released");
    }
    Ok(StatusCode::OK.into_response())
}

async fn webrtc(State(state): State<ApiState>, request: Request) -> Reply {
    let query = Query::of(request.uri())?;
    let stream = query.stream(&state)?;
    let answerer = state.answerer.clone().ok_or_else(|| text(StatusCode::NOT_IMPLEMENTED, "webrtc is not enabled"))?;
    let extensions = request.extensions();
    let remote = extensions
        .get::<ConnectInfo<PeerAddr>>()
        .map(|c| c.0.0)
        .or_else(|| extensions.get::<ConnectInfo<SocketAddr>>().map(|c| c.0));
    // A header that is not text is left out, as one that is absent.
    let user_agent = request.headers().get(header::USER_AGENT).and_then(|v| v.to_str().ok()).map(str::to_owned);
    let body = axum::body::to_bytes(request.into_body(), MAX_BODY_BYTES)
        .await
        .map_err(|_| text(StatusCode::PAYLOAD_TOO_LARGE, "offer too large"))?;
    let offer = String::from_utf8(body.to_vec()).map_err(|_| text(StatusCode::BAD_REQUEST, "offer is not UTF-8"))?;
    let name = stream.name().to_owned();
    match answerer.answer(stream, offer, remote, user_agent).await {
        Ok(answer) => Ok(Response::builder()
            .status(StatusCode::CREATED)
            .header(header::CONTENT_TYPE, "application/sdp")
            .body(Body::from(answer))
            .unwrap_or_else(|_| text(StatusCode::INTERNAL_SERVER_ERROR, "response").into_response())),
        Err(error) => {
            let status = match error {
                AnswerError::BadOffer(_) | AnswerError::NoMatchingCodec => StatusCode::BAD_REQUEST,
                AnswerError::Busy => StatusCode::SERVICE_UNAVAILABLE,
                AnswerError::Unavailable(_) | AnswerError::Internal(_) => StatusCode::INTERNAL_SERVER_ERROR,
            };
            let message = match &error {
                AnswerError::Unavailable(reason) => format!("streams: {reason}"),
                other => other.to_string(),
            };
            let fail = text(status, message);
            if error != AnswerError::Busy {
                tracing::warn!(stream = %name, "api: webrtc failed: {}", fail.message);
            } else if let Some(refused) = state.busy.refused() {
                // A limit, like the listeners' own: one warning per interval.
                tracing::warn!(stream = %name, refused, "api: webrtc failed: {}", fail.message);
            }
            Err(fail)
        }
    }
}

async fn frame_jpeg(State(state): State<ApiState>, uri: Uri) -> Reply {
    let query = Query::of(&uri)?;
    let stream = query.stream(&state)?;
    let width = match query.first("width") {
        None => None,
        Some(w) => match w.parse::<u32>() {
            Ok(w) if (1..=MAX_SNAPSHOT_WIDTH).contains(&w) => Some(w),
            _ => return Err(text(StatusCode::BAD_REQUEST, "invalid width")),
        },
    };
    let snapshotter =
        state.snapshotter.clone().ok_or_else(|| text(StatusCode::NOT_IMPLEMENTED, "snapshots are not enabled"))?;
    let name = stream.name().to_owned();
    match snapshotter.jpeg(stream, width).await {
        Ok(jpeg) => Ok(([(header::CONTENT_TYPE, HeaderValue::from_static("image/jpeg"))], jpeg).into_response()),
        Err(error) => {
            let fail = text(StatusCode::INTERNAL_SERVER_ERROR, error.to_string());
            tracing::warn!(stream = %name, "api: snapshot failed: {}", fail.message);
            Err(fail)
        }
    }
}
