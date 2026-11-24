//! `/api/streams` and `/api/streams.dot`, as go2rtc 1.9.15 defines them.
//!
//! `GET` lists (one stream with `src`, and with `video`, `audio` or
//! `microphone` a probe that opens it first); `PUT` creates or replaces,
//! `PATCH` sets one source (or makes the name read another stream), `DELETE`
//! removes; `POST` (a source played into a stream, a stream pushed to a
//! URL) is refused until F10 and F12 bring it. The four that change the
//! server answer only while `api.allow_stream_changes` holds (D48). Every
//! source goes through the hub's own parser and drivers: one the API adds is
//! held to the rules of one in the file.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode, Uri, header};
use axum::response::IntoResponse;
use oxrtc_core::{ConsumerTag, Event, Stream, SubEnd, Want};

use crate::ApiState;
use crate::info::stream_info;
use crate::routes::{Fail, Query, Reply, json, text};

/// How long a probe (`GET /api/streams?src=&video=all&audio=all`) waits for
/// the stream's tracks before it answers with what is known.
pub const PROBE_WAIT: Duration = Duration::from_secs(5);

/// A route that changes the server, refused while its permission is off.
pub(crate) fn forbidden(key: &str) -> Fail {
    text(StatusCode::FORBIDDEN, format!("disabled by configuration: set api.{key}: true in the file to allow it"))
}

fn may_change(state: &ApiState) -> Result<(), Fail> {
    if state.permissions.stream_changes { Ok(()) } else { Err(forbidden("allow_stream_changes")) }
}

fn info_of(state: &ApiState, stream: &Arc<Stream>) -> crate::info::StreamInfo {
    stream_info(&stream.snapshot(), state.preloads.medias_of(stream).as_deref())
}

/// `GET /api/streams`: every stream by name, or one (`src`, `404` when
/// absent); with `video`, `audio` or `microphone` (go2rtc's probe) the
/// stream is opened for those media first, at most [`PROBE_WAIT`].
pub(crate) async fn list(State(state): State<ApiState>, headers: HeaderMap, uri: Uri) -> Reply {
    let query = Query::of(&uri)?;
    if query.first("src").is_none() {
        let all: BTreeMap<String, _> =
            state.hub.streams().iter().map(|s| (s.name().to_owned(), info_of(&state, s))).collect();
        return Ok(json(&all));
    }
    let stream = query.stream(&state)?;
    if !query.probes() {
        return Ok(json(&info_of(&state, &stream)));
    }
    let want = Want::from_query(&query.media()).map_err(|e| text(StatusCode::BAD_REQUEST, e.to_string()))?;
    let agent = headers.get(header::USER_AGENT).and_then(|v| v.to_str().ok());
    let tag = match agent {
        Some(agent) => ConsumerTag::new("probe").with_user_agent(agent),
        None => ConsumerTag::new("probe"),
    };
    let mut subscription = stream
        .subscribe(&want, tag)
        .await
        .map_err(|e| text(StatusCode::INTERNAL_SERVER_ERROR, format!("streams: {e}")))?;
    let tracks = tokio::time::timeout(PROBE_WAIT, async {
        loop {
            match subscription.recv().await {
                Ok(Event::Tracks(_)) => return Ok(()),
                Ok(_) => {}
                Err(SubEnd::Closed) => return Err("stream closed"),
                Err(SubEnd::NoTracks) => return Err("no source delivered a wanted track"),
            }
        }
    })
    .await;
    // Listed while it holds the stream, as go2rtc lists its probe.
    let shown = info_of(&state, &stream);
    drop(subscription);
    match tracks {
        Ok(Err(reason)) => Err(text(StatusCode::INTERNAL_SERVER_ERROR, format!("streams: {reason}"))),
        // In time, or not: what is known is the answer.
        Ok(Ok(())) | Err(_) => Ok(json(&shown)),
    }
}

fn set(state: &ApiState, name: &str, sources: &[String]) -> Reply {
    match state.hub.upsert(name, sources) {
        Ok(stream) => {
            tracing::info!(stream = stream.name(), sources = sources.len(), "api: stream set");
            Ok(StatusCode::OK.into_response())
        }
        Err(error) => {
            let fail = text(StatusCode::BAD_REQUEST, error.to_string());
            // The name is not logged: it is unvalidated input.
            tracing::warn!("api: stream refused: {}", fail.message());
            Err(fail)
        }
    }
}

/// `PUT /api/streams?name=&src=[&src=]`: creates or replaces the stream; the
/// same sources again change nothing.
pub(crate) async fn put(State(state): State<ApiState>, uri: Uri) -> Reply {
    may_change(&state)?;
    let query = Query::of(&uri)?;
    let name = query.first("name").ok_or_else(|| text(StatusCode::BAD_REQUEST, "name is required"))?;
    let sources: Vec<String> = query.all("src").into_iter().map(str::to_owned).collect();
    if sources.is_empty() {
        return Err(text(StatusCode::BAD_REQUEST, "src is required"));
    }
    set(&state, name, &sources)?;
    // go2rtc writes the stream into its file; here only when the file lets
    // the API write it (`api.allow_config_write`, D48).
    crate::config_file::persist_stream(&state, name, Some(sources)).await?;
    Ok(StatusCode::OK.into_response())
}

/// `PATCH /api/streams?name=&src=`: go2rtc's update of one stream to one
/// source, created when absent. A `src` that names a stream of this server
/// makes `name` read it (go2rtc links the two names to one stream; here
/// `name` is an alias of it, `stream:<src>`, with the same media).
pub(crate) async fn patch(State(state): State<ApiState>, uri: Uri) -> Reply {
    may_change(&state)?;
    let query = Query::of(&uri)?;
    let name = query.first("name").ok_or_else(|| text(StatusCode::BAD_REQUEST, "name is required"))?;
    let src = query.first("src").ok_or_else(|| text(StatusCode::BAD_REQUEST, "src is required"))?;
    let source = if src != name && oxrtc_core::hub::validate_name(src).is_ok() && state.hub.get(src).is_some() {
        format!("stream:{src}")
    } else {
        src.to_owned()
    };
    set(&state, name, &[source])
}

/// `POST /api/streams?src=&dst=`: go2rtc plays a source into a stream (a
/// sound to a camera's speaker, F12) or pushes a stream to a URL (RTMP,
/// F10); neither exists here yet. Answered as go2rtc answers what it cannot
/// find (`400` without `dst`, `404` when neither names a stream), then
/// `501`.
pub(crate) async fn post(State(state): State<ApiState>, uri: Uri) -> Reply {
    may_change(&state)?;
    let query = Query::of(&uri)?;
    let src = query.first("src").ok_or_else(|| text(StatusCode::BAD_REQUEST, "src is required"))?;
    let dst = query.first("dst").ok_or_else(|| text(StatusCode::BAD_REQUEST, "dst is required"))?;
    if state.hub.get(dst).is_some() {
        return Err(text(
            StatusCode::NOT_IMPLEMENTED,
            "streams: playing a source into a stream is not supported by this server yet",
        ));
    }
    if state.hub.get(src).is_some() {
        return Err(text(
            StatusCode::NOT_IMPLEMENTED,
            "streams: pushing a stream to a URL is not supported by this server yet",
        ));
    }
    Err(text(StatusCode::NOT_FOUND, "stream not found"))
}

/// `DELETE /api/streams?src=`: removes the stream; its consumers end.
pub(crate) async fn delete(State(state): State<ApiState>, uri: Uri) -> Reply {
    may_change(&state)?;
    let query = Query::of(&uri)?;
    let stream = query.stream(&state)?;
    state.preloads.release(stream.name());
    if state.hub.remove(stream.name()) {
        tracing::info!(stream = stream.name(), "api: stream removed");
        crate::config_file::persist_stream(&state, stream.name(), None).await?;
        Ok(StatusCode::OK.into_response())
    } else {
        Err(text(StatusCode::NOT_FOUND, "stream not found"))
    }
}

/// `GET /api/streams.dot[?src=a&src=b]`: the streams as a Graphviz graph,
/// go2rtc's `text/vnd.graphviz` (its `net.html` draws it); a name that is no
/// stream is left out.
pub(crate) async fn dot(State(state): State<ApiState>, uri: Uri) -> Reply {
    let query = Query::of(&uri)?;
    let names = query.all("src");
    let streams: Vec<Arc<Stream>> = if names.is_empty() {
        state.hub.streams()
    } else {
        names.iter().filter_map(|name| state.hub.get(name)).collect()
    };
    let snapshots: Vec<_> = streams.iter().map(|s| s.snapshot()).collect();
    let body = crate::dot::graph(&snapshots);
    Ok(([(header::CONTENT_TYPE, "text/vnd.graphviz")], body).into_response())
}
