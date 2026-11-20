//! One `GET` whose body the caller reads as it comes: the WAV stream an
//! `ffmpeg:http://...#audio=...` source names, which the native sound
//! transcode reads instead of FFmpeg (W5, D37). The same connection, TLS,
//! authentication and redirects as a webcam's ([`crate::fetch`]).

use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use http::HeaderValue;
use http::header::CONTENT_TYPE;
use http_body_util::BodyExt;
use hyper::body::Incoming;
use oxrtc_core::{SourceError, SourceSpec};
use oxrtc_net::Tls;

use crate::fetch::Fetcher;

/// Opens bodies with the factory's `User-Agent` and trusted roots
/// ([`crate::HttpFactory::bodies`]).
#[derive(Debug, Clone)]
pub struct Bodies {
    pub(crate) user_agent: HeaderValue,
    pub(crate) tls: Arc<Tls>,
}

/// A response: its status, its `Content-Type`, and its body, read with
/// [`Body::chunk`]. The connection lives as long as it does.
pub struct Body {
    status: u16,
    content_type: Option<String>,
    incoming: Incoming,
    /// Holds the connection; dropping it closes it.
    _fetcher: Fetcher,
}

impl std::fmt::Debug for Body {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Body").field("status", &self.status).field("content_type", &self.content_type).finish()
    }
}

impl Bodies {
    /// The response to a `GET` of `url` (credentials in its user
    /// information are answered to a `401`, as for a webcam), its head
    /// within `timeout`.
    ///
    /// # Errors
    ///
    /// The URL is not an `http(s)` one, the server cannot be reached, does
    /// not answer within `timeout`, or refuses the credentials.
    pub async fn get(&self, url: &str, timeout: Duration) -> Result<Body, SourceError> {
        let spec = SourceSpec::parse(url).map_err(|e| SourceError::Unsupported(e.to_string()))?;
        let target = crate::target::parse(&spec, timeout).map_err(|e| SourceError::Unsupported(e.to_string()))?;
        let mut fetcher = Fetcher::new(
            target.url.clone(),
            target.creds.clone(),
            self.user_agent.clone(),
            timeout,
            self.tls.clone(),
            target.insecure,
        );
        let response = fetcher.get().await?;
        let status = response.status().as_u16();
        let content_type = response.headers().get(CONTENT_TYPE).and_then(|v| v.to_str().ok()).map(str::to_owned);
        Ok(Body { status, content_type, incoming: response.into_body(), _fetcher: fetcher })
    }
}

impl Body {
    /// The response's status code.
    #[must_use]
    pub fn status(&self) -> u16 {
        self.status
    }

    /// The response's `Content-Type`, when it has one that is text.
    #[must_use]
    pub fn content_type(&self) -> Option<&str> {
        self.content_type.as_deref()
    }

    /// The next piece of the body; `None` once it has ended.
    pub async fn chunk(&mut self) -> Option<Result<Bytes, SourceError>> {
        loop {
            match self.incoming.frame().await? {
                Err(error) => return Some(Err(SourceError::Unreachable(oxrtc_core::spec::redact(&error.to_string())))),
                Ok(frame) => {
                    if let Ok(data) = frame.into_data() {
                        return Some(Ok(data));
                    }
                }
            }
        }
    }
}
