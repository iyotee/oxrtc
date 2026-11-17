//! The session resource a WHEP offer's answer names in `Location`
//! (`/api/webrtc/<id>`): `PATCH` trickles remote candidates, `DELETE` ends the
//! session (RFC 9725). Both are routes of the same authenticated router, under
//! the same connection limits per address as the offer itself.

use axum::extract::{Path, Request, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};

use crate::ApiState;
use crate::backend::{ResourceError, WebRtcAnswerer};
use crate::routes::{MAX_BODY_BYTES, Reply, text};

/// The media type of a `PATCH` body.
pub const TRICKLE_CONTENT_TYPE: &str = "application/trickle-ice-sdpfrag";

/// The path of the resource of session `id`, as `Location` gives it.
pub(crate) fn location(id: &str) -> String {
    format!("/api/webrtc/{id}")
}

/// An id shaped like one this server makes: anything else cannot name a
/// session, and is `404` before the WebRTC egress is asked.
fn plausible(id: &str) -> bool {
    !id.is_empty() && id.len() <= 64 && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// `path` as a log line may carry it: the id of a session resource is a
/// capability, so the segment after `/api/webrtc/` is elided (`ice` is a route
/// of its own, not an id).
pub(crate) fn loggable(path: &str) -> &str {
    match path.strip_prefix("/api/webrtc/") {
        Some(rest) if rest != "ice" => "/api/webrtc/{id}",
        _ => path,
    }
}

fn refused(error: ResourceError) -> axum::response::Response {
    match error {
        ResourceError::NotFound => text(StatusCode::NOT_FOUND, "session not found"),
        ResourceError::Bad(reason) => text(StatusCode::BAD_REQUEST, reason),
        ResourceError::Unsupported(reason) => text(StatusCode::NOT_IMPLEMENTED, reason),
    }
    .into_response()
}

fn answerer(state: &ApiState) -> Result<std::sync::Arc<dyn WebRtcAnswerer>, crate::routes::Fail> {
    state.answerer.clone().ok_or_else(|| text(StatusCode::NOT_IMPLEMENTED, "webrtc is not enabled"))
}

/// Whether `value` is the trickle media type, parameters (`; charset=...`) and
/// case aside.
fn is_trickle(value: &str) -> bool {
    value.split(';').next().is_some_and(|kind| kind.trim().eq_ignore_ascii_case(TRICKLE_CONTENT_TYPE))
}

/// `PATCH /api/webrtc/<id>`: remote candidates the offer did not carry. `204`.
pub(crate) async fn patch(State(state): State<ApiState>, Path(id): Path<String>, request: Request) -> Reply {
    let answerer = answerer(&state)?;
    let sent = request.headers().get(header::CONTENT_TYPE).and_then(|v| v.to_str().ok()).is_some_and(is_trickle);
    // Refused on the head, the body unread: the listener closes in two steps
    // (`linger.rs`), so the refusal is not lost to a reset whatever the route.
    if !sent {
        return Err(text(StatusCode::UNSUPPORTED_MEDIA_TYPE, format!("the body is {TRICKLE_CONTENT_TYPE}")));
    }
    if !plausible(&id) {
        return Err(text(StatusCode::NOT_FOUND, "session not found"));
    }
    let body = axum::body::to_bytes(request.into_body(), MAX_BODY_BYTES)
        .await
        .map_err(|_| text(StatusCode::PAYLOAD_TOO_LARGE, "fragment too large"))?;
    let fragment = String::from_utf8(body.to_vec()).map_err(|_| text(StatusCode::BAD_REQUEST, "not UTF-8"))?;
    match answerer.trickle(id, fragment).await {
        Ok(()) => Ok(StatusCode::NO_CONTENT.into_response()),
        Err(error) => Ok(refused(error)),
    }
}

/// `DELETE /api/webrtc/<id>`: the session ends at once. `200`.
pub(crate) async fn delete(State(state): State<ApiState>, Path(id): Path<String>) -> Reply {
    let answerer = answerer(&state)?;
    if !plausible(&id) {
        return Err(text(StatusCode::NOT_FOUND, "session not found"));
    }
    match answerer.close(id).await {
        Ok(()) => {
            tracing::debug!("api: webrtc session closed by its client");
            Ok(StatusCode::OK.into_response())
        }
        Err(error) => Ok(refused(error)),
    }
}

/// The headers of a `201` answer to an offer: its media type, the resource of
/// the session, and what that resource takes.
pub(crate) fn created(answer: crate::backend::Answered) -> Response {
    Response::builder()
        .status(StatusCode::CREATED)
        .header(header::CONTENT_TYPE, "application/sdp")
        .header(header::LOCATION, location(&answer.resource))
        .header("Accept-Patch", TRICKLE_CONTENT_TYPE)
        .body(axum::body::Body::from(answer.sdp))
        .unwrap_or_else(|_| text(StatusCode::INTERNAL_SERVER_ERROR, "response").into_response())
}
