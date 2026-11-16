//! RTSP over TLS: the connection retina is handed for an `rtsps://` source.
//!
//! retina reads from a TCP socket of its own and has no TLS. The copy of it in
//! `third_party/retina` (D20) takes a [`Connector`] instead: this one connects
//! (every address of the camera's name in turn), runs the TLS handshake, and
//! hands retina the secured stream. retina uses it for the session's
//! connection and, should the session need one, for the fresh connections of
//! its background TEARDOWN, so no plain socket is ever opened to a camera
//! that was named `rtsps`.

use std::future::Future;
use std::io;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use oxrtc_net::Tls;
use retina::client::{ConnectedStream, Connector};
use tokio::time::Instant;
use url::{Host, Url};

use crate::target::DEFAULT_RTSPS_PORT;

/// The share of a source's stall timeout the connection (TCP and TLS together)
/// may take: the rest is left to the session's own bound, so that a camera
/// that stops answering in the middle of the handshake is reported as that,
/// with its reason, and not as a DESCRIBE with no answer.
const CONNECT_SHARE_PERCENT: u32 = 80;

/// Connects to one camera over TLS.
pub(crate) struct TlsConnector {
    tls: Arc<Tls>,
    /// Whether the camera's certificate is checked (`#verify`, D20).
    verify: bool,
    /// Bound on connecting and handshaking.
    budget: Duration,
}

impl TlsConnector {
    pub(crate) fn new(tls: Arc<Tls>, verify: bool, stall_timeout: Duration) -> Self {
        Self { tls, verify, budget: stall_timeout * CONNECT_SHARE_PERCENT / 100 }
    }

    async fn open(&self, url: &Url) -> Result<ConnectedStream, String> {
        let host = match url.host().ok_or("the URL has no host")? {
            Host::Domain(name) => name.to_owned(),
            Host::Ipv4(addr) => addr.to_string(),
            Host::Ipv6(addr) => addr.to_string(),
        };
        let port = port_of(url);
        let started = Instant::now();
        let tcp = oxrtc_net::connect(&host, port, self.budget).await?;
        // RTSP is small requests and answers: do not wait to merge them.
        let _ = tcp.set_nodelay(true);
        let (local_addr, peer_addr) =
            tcp.local_addr().and_then(|l| tcp.peer_addr().map(|p| (l, p))).map_err(|e| e.to_string())?;
        let left = self.budget.saturating_sub(started.elapsed());
        let secured = self.tls.handshake(&host, self.verify, tcp, left).await?;
        Ok(ConnectedStream { stream: Box::new(secured), local_addr, peer_addr })
    }
}

impl Connector for TlsConnector {
    fn connect<'a>(&'a self, url: &'a Url) -> Pin<Box<dyn Future<Output = io::Result<ConnectedStream>> + Send + 'a>> {
        // The reason is a sentence for the operator and carries no URL.
        Box::pin(async move { self.open(url).await.map_err(io::Error::other) })
    }
}

/// The port the URL names, else RTSPS's own.
fn port_of(url: &Url) -> u16 {
    url.port().unwrap_or(DEFAULT_RTSPS_PORT)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_rtsps_url_with_no_port_is_on_322() {
        assert_eq!(port_of(&Url::parse("rtsps://cam/live").unwrap()), 322);
        assert_eq!(port_of(&Url::parse("rtsps://cam:8322/live").unwrap()), 8322);
        assert_eq!(port_of(&Url::parse("rtsps://[::1]/live").unwrap()), 322);
    }
}
