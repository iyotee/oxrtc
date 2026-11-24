//! The application's own routes, as go2rtc names them: `GET /api` and
//! `GET /api/schemes`.

use axum::extract::State;
use axum::http::{HeaderMap, header};
use axum::response::Response;

use crate::ApiState;
use crate::routes::json;

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
    out.insert("version".into(), reported_version(&state.version).into());
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
