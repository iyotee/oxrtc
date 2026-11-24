//! The routes of the go2rtc-compatible API.

use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::extract::{DefaultBodyLimit, Request, State};
use axum::http::{HeaderValue, StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, patch, post, put};
use oxrtc_core::{Stream, Want};
use serde::Serialize;
use tower_http::limit::RequestBodyLimitLayer;
use tower_http::timeout::TimeoutLayer;

use crate::ApiState;
use crate::backend::AnswerError;

/// Largest request body, SDP offers included.
pub const MAX_BODY_BYTES: usize = 64 * 1024;
/// Deadline of most requests.
pub const QUICK_TIMEOUT: Duration = Duration::from_secs(10);
/// Deadline of `/api/webrtc` and `/api/preload`, which wait for sources.
pub const SLOW_TIMEOUT: Duration = Duration::from_secs(60);
/// Most query parameters looked at in one request.
pub(crate) const MAX_QUERY_PAIRS: usize = 64;
/// Longest stream name an answer quotes back, in characters.
const MAX_NAME_SHOWN: usize = 64;
/// Widest snapshot that may be asked for: wider answers `400` here, before any
/// work. The binary pins it to the still maker's own cap with a test.
pub const MAX_SNAPSHOT_WIDTH: u32 = 7680;

pub(crate) type Reply = Result<Response, Fail>;

/// A failed request: its status and a plain-text reason. The reason is always
/// passed through [`oxrtc_core::spec::redact`]: a backend's message (an FFmpeg
/// error, a driver's refusal, a codec name taken from the query) may quote a
/// URL with its user information, and no response may carry one.
#[derive(Debug)]
pub(crate) struct Fail {
    status: StatusCode,
    message: String,
}

impl Fail {
    /// The reason, redacted.
    pub(crate) fn message(&self) -> &str {
        &self.message
    }
}

impl IntoResponse for Fail {
    fn into_response(self) -> Response {
        (self.status, self.message).into_response()
    }
}

pub(crate) fn text(status: StatusCode, message: impl AsRef<str>) -> Fail {
    Fail { status, message: oxrtc_core::spec::redact(message.as_ref()) }
}

pub(crate) fn json<T: Serialize>(value: &T) -> Response {
    match serde_json::to_vec(value) {
        Ok(body) => ([(header::CONTENT_TYPE, HeaderValue::from_static("application/json"))], body).into_response(),
        Err(_) => text(StatusCode::INTERNAL_SERVER_ERROR, "serialization failed").into_response(),
    }
}

/// The decoded query, at most [`MAX_QUERY_PAIRS`] pairs.
pub(crate) struct Query(Vec<(String, String)>);

impl Query {
    pub(crate) fn of(uri: &Uri) -> Result<Self, Fail> {
        let mut pairs = Vec::new();
        for (key, value) in form_urlencoded::parse(uri.query().unwrap_or_default().as_bytes()) {
            if pairs.len() >= MAX_QUERY_PAIRS {
                return Err(text(StatusCode::BAD_REQUEST, "too many query parameters"));
            }
            pairs.push((key.into_owned(), value.into_owned()));
        }
        Ok(Self(pairs))
    }

    pub(crate) fn first(&self, key: &str) -> Option<&str> {
        self.0.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str()).filter(|v| !v.is_empty())
    }

    pub(crate) fn all(&self, key: &str) -> Vec<&str> {
        self.0.iter().filter(|(k, _)| k == key).map(|(_, v)| v.as_str()).filter(|v| !v.is_empty()).collect()
    }

    /// Whether the query names a media (`video`, `audio`, `microphone`, with a
    /// value or not): go2rtc's probe of `GET /api/streams`.
    pub(crate) fn probes(&self) -> bool {
        self.0.iter().any(|(k, _)| matches!(k.as_str(), "video" | "audio" | "microphone"))
    }

    /// The media part of the query (`video`, `audio`), as go2rtc notation.
    pub(crate) fn media(&self) -> String {
        let parts: Vec<String> = self
            .0
            .iter()
            .filter(|(k, _)| k == "video" || k == "audio")
            .map(|(k, v)| if v.is_empty() { k.clone() } else { format!("{k}={v}") })
            .collect();
        parts.join("&")
    }

    pub(crate) fn stream(&self, state: &ApiState) -> Result<Arc<Stream>, Fail> {
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
        .route("/api", get(crate::control::info))
        .route("/api/schemes", get(crate::control::schemes))
        .route("/api/openapi.yaml", get(crate::control::openapi))
        .route(
            "/api/streams",
            get(crate::streams::list)
                .put(crate::streams::put)
                .patch(crate::streams::patch)
                .post(crate::streams::post)
                .delete(crate::streams::delete),
        )
        .route("/api/streams.dot", get(crate::streams::dot))
        .route("/api/log", get(crate::control::log).delete(crate::control::clear_log))
        .route("/api/restart", post(crate::control::restart))
        .route("/api/exit", post(crate::control::exit))
        .route("/api/frame.jpeg", get(crate::snapshot::frame_jpeg))
        .route("/api/webrtc/ice", put(crate::ice::put_ice_servers))
        .route("/api/webrtc/{id}", patch(crate::resource::patch).delete(crate::resource::delete))
        .route("/api/ws", get(crate::ws::upgrade))
        .layer(TimeoutLayer::with_status_code(StatusCode::REQUEST_TIMEOUT, QUICK_TIMEOUT));
    let slow = Router::new()
        .route("/api/preload", get(list_preloads).put(put_preload).delete(delete_preload))
        .route("/api/webrtc", post(webrtc))
        // Their head waits for the source as an answer does; the body of
        // `stream.mp4` and `stream.mjpeg` is under its own bounds (`mp4.rs`),
        // not a deadline.
        .route("/api/stream.mp4", get(crate::mp4::stream_mp4))
        .route("/api/frame.mp4", get(crate::mp4::frame_mp4))
        .route("/api/stream.mjpeg", get(crate::mjpeg::stream_mjpeg))
        // A session's first tracks, as an answer's.
        .route("/api/stream.m3u8", get(crate::hls::stream_m3u8))
        .route("/api/stream.mpd", get(crate::hls::stream_mpd))
        .layer(TimeoutLayer::with_status_code(StatusCode::REQUEST_TIMEOUT, SLOW_TIMEOUT));
    // An HLS session's own routes are read with its id, which only an
    // authenticated `stream.m3u8` (or `/api/ws`) hands out (D47); a
    // blocking reload waits at most `HlsLimits::max_wait`.
    let sessions = crate::hls::router()
        .layer(TimeoutLayer::with_status_code(StatusCode::REQUEST_TIMEOUT, SLOW_TIMEOUT))
        .layer(DefaultBodyLimit::max(MAX_BODY_BYTES))
        .layer(RequestBodyLimitLayer::new(MAX_BODY_BYTES));
    // The configuration file is larger than a request body elsewhere.
    let config = Router::new()
        .route(
            "/api/config",
            get(crate::config_file::get).post(crate::config_file::post).patch(crate::config_file::patch),
        )
        .layer(TimeoutLayer::with_status_code(StatusCode::REQUEST_TIMEOUT, QUICK_TIMEOUT))
        .layer(DefaultBodyLimit::max(crate::config::MAX_CONFIG_BYTES))
        .layer(RequestBodyLimitLayer::new(crate::config::MAX_CONFIG_BYTES));
    quick
        .merge(slow)
        .layer(DefaultBodyLimit::max(MAX_BODY_BYTES))
        .layer(RequestBodyLimitLayer::new(MAX_BODY_BYTES))
        .merge(config)
        .fallback(|| async { text(StatusCode::NOT_FOUND, "not found") })
        .layer(axum::middleware::from_fn_with_state(auth, crate::auth::require))
        .merge(sessions)
        .with_state(state)
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

/// How an offer came, and so how its answer goes back (go2rtc's three
/// forms of `POST /api/webrtc?src=`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OfferForm {
    /// `application/json`: `{"type": "offer", "sdp": ...}` in,
    /// `{"type": "answer", "sdp": ...}` out, `200` (Home Assistant's
    /// `go2rtc-client`, `forward_whep_sdp_offer`).
    Json,
    /// `application/x-www-form-urlencoded`: the offer base64 in `data`, the
    /// answer base64 out, `200`.
    Form,
    /// Anything else: the SDP itself, answered `201` with the session's
    /// resource (WHEP, RFC 9725).
    Sdp,
}

impl OfferForm {
    fn of(headers: &axum::http::HeaderMap) -> Self {
        let kind = headers.get(header::CONTENT_TYPE).and_then(|v| v.to_str().ok()).unwrap_or_default();
        let kind = kind.split(';').next().unwrap_or_default().trim();
        if kind.eq_ignore_ascii_case("application/json") {
            Self::Json
        } else if kind.eq_ignore_ascii_case("application/x-www-form-urlencoded") {
            Self::Form
        } else {
            Self::Sdp
        }
    }

    /// The SDP of the body.
    fn offer(self, body: &[u8]) -> Result<String, Fail> {
        match self {
            Self::Json => {
                #[derive(serde::Deserialize)]
                struct Description {
                    sdp: String,
                }
                let described: Description = serde_json::from_slice(body)
                    .map_err(|_| text(StatusCode::BAD_REQUEST, "the offer is not {\"type\":\"offer\",\"sdp\":...}"))?;
                Ok(described.sdp)
            }
            Self::Form => {
                let data = form_urlencoded::parse(body).find(|(k, _)| k == "data").map(|(_, v)| v.into_owned());
                let data = data.ok_or_else(|| text(StatusCode::BAD_REQUEST, "the form has no data"))?;
                let decoded = base64::Engine::decode(&base64::engine::general_purpose::STANDARD, data.trim())
                    .map_err(|_| text(StatusCode::BAD_REQUEST, "data is not base64"))?;
                String::from_utf8(decoded).map_err(|_| text(StatusCode::BAD_REQUEST, "offer is not UTF-8"))
            }
            Self::Sdp => {
                String::from_utf8(body.to_vec()).map_err(|_| text(StatusCode::BAD_REQUEST, "offer is not UTF-8"))
            }
        }
    }

    /// The answer in the offer's form; the session's resource named in every
    /// form (oxrtc only: go2rtc names none).
    fn answer(self, answered: crate::backend::Answered) -> Response {
        let (kind, body) = match self {
            Self::Sdp => return crate::resource::created(answered),
            Self::Json => ("application/json", serde_json::json!({"type": "answer", "sdp": answered.sdp}).to_string()),
            Self::Form => (
                "application/x-www-form-urlencoded",
                base64::Engine::encode(&base64::engine::general_purpose::STANDARD, answered.sdp.as_bytes()),
            ),
        };
        Response::builder()
            .status(StatusCode::OK)
            .header(header::CONTENT_TYPE, kind)
            .header(header::LOCATION, crate::resource::location(&answered.resource))
            .header("Accept-Patch", crate::resource::TRICKLE_CONTENT_TYPE)
            .body(axum::body::Body::from(body))
            .unwrap_or_else(|_| text(StatusCode::INTERNAL_SERVER_ERROR, "response").into_response())
    }
}

async fn webrtc(State(state): State<ApiState>, request: Request) -> Reply {
    let query = Query::of(request.uri())?;
    let stream = query.stream(&state)?;
    let answerer = state.answerer.clone().ok_or_else(|| text(StatusCode::NOT_IMPLEMENTED, "webrtc is not enabled"))?;
    let remote = crate::mp4::remote(request.extensions());
    // A header that is not text is left out, as one that is absent.
    let user_agent = request.headers().get(header::USER_AGENT).and_then(|v| v.to_str().ok()).map(str::to_owned);
    let form = OfferForm::of(request.headers());
    let body = axum::body::to_bytes(request.into_body(), MAX_BODY_BYTES)
        .await
        .map_err(|_| text(StatusCode::PAYLOAD_TOO_LARGE, "offer too large"))?;
    let offer = form.offer(&body)?;
    let name = stream.name().to_owned();
    match answerer.answer(stream, offer, remote, user_agent).await {
        Ok(answer) => Ok(form.answer(answer)),
        Err(error) => Err(refused_offer(&state, &name, &error)),
    }
}

/// An offer that got no answer, as `POST /api/webrtc` and `/api/ws` say it:
/// its status and redacted reason, logged (a refusal for want of a session
/// slot at most once per interval, a viewer retrying in a loop being one
/// warning).
pub(crate) fn refused_offer(state: &ApiState, name: &str, error: &AnswerError) -> Fail {
    let status = match error {
        AnswerError::BadOffer(_) | AnswerError::NoMatchingCodec(_) => StatusCode::BAD_REQUEST,
        AnswerError::Busy => StatusCode::SERVICE_UNAVAILABLE,
        AnswerError::Unavailable(_) | AnswerError::Internal(_) => StatusCode::INTERNAL_SERVER_ERROR,
    };
    let message = match error {
        AnswerError::Unavailable(reason) => format!("streams: {reason}"),
        other => other.to_string(),
    };
    let fail = text(status, message);
    if *error != AnswerError::Busy {
        tracing::warn!(stream = %name, "api: webrtc failed: {}", fail.message);
    } else if let Some(refused) = state.busy.refused() {
        // A limit, like the listeners' own: one warning per interval.
        tracing::warn!(stream = %name, refused, "api: webrtc failed: {}", fail.message);
    }
    fail
}
