//! The application's own routes, as go2rtc names them: `GET /api`,
//! `GET /api/schemes`, `GET` and `DELETE /api/log`, `POST /api/restart` and
//! `POST /api/exit`.

use std::convert::Infallible;
use std::task::Poll;

use axum::body::Body;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};
use bytes::Bytes;
use futures_util::StreamExt as _;

use crate::ApiState;
use crate::routes::{Query, Reply, json, text};
use crate::streams::forbidden;

/// What `POST /api/restart` and `POST /api/exit` ask of the application,
/// sent once their answer is written.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Command {
    /// Stop everything, read the configuration file again, start again, in
    /// the same process.
    Restart,
    /// Stop everything and end the process with this code (0 to 125).
    Exit(u8),
}

/// The go2rtc release whose API this server answers (`docs/COMPATIBILITY.md`):
/// what `GET /api` reports as `version`, so that a client that checks
/// go2rtc's version (Home Assistant's `go2rtc-client` takes 1.9.13 up to, not
/// including, 2.0.0) accepts it.
pub const GO2RTC_API_VERSION: &str = "1.9.15";

/// What `GET /api` says of the running application, set by the binary.
#[derive(Debug, Clone, Default)]
pub(crate) struct AppFacts {
    /// The configuration file the server was started with.
    pub(crate) config_path: Option<std::path::PathBuf>,
    /// The RTSP server's address, as `rtsp.listen` gave it.
    pub(crate) rtsp_listen: Option<String>,
    /// What `version` says (`api.reported_version`).
    pub(crate) reported: crate::config::ReportedVersion,
}

/// Longest `Host` quoted back by `GET /api`, in characters.
const MAX_HOST_SHOWN: usize = 256;

/// `version` of `GET /api`: go2rtc's API version with this server's own as
/// semantic versioning's build metadata (`1.9.15+oxrtc.0.2.0`), as go2rtc
/// marks its own development builds (`1.9.15+dev.7c05066`); a version
/// comparison ignores it, a person reads it.
pub fn reported_version(own: &str) -> String {
    format!("{GO2RTC_API_VERSION}+oxrtc.{own}")
}

/// `GET /api`: go2rtc's facts (`version`, `host`, `config_path`,
/// `rtsp.listen`) and this server's own (`oxrtc_version`, and the output
/// memory budget of the readers that write over HTTP or a WebSocket, D45).
pub(crate) async fn info(State(state): State<ApiState>, headers: HeaderMap) -> Response {
    let mut out = serde_json::Map::new();
    let version = match state.facts.reported {
        crate::config::ReportedVersion::Go2rtc => reported_version(&state.version),
        crate::config::ReportedVersion::Oxrtc => state.version.to_string(),
    };
    out.insert("version".into(), version.into());
    out.insert("oxrtc_version".into(), (*state.version).into());
    if let Some(host) = headers.get(header::HOST).and_then(|h| h.to_str().ok()) {
        let shown: String = host.chars().filter(|c| !c.is_control()).take(MAX_HOST_SHOWN).collect();
        out.insert("host".into(), shown.into());
    }
    if let Some(path) = &state.facts.config_path {
        out.insert("config_path".into(), path.display().to_string().into());
    }
    if let Some(listen) = &state.facts.rtsp_listen {
        out.insert("rtsp".into(), serde_json::json!({ "listen": listen }));
    }
    if let Some(memory) = state.hub.output_memory().map(|b| b.snapshot()) {
        out.insert(
            "output_memory".into(),
            serde_json::json!({
                "budget": memory.budget,
                "held": memory.held,
                "readers": memory.readers,
                "skips": memory.skips,
                "limit": memory.reading.map(|r| r.limit),
                "available": memory.reading.map(|r| r.available),
                "source": oxrtc_core::memory::source_name(memory.reading),
            }),
        );
    }
    json(&out)
}

/// `GET /api/schemes`: every scheme a source may have here, sorted (go2rtc
/// lists its own in map order). Home Assistant asks whether a camera's
/// source is one of them before it hands the camera over.
pub(crate) async fn schemes(State(state): State<ApiState>) -> Response {
    json(&state.hub.schemes())
}

/// This server's own OpenAPI description, its version filled in.
const OPENAPI: &str = include_str!("openapi.yaml");

/// `GET /api/openapi.yaml`: this server's OpenAPI 3.1 description (go2rtc
/// publishes its own at that path of its web site, not from the server).
pub(crate) async fn openapi(State(state): State<ApiState>) -> Response {
    let text = OPENAPI.replace("{{version}}", &state.version).replace("{{go2rtc}}", GO2RTC_API_VERSION);
    ([(header::CONTENT_TYPE, "application/yaml")], text).into_response()
}

/// `GET /api/log`: the newest log lines, go2rtc's JSON lines.
pub(crate) async fn log(State(state): State<ApiState>) -> Reply {
    let ring = state.logs.as_ref().ok_or_else(|| text(StatusCode::NOT_IMPLEMENTED, "no log is kept in memory"))?;
    Ok(([(header::CONTENT_TYPE, "application/jsonlines")], ring.text()).into_response())
}

/// `DELETE /api/log`: forgets them (the process's own output is untouched).
pub(crate) async fn clear_log(State(state): State<ApiState>) -> Reply {
    let ring = state.logs.as_ref().ok_or_else(|| text(StatusCode::NOT_IMPLEMENTED, "no log is kept in memory"))?;
    ring.clear();
    tracing::info!("api: log cleared");
    Ok(([(header::CONTENT_TYPE, "text/plain")], "OK").into_response())
}

/// An answer whose body, once written out (or given up on), sends
/// `command`: the client has its answer before the server stops.
fn then(state: &ApiState, command: Command) -> Result<Response, crate::routes::Fail> {
    let commands = state
        .commands
        .clone()
        .ok_or_else(|| text(StatusCode::NOT_IMPLEMENTED, "this server cannot be restarted or stopped from its API"))?;
    struct Send(Option<(tokio::sync::mpsc::Sender<Command>, Command)>);
    impl Drop for Send {
        fn drop(&mut self) {
            if let Some((commands, command)) = self.0.take() {
                // A full queue already holds a restart or an exit.
                let _ = commands.try_send(command);
            }
        }
    }
    let mut on_end = Some(Send(Some((commands, command))));
    let body = futures_util::stream::once(async { Ok::<_, Infallible>(Bytes::from_static(b"OK")) }).chain(
        futures_util::stream::poll_fn(move |_| {
            // Dropped once the body has been read to its end.
            drop(on_end.take());
            Poll::Ready(None::<Result<Bytes, Infallible>>)
        }),
    );
    Ok(([(header::CONTENT_TYPE, "text/plain")], Body::from_stream(body)).into_response())
}

/// `POST /api/restart`: the configuration file read again and every
/// listener started again, in this process (go2rtc replaces its process,
/// which Windows cannot do: there its restart answers and does nothing).
/// The file is checked first: one that would not start is refused here,
/// and the running server goes on.
pub(crate) async fn restart(State(state): State<ApiState>) -> Reply {
    if !state.permissions.restart {
        return Err(forbidden("allow_restart"));
    }
    if let Some(path) = state.facts.config_path.clone() {
        let checked = tokio::task::spawn_blocking(move || {
            crate::config::load(&path).and_then(|config| crate::Auth::from_config(&config.api).map(|_| ()))
        })
        .await;
        match checked {
            Ok(Ok(())) => {}
            Ok(Err(error)) => {
                return Err(text(
                    StatusCode::BAD_REQUEST,
                    format!("restart refused, the file would not start: {error}"),
                ));
            }
            Err(_) => return Err(text(StatusCode::INTERNAL_SERVER_ERROR, "restart check failed")),
        }
    }
    tracing::warn!("api: restart asked (POST /api/restart)");
    then(&state, Command::Restart)
}

/// `POST /api/exit?code=N`: the server stops and the process ends with
/// `N`, 0 to 125 as go2rtc takes it - once the answer is written (go2rtc
/// ends at once and the client's connection is reset).
pub(crate) async fn exit(State(state): State<ApiState>, uri: Uri) -> Reply {
    if !state.permissions.exit {
        return Err(forbidden("allow_exit"));
    }
    let query = Query::of(&uri)?;
    let code = query
        .first("code")
        .and_then(|c| c.parse::<u8>().ok())
        .filter(|c| *c <= 125)
        .ok_or_else(|| text(StatusCode::BAD_REQUEST, "Code must be in the range [0, 125]"))?;
    tracing::warn!(code, "api: exit asked (POST /api/exit)");
    then(&state, Command::Exit(code))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_reported_version_is_one_home_assistant_accepts() {
        let version = reported_version("0.2.0");
        assert_eq!(version, "1.9.15+oxrtc.0.2.0");
        // go2rtc-client 0.4.0: 1.9.13 <= version < 2.0.0, build metadata
        // ignored in the comparison (semantic versioning, AwesomeVersion).
        let core = version.split('+').next().unwrap_or_default();
        let parts: Vec<u32> = core.split('.').map(|p| p.parse().unwrap_or(0)).collect();
        assert!(parts >= vec![1, 9, 13] && parts < vec![2, 0, 0], "{version}");
    }
}
