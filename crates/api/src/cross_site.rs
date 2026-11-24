//! Requests a page of another site sends with the credential the browser holds
//! for this server (cross-site request forgery, F7 review).
//!
//! A form posted to `/api/restart`, `/api/exit` or `/api/config` is a "simple"
//! request: the browser sends it, with the Basic credential it keeps for the
//! server, without asking the server first, and the page needs no answer to do
//! harm. go2rtc 1.9.15 checks a page's origin on `/api/ws` alone (`api.origin`).
//! Here every request that may change the server (any method but `GET`,
//! `HEAD` and `OPTIONS`) is held to the same rule as `/api/ws`, and a `GET`
//! whose fetch metadata says another site sent it never sets a stream from its
//! `src` (`crate::streams::get_or_patch`). A request with no `Origin` and no
//! `Sec-Fetch-Site` is a program's, taken as before: what protects the API from
//! programs is its credential.

use axum::extract::{Request, State};
use axum::http::{HeaderMap, Method, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};

use crate::ApiState;
use crate::ws::OriginPolicy;

/// Longest `Origin` or `Host` a refusal quotes, in characters.
const MAX_QUOTED: usize = 128;

/// Refuses `403` a request that may change the server from a page `api.origin`
/// does not let in.
pub(crate) async fn guard(State(state): State<ApiState>, request: Request, next: Next) -> Response {
    let reads = matches!(*request.method(), Method::GET | Method::HEAD | Method::OPTIONS);
    if reads || state.ws_origin.allows(request.headers()) {
        return next.run(request).await;
    }
    let refusal = refusal(request.headers());
    // One limit with the websocket's refusals: a page retrying is one warning.
    if let Some(refused) = state.ws_refused_origins.refused() {
        tracing::warn!(refused, "api: request refused: {refusal}");
    }
    crate::routes::text(StatusCode::FORBIDDEN, refusal).into_response()
}

/// Whether a `GET` was sent by a page of another site, as its fetch metadata
/// says (`Sec-Fetch-Site: cross-site` or `same-site`: an `<img>` or a link of
/// another site's page; a page of another port of this host is `same-site`).
pub(crate) fn sent_by_another_site(state: &ApiState, headers: &HeaderMap) -> bool {
    if state.ws_origin == OriginPolicy::Any {
        return false;
    }
    let site = headers.get("sec-fetch-site").and_then(|v| v.to_str().ok());
    matches!(site, Some("cross-site" | "same-site")) || !state.ws_origin.allows(headers)
}

fn refusal(headers: &HeaderMap) -> String {
    let quote = |name| {
        let value = headers.get(name).map(|v| String::from_utf8_lossy(v.as_bytes()).into_owned()).unwrap_or_default();
        let clean: String = value.chars().filter(|c| c.is_ascii_graphic()).take(MAX_QUOTED).collect();
        if clean.is_empty() { "(none)".to_owned() } else { clean }
    };
    format!(
        "origin not allowed: a page of {} may not change the server of host {} (api.origin); a reverse proxy \
         passes the browser's Host header on, or api.origin: \"*\" lets every page in",
        quote(header::ORIGIN),
        quote(header::HOST)
    )
}
