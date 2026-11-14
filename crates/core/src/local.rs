//! The built-in `stream:` scheme: a source that reads another stream of the
//! same hub, without a network hop.
//!
//! `stream:camera_1_src?video` is what go2rtc spells
//! `rtsp://127.0.0.1:8554/camera_1_src?video`; the RTSP driver turns such a
//! loopback URL into this driver, so a relay of a relay costs a pointer copy,
//! not an RTP round trip.

use std::sync::{Arc, Weak};

use tokio_util::sync::CancellationToken;

use crate::channel::ProducerSink;
use crate::driver::{BoxFuture, FactoryCtx, SourceDriver, SourceError, SourceFactory};
use crate::error::{SpecError, SubEnd};
use crate::frame::Frame;
use crate::hub::{Hub, validate_name};
use crate::spec::SourceSpec;
use crate::stream::ConsumerTag;
use crate::subscription::Event;
use crate::want::Want;

pub(crate) struct LocalFactory;

impl SourceFactory for LocalFactory {
    fn schemes(&self) -> &[&'static str] {
        &["stream"]
    }

    fn build(&self, spec: &SourceSpec, ctx: &FactoryCtx) -> Result<Arc<dyn SourceDriver>, SpecError> {
        let (name, query) = spec.target().split_once('?').unwrap_or((spec.target(), ""));
        LocalDriver::build(ctx.hub(), ctx.stream_name(), name, query)
    }
}

/// Reads `target` of the same hub with a [`Want`].
#[derive(Debug)]
pub struct LocalDriver {
    hub: Weak<Hub>,
    target: String,
    want: Want,
}

impl LocalDriver {
    /// A driver for stream `owner` that reads `target` with the go2rtc query
    /// `query` (`video`, `audio=opus`...).
    pub fn build(hub: Weak<Hub>, owner: &str, target: &str, query: &str) -> Result<Arc<dyn SourceDriver>, SpecError> {
        validate_name(target).map_err(|e| SpecError::Invalid(e.to_string()))?;
        if target == owner {
            return Err(SpecError::SelfReference);
        }
        let want = Want::from_query(query).map_err(|e| SpecError::Invalid(e.to_string()))?;
        Ok(Arc::new(Self { hub, target: target.to_owned(), want }))
    }
}

impl SourceDriver for LocalDriver {
    fn run(
        self: Arc<Self>,
        sink: ProducerSink,
        cancel: CancellationToken,
    ) -> BoxFuture<'static, Result<(), SourceError>> {
        Box::pin(async move {
            let stream = self
                .hub
                .upgrade()
                .and_then(|hub| hub.get(&self.target))
                .ok_or_else(|| SourceError::Unreachable(format!("no stream named {}", self.target)))?;
            let mut subscription = stream
                .subscribe(&self.want, ConsumerTag::new("internal"))
                .await
                .map_err(|e| SourceError::Unreachable(e.to_string()))?;
            loop {
                let event = tokio::select! {
                    biased;
                    () = cancel.cancelled() => return Ok(()),
                    event = subscription.recv() => event,
                };
                match event {
                    Ok(Event::Tracks(tracks)) => {
                        sink.set_tracks(tracks.to_vec()).map_err(|e| SourceError::Refused(e.to_string()))?;
                    }
                    Ok(Event::Frame { track, frame }) => {
                        let track = u8::try_from(track).map_err(|_| SourceError::Refused("track index".into()))?;
                        let frame = Frame { track, ..Frame::clone(&frame) };
                        sink.publish(frame).map_err(|e| SourceError::Refused(e.to_string()))?;
                    }
                    Ok(Event::Discontinuity) => {}
                    Err(SubEnd::Closed) => return Err(SourceError::Unreachable("the stream read was closed".into())),
                }
            }
        })
    }
}
