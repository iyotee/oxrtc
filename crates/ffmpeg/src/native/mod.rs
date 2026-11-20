//! The native sound transcode (W5, R33, D37): an `ffmpeg:` source that turns
//! G.711 or L16 into Opus, G.711 or L16 in process, with no FFmpeg.
//!
//! [`plan`] decides from the source alone which ones qualify; the driver
//! then reads its input itself - a stream of this hub through a
//! subscription ([`stream`]), or a WAV stream over HTTP ([`wav`]) - and
//! publishes what `oxrtc-audio` makes of it. Its input is only known once
//! read: one that carries another sound (AAC, Opus), or an HTTP answer that
//! is not a WAV this reader takes, runs FFmpeg instead, the same run, with
//! nothing published before. Everything the core sees is FFmpeg's: the
//! declared output, the timeouts, `local()`, `transcodes()`, the errors
//! (an HTTP 4XX is still [`SourceError::Declined`]).

mod plan;
mod publish;
mod stream;
mod wav;

use std::sync::{Arc, Weak};
use std::time::Duration;

use bytes::Bytes;
use oxrtc_core::{BoxFuture, CancellationToken, Declared, Hub, ProducerSink, SourceDriver, SourceError};

use crate::driver::FfmpegDriver;
pub(crate) use plan::{Input, NativePlan, plan};

/// An HTTP client for the WAV input: the binary hands the webcams' one
/// (`oxrtc_http_client::Bodies`), which this crate does not depend on.
pub trait AudioHttp: Send + Sync + 'static {
    /// The response to a `GET` of `url`, its head within `timeout`.
    fn get(&self, url: String, timeout: Duration) -> BoxFuture<'static, Result<Box<dyn HttpBody>, SourceError>>;
}

/// A response [`AudioHttp`] opened.
pub trait HttpBody: Send {
    /// Its status code.
    fn status(&self) -> u16;
    /// Its `Content-Type`.
    fn content_type(&self) -> Option<&str>;
    /// The next piece of its body; `None` once it has ended.
    fn chunk(&mut self) -> BoxFuture<'_, Option<Result<Bytes, SourceError>>>;
}

/// How a native run ended.
pub(crate) enum Attempt {
    /// It ran: this is the run's result.
    Ran(Result<(), SourceError>),
    /// Its input is not one it transcodes, and nothing was published: FFmpeg
    /// runs instead.
    NotNative(String),
}

/// The driver: the native transcode, FFmpeg's driver behind it.
pub(crate) struct NativeDriver {
    pub(crate) label: String,
    pub(crate) plan: NativePlan,
    pub(crate) ffmpeg: Arc<FfmpegDriver>,
    pub(crate) hub: Weak<Hub>,
    pub(crate) http: Option<Arc<dyn AudioHttp>>,
}

impl SourceDriver for NativeDriver {
    fn declared(&self) -> Option<Declared> {
        self.ffmpeg.declared()
    }

    fn stall_timeout(&self) -> Option<Duration> {
        self.ffmpeg.stall_timeout()
    }

    fn start_timeout(&self) -> Option<Duration> {
        self.ffmpeg.start_timeout()
    }

    fn local(&self) -> bool {
        self.ffmpeg.local()
    }

    fn transcodes(&self) -> bool {
        self.ffmpeg.transcodes()
    }

    fn run(
        self: Arc<Self>,
        sink: ProducerSink,
        cancel: CancellationToken,
    ) -> BoxFuture<'static, Result<(), SourceError>> {
        Box::pin(async move {
            let attempt = match &self.plan.input {
                Input::Stream { name, audio } => stream::run(&self, name, audio, &sink, &cancel).await,
                Input::Wav(url) => wav::run(&self, url, &sink, &cancel).await,
            };
            match attempt {
                Attempt::Ran(result) => result,
                Attempt::NotNative(why) => {
                    tracing::debug!(source = %self.label, "native audio: {why}; FFmpeg instead");
                    self.ffmpeg.run_once(sink, cancel).await
                }
            }
        })
    }
}

impl NativeDriver {
    /// How long the WAV input's server may take to answer: the source's
    /// `#timeout=`, else 15 s.
    fn answer_timeout(&self) -> Duration {
        self.ffmpeg.stall_timeout().unwrap_or(Duration::from_secs(15))
    }
}
