//! The glue between crates that do not know each other: the API's backend
//! traits over WebRTC and FFmpeg, the FFmpeg crate's publication target over
//! the RTSP server, and a factory that is filled in once the RTSP server is
//! bound.

use std::net::SocketAddr;
use std::sync::{Arc, OnceLock};

use bytes::Bytes;
use oxrtc_api::config::IceServer;
use oxrtc_api::{AnswerError, IceSummary, SnapshotError, Snapshotter, WebRtcAnswerer};
use oxrtc_core::spec::redact;
use oxrtc_core::{BoxFuture, FactoryCtx, ProducerSink, SourceDriver, SourceFactory, SourceSpec, SpecError, Stream};
use oxrtc_ffmpeg::{PublishLease, PublishTarget, StillError, Stills};
use oxrtc_rtsp_server::{PublishHandle, Publishers};
use oxrtc_webrtc::{WebRtcServer, WhepError};

/// A factory whose real one is only known later. The `rtsp://` factory needs
/// the RTSP server's port (to recognise its own loopback URLs) and the
/// `ffmpeg:` one its publication tokens, while the server needs the hub,
/// which needs its factories: the hub gets these placeholders, the binary
/// sets them right after binding, before any stream exists.
pub struct LateFactory {
    schemes: &'static [&'static str],
    inner: OnceLock<Arc<dyn SourceFactory>>,
}

impl LateFactory {
    /// A placeholder for `schemes`.
    pub fn new(schemes: &'static [&'static str]) -> Arc<Self> {
        Arc::new(Self { schemes, inner: OnceLock::new() })
    }

    /// Sets the real factory; `false` if it was already set.
    pub fn set(&self, factory: Arc<dyn SourceFactory>) -> bool {
        self.inner.set(factory).is_ok()
    }
}

impl SourceFactory for LateFactory {
    fn schemes(&self) -> &[&'static str] {
        self.schemes
    }

    fn build(&self, spec: &SourceSpec, ctx: &FactoryCtx) -> Result<Arc<dyn SourceDriver>, SpecError> {
        let factory = self.inner.get().ok_or_else(|| SpecError::Invalid("server not ready".into()))?;
        factory.build(spec, ctx)
    }
}

/// Transcoders publish into the RTSP server on one-time tokens.
pub struct ServerTarget(pub Publishers);

struct ServerLease(PublishHandle);

impl PublishLease for ServerLease {
    fn url(&self) -> String {
        self.0.url()
    }
}

impl PublishTarget for ServerTarget {
    fn lease(&self, sink: ProducerSink) -> Box<dyn PublishLease> {
        Box::new(ServerLease(self.0.register(sink)))
    }
}

/// `POST /api/webrtc` answered by the WebRTC server.
pub struct Answerer(pub Arc<WebRtcServer>);

/// The API's view of a WHEP failure. Every reason reaches the caller's
/// answer body (and openIVR's log), so a URL in it is redacted.
pub fn answer_error(error: WhepError) -> AnswerError {
    match error {
        WhepError::BadOffer(reason) => AnswerError::BadOffer(redact(&reason)),
        WhepError::NoMatchingCodec => AnswerError::NoMatchingCodec,
        WhepError::SourceUnavailable(reason) => AnswerError::Unavailable(redact(&reason)),
        WhepError::TooManySessions => AnswerError::Busy,
        WhepError::Internal(reason) => AnswerError::Internal(redact(&reason)),
    }
}

/// `webrtc.ice_servers` as the WebRTC server takes them.
pub fn ice_servers(servers: &[IceServer]) -> Vec<oxrtc_webrtc::IceServer> {
    servers
        .iter()
        .map(|s| oxrtc_webrtc::IceServer {
            urls: s.urls.clone(),
            username: s.username.clone(),
            credential: s.credential.clone(),
        })
        .collect()
}

impl WebRtcAnswerer for Answerer {
    fn set_ice_servers(&self, servers: Vec<IceServer>) -> IceSummary {
        let kept = self.0.set_ice_servers(&ice_servers(&servers));
        IceSummary { turn: kept.turn, stun: kept.stun }
    }

    fn answer(
        &self,
        stream: Arc<Stream>,
        offer_sdp: String,
        remote: Option<SocketAddr>,
        user_agent: Option<String>,
    ) -> BoxFuture<'static, Result<String, AnswerError>> {
        let server = self.0.clone();
        Box::pin(async move {
            server.answer_as(stream, &offer_sdp, remote, user_agent.as_deref()).await.map_err(answer_error)
        })
    }
}

/// `GET /api/frame.jpeg` made by FFmpeg.
pub struct Snapshots(pub Arc<Stills>);

/// The API's view of a still failure: what the stream cannot give is
/// "unavailable", what went wrong on our side is "internal". The reason
/// (FFmpeg's own words included) is redacted like every other answer.
pub fn snapshot_error(error: StillError) -> SnapshotError {
    let reason = redact(&error.to_string());
    match error {
        StillError::Subscribe(_) | StillError::Closed | StillError::Timeout(_) | StillError::Unsupported(_) => {
            SnapshotError::Unavailable(reason)
        }
        StillError::Width
        | StillError::Config(_)
        | StillError::Spawn(_)
        | StillError::Ffmpeg(_)
        | StillError::TooLarge => SnapshotError::Internal(reason),
    }
}

impl Snapshotter for Snapshots {
    fn jpeg(&self, stream: Arc<Stream>, width: Option<u32>) -> BoxFuture<'static, Result<Bytes, SnapshotError>> {
        let stills = self.0.clone();
        Box::pin(async move { stills.jpeg(stream, width).await.map_err(snapshot_error) })
    }
}

#[cfg(test)]
mod tests {
    use oxrtc_core::{Drivers, Hub, Limits, SubscribeError};

    use super::*;

    #[test]
    fn whep_errors_map_one_to_one() {
        assert_eq!(answer_error(WhepError::BadOffer("x".into())), AnswerError::BadOffer("x".into()));
        assert_eq!(answer_error(WhepError::NoMatchingCodec), AnswerError::NoMatchingCodec);
        assert_eq!(answer_error(WhepError::SourceUnavailable("y".into())), AnswerError::Unavailable("y".into()));
        assert_eq!(answer_error(WhepError::TooManySessions), AnswerError::Busy);
        assert_eq!(answer_error(WhepError::Internal("z".into())), AnswerError::Internal("z".into()));
        let leak = WhepError::SourceUnavailable("rtsp://user:hunter2@10.0.0.2/x: refused".into());
        assert!(!format!("{:?}", answer_error(leak)).contains("hunter2"));
    }

    #[test]
    fn still_errors_split_into_unavailable_and_internal() {
        let unavailable = [
            StillError::Subscribe(SubscribeError::NoMatchingSource),
            StillError::Closed,
            StillError::Timeout(std::time::Duration::from_secs(1)),
            StillError::Unsupported("opus"),
        ];
        for error in unavailable {
            assert!(matches!(snapshot_error(error), SnapshotError::Unavailable(_)));
        }
        let internal =
            [StillError::Width, StillError::Spawn("x".into()), StillError::Ffmpeg("y".into()), StillError::TooLarge];
        for error in internal {
            assert!(matches!(snapshot_error(error), SnapshotError::Internal(_)));
        }
        let leak = StillError::Ffmpeg("rtsp://user:hunter2@10.0.0.2/x: 401".into());
        assert!(!format!("{:?}", snapshot_error(leak)).contains("hunter2"));
    }

    #[test]
    fn the_api_refuses_every_width_the_still_maker_would_refuse() {
        // A width the API lets through but the stills refuse would answer 500, not 400.
        assert_eq!(oxrtc_api::MAX_SNAPSHOT_WIDTH, oxrtc_ffmpeg::MAX_WIDTH);
    }

    #[test]
    fn a_late_factory_refuses_until_set_then_delegates() {
        let late = LateFactory::new(&["rtsp", "rtsps"]);
        let mut drivers = Drivers::new();
        drivers.register(late.clone());
        let hub = Hub::new(drivers, Limits::default());
        assert!(hub.upsert("cam", &["rtsp://127.0.0.1:9/x"]).is_err(), "refused before the real factory is set");
        let real = oxrtc_rtsp_client::RtspFactory::new(oxrtc_rtsp_client::RtspClientConfig::default());
        assert!(late.set(Arc::new(real.clone())));
        assert!(!late.set(Arc::new(real)), "set once");
        assert!(hub.upsert("cam", &["rtsp://127.0.0.1:9/x"]).is_ok());
        assert_eq!(late.schemes(), ["rtsp", "rtsps"]);
    }
}
