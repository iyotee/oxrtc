//! The built-in `stream:` scheme: a source that is another stream of the same
//! hub, read through a filter.
//!
//! `stream:camera_1_src?video` is what go2rtc spells
//! `rtsp://127.0.0.1:8554/camera_1_src?video`; the RTSP factory turns such a
//! loopback URL into the same thing. Either is an **alias**, not a source of
//! its own: nothing runs it. When a consumer subscribes, the wish it has for
//! each media is narrowed by the alias's query and resolved against the
//! sources of the stream it names (`crate::select`), so the consumer reads
//! that stream's own source - its ring, its GOP cache, its supervisor and its
//! timeline. A frame is never copied, published again, buffered again, or
//! retried by a second backoff on the way.
//!
//! An alias names a stream, resolved at each selection: the stream it names
//! may be created later, or replaced, and the next consumer reads the current
//! one (a subscription already reading one that is replaced moves to its
//! successor). A stream that reads itself, directly (`a: stream:a`, refused
//! when the alias is built) or through others (`a -> b -> a`, refused by
//! [`crate::Hub::upsert`]), is refused.

use std::sync::Arc;

use tokio_util::sync::CancellationToken;

use crate::channel::ProducerSink;
use crate::driver::{BoxFuture, FactoryCtx, SourceDriver, SourceError, SourceFactory};
use crate::error::SpecError;
use crate::hub::validate_name;
use crate::spec::SourceSpec;
use crate::want::Want;

pub(crate) struct LocalFactory;

impl SourceFactory for LocalFactory {
    fn schemes(&self) -> &[&'static str] {
        &["stream"]
    }

    fn build(&self, spec: &SourceSpec, ctx: &FactoryCtx) -> Result<Arc<dyn SourceDriver>, SpecError> {
        let (name, query) = spec.target().split_once('?').unwrap_or((spec.target(), ""));
        LocalDriver::build(ctx.stream_name(), name, query)
    }
}

/// Another stream of the same hub, and what of it passes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Alias {
    target: String,
    want: Want,
}

impl Alias {
    /// The stream it names.
    pub fn target(&self) -> &str {
        &self.target
    }

    /// What it passes of that stream (go2rtc's query: `video`, `audio=opus`...).
    pub fn want(&self) -> &Want {
        &self.want
    }
}

/// The driver form of an [`Alias`], which is what a [`SourceFactory`] hands
/// the hub. The hub reads [`SourceDriver::alias`] and never runs it.
#[derive(Debug)]
pub struct LocalDriver {
    alias: Alias,
}

impl LocalDriver {
    /// An alias for stream `owner` onto `target`, with the go2rtc query
    /// `query` (`video`, `audio=opus`...).
    pub fn build(owner: &str, target: &str, query: &str) -> Result<Arc<dyn SourceDriver>, SpecError> {
        validate_name(target).map_err(|e| SpecError::Invalid(e.to_string()))?;
        if target == owner {
            return Err(SpecError::SelfReference);
        }
        let want = Want::from_query(query).map_err(|e| SpecError::Invalid(e.to_string()))?;
        Ok(Arc::new(Self { alias: Alias { target: target.to_owned(), want } }))
    }
}

impl SourceDriver for LocalDriver {
    fn local(&self) -> bool {
        true
    }

    fn alias(&self) -> Option<&Alias> {
        Some(&self.alias)
    }

    fn run(
        self: Arc<Self>,
        _sink: ProducerSink,
        _cancel: CancellationToken,
    ) -> BoxFuture<'static, Result<(), SourceError>> {
        // Unreachable through a hub, which reads the stream named instead.
        Box::pin(async { Err(SourceError::Unsupported("an alias is read through the stream it names".into())) })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::want::MediaWant;

    #[test]
    fn an_alias_names_its_stream_and_its_filter() {
        let driver = LocalDriver::build("camera_1", "camera_1_src", "video").unwrap();
        let alias = driver.alias().unwrap();
        assert_eq!(alias.target(), "camera_1_src");
        assert_eq!((&alias.want().video, &alias.want().audio), (&MediaWant::Any, &MediaWant::Skip));
        assert!(driver.local(), "its failures are this server's own");
    }

    #[tokio::test]
    async fn an_alias_run_on_its_own_publishes_nothing() {
        let driver = LocalDriver::build("a", "b", "").unwrap();
        let channel = crate::channel::Channel::new(&crate::Limits::default(), tokio::time::Instant::now());
        let ended = driver.run(ProducerSink::new(channel.clone()), CancellationToken::new()).await;
        assert!(matches!(ended, Err(SourceError::Unsupported(_))), "{ended:?}");
        assert!(channel.tracks().is_none());
    }

    #[test]
    fn an_alias_of_itself_or_of_a_bad_name_is_refused() {
        assert_eq!(LocalDriver::build("a", "a", "").err(), Some(SpecError::SelfReference));
        assert!(matches!(LocalDriver::build("a", "b c", ""), Err(SpecError::Invalid(_))));
        assert!(matches!(LocalDriver::build("a", "b", "video=vp9"), Err(SpecError::Invalid(_))));
    }
}
