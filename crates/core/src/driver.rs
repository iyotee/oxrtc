//! The contract between the core and the protocol crates that produce media.

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Weak};

use thiserror::Error;
use tokio_util::sync::CancellationToken;

use crate::channel::ProducerSink;
use crate::error::SpecError;
use crate::hub::Hub;
use crate::local::{Alias, LocalFactory};
use crate::media::{Codec, MediaKind};
use crate::spec::SourceSpec;

/// A boxed, sendable future.
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// What a source will produce, known without connecting to it.
pub type Declared = Vec<(MediaKind, Codec)>;

/// Why a source run ended.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum SourceError {
    /// The peer could not be reached or refused the session.
    #[error("unreachable: {0}")]
    Unreachable(String),
    /// The peer was rejected our credentials.
    #[error("authentication refused")]
    Auth,
    /// The peer spoke the protocol wrongly.
    #[error("protocol error: {0}")]
    Protocol(String),
    /// The source asks for something this build cannot do; retrying will not help.
    #[error("unsupported: {0}")]
    Unsupported(String),
    /// No frame arrived within the connect timeout.
    #[error("no media within the connect timeout")]
    NoMedia,
    /// No frame arrived within the stall timeout.
    #[error("no media within the stall timeout")]
    Stalled,
    /// The source ended by itself.
    #[error("source ended")]
    Ended,
    /// The core refused what the source published.
    #[error("refused media: {0}")]
    Refused(String),
}

impl SourceError {
    /// Whether reconnecting may help.
    pub fn is_retryable(&self) -> bool {
        !matches!(self, Self::Unsupported(_))
    }
}

/// Runs one source. A driver is built once per stream from its spec and run
/// again at each (re)connection.
pub trait SourceDriver: Send + Sync + 'static {
    /// What the source will produce, when that is known without connecting
    /// (an `ffmpeg:` transcode to H.264 knows its output). `None` makes the
    /// first consumer that needs to know start the source to find out.
    fn declared(&self) -> Option<Declared> {
        None
    }

    /// How long this source may stay silent before it is restarted, when it
    /// differs from [`crate::Limits::stall_timeout`] (a camera URL's
    /// `#timeout=N`). Also stretches the connect timeout to at least this.
    fn stall_timeout(&self) -> Option<std::time::Duration> {
        None
    }

    /// How long the source may take to deliver its first frame, when that is
    /// longer than [`crate::Limits::connect_timeout`] and its stall timeout
    /// (an `ffmpeg:` transcode probes its input, 20 s for a camera that sends
    /// a picture a second, before it encodes one). Past it, the run is
    /// restarted.
    fn start_timeout(&self) -> Option<std::time::Duration> {
        None
    }

    /// Whether the source reads this server itself (another stream, a relay
    /// over the loopback, a transcode of one of its streams). Its failure is
    /// then this server's own answer, which an immediate retry repeats: it is
    /// given up at once, where a camera, which may refuse one connection and
    /// take the next, keeps its chance until the probe deadline.
    fn local(&self) -> bool {
        false
    }

    /// Whether the source is another stream of this hub ([`crate::local`]):
    /// the hub then reads that stream's own sources in its place, resolved at
    /// each selection, and never runs this driver.
    fn alias(&self) -> Option<&Alias> {
        None
    }

    /// Connects and publishes into `sink` until the source ends, fails, or
    /// `cancel` fires. Dropping the returned future must also stop it.
    fn run(
        self: Arc<Self>,
        sink: ProducerSink,
        cancel: CancellationToken,
    ) -> BoxFuture<'static, Result<(), SourceError>>;
}

/// What a factory may use to build a driver.
#[derive(Debug, Clone)]
pub struct FactoryCtx {
    pub(crate) hub: Weak<Hub>,
    pub(crate) stream: String,
}

impl FactoryCtx {
    /// A context for building a driver outside [`Hub::upsert`] (a factory's own tests).
    pub fn new(hub: Weak<Hub>, stream: impl Into<String>) -> Self {
        Self { hub, stream: stream.into() }
    }

    /// The hub, for sources that read other streams.
    pub fn hub(&self) -> Weak<Hub> {
        self.hub.clone()
    }

    /// The name of the stream the source belongs to.
    pub fn stream_name(&self) -> &str {
        &self.stream
    }
}

/// Builds drivers for one or more schemes.
pub trait SourceFactory: Send + Sync + 'static {
    /// The schemes handled (`rtsp`, `rtsps`...).
    fn schemes(&self) -> &[&'static str];

    /// Builds a driver, or says why the spec is unusable. Must not do I/O.
    fn build(&self, spec: &SourceSpec, ctx: &FactoryCtx) -> Result<Arc<dyn SourceDriver>, SpecError>;
}

/// The registry of factories, by scheme. Starts with the built-in `stream:`
/// scheme, which reads another stream of the same hub.
#[derive(Clone)]
pub struct Drivers {
    by_scheme: HashMap<String, Arc<dyn SourceFactory>>,
}

impl Default for Drivers {
    fn default() -> Self {
        let mut drivers = Self { by_scheme: HashMap::new() };
        drivers.register(Arc::new(LocalFactory));
        drivers
    }
}

impl Drivers {
    /// A registry with the built-in schemes only.
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers a factory for its schemes, replacing any earlier one.
    pub fn register(&mut self, factory: Arc<dyn SourceFactory>) {
        for scheme in factory.schemes() {
            self.by_scheme.insert(scheme.to_ascii_lowercase(), factory.clone());
        }
    }

    /// Whether a scheme has a factory.
    pub fn supports(&self, scheme: &str) -> bool {
        self.by_scheme.contains_key(&scheme.to_ascii_lowercase())
    }

    pub(crate) fn build(&self, spec: &SourceSpec, ctx: &FactoryCtx) -> Result<Arc<dyn SourceDriver>, SpecError> {
        let factory =
            self.by_scheme.get(spec.scheme()).ok_or_else(|| SpecError::UnknownScheme(spec.scheme().to_owned()))?;
        factory.build(spec, ctx)
    }
}
