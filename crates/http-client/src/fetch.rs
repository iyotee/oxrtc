//! One webcam's HTTP/1.1 connection: connect (TLS for `https`), answer a
//! `401` with Basic or Digest from the URL's user information, follow
//! redirects, and send the requests one after the other on the connection a
//! camera keeps open.
//!
//! Credentials go to the origin the URL names and to no other: a redirect to
//! another scheme, host or port drops them. Nothing a camera sends is put in
//! an error message but its status code.

use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use http::header::{ACCEPT, AUTHORIZATION, HOST, LOCATION, USER_AGENT, WWW_AUTHENTICATE};
use http::{HeaderValue, Request, Response, StatusCode};
use http_auth::{PasswordClient, PasswordParams};
use http_body_util::Empty;
use hyper::body::Incoming;
use hyper::client::conn::http1::{self, SendRequest};
use hyper_util::rt::TokioIo;
use oxrtc_core::SourceError;
use oxrtc_core::spec::redact;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::task::JoinHandle;
use url::{Position, Url};

use crate::target::Credentials;
use crate::tls::{self, Tls};

/// Most redirects followed for one request.
pub const MAX_REDIRECTS: u8 = 5;

/// Largest response head (status line and headers) accepted.
pub const MAX_HEAD_BYTES: usize = 64 * 1024;

/// Where a connection goes.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Origin {
    tls: bool,
    host: String,
    port: u16,
}

impl Origin {
    fn of(url: &Url) -> Result<Self, SourceError> {
        let tls = url.scheme() == "https";
        let host = match url.host() {
            Some(url::Host::Domain(name)) => name.to_owned(),
            Some(url::Host::Ipv4(ip)) => ip.to_string(),
            Some(url::Host::Ipv6(ip)) => ip.to_string(),
            None => return Err(SourceError::Protocol("a redirect without a host".into())),
        };
        let port = url.port_or_known_default().unwrap_or(if tls { 443 } else { 80 });
        Ok(Self { tls, host, port })
    }
}

/// An open connection: the request sender, and the task that drives the
/// connection, ended with it.
struct Conn {
    origin: Origin,
    sender: SendRequest<Empty<Bytes>>,
    task: JoinHandle<()>,
}

impl Drop for Conn {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// Reads one webcam, one request at a time.
pub(crate) struct Fetcher {
    /// The URL requests go to: the source's, then wherever a redirect led.
    url: Url,
    creds: Option<Credentials>,
    /// The answer to the last challenge, kept for the next requests.
    auth: Option<PasswordClient>,
    conn: Option<Conn>,
    user_agent: HeaderValue,
    /// A connect, and the wait for a response head, may take this long.
    timeout: Duration,
    tls: Arc<Tls>,
    /// `#insecure`: no check of the certificate.
    insecure: bool,
}

fn unreachable(why: impl std::fmt::Display) -> SourceError {
    SourceError::Unreachable(redact(&why.to_string()))
}

impl Fetcher {
    pub(crate) fn new(
        url: Url,
        creds: Option<Credentials>,
        user_agent: HeaderValue,
        timeout: Duration,
        tls: Arc<Tls>,
        insecure: bool,
    ) -> Self {
        Self { url, creds, auth: None, conn: None, user_agent, timeout, tls, insecure }
    }

    /// Connects, bounded by the timeout. TLS, when the origin is `https`.
    async fn connect(&self, origin: Origin) -> Result<Conn, SourceError> {
        let within = |what: &'static str| move |_| unreachable(format!("{what} took over {}s", self.timeout.as_secs()));
        let tcp = crate::connect::connect(&origin.host, origin.port, self.timeout).await.map_err(unreachable)?;
        // A picture is one write of the camera's; do not wait to merge it.
        let _ = tcp.set_nodelay(true);
        if !origin.tls {
            return Self::handshake(origin, tcp).await;
        }
        let connector = self.tls.connector(!self.insecure).map_err(SourceError::Unreachable)?;
        let name = tls::server_name(&origin.host).map_err(SourceError::Unreachable)?;
        let secured = tokio::time::timeout(self.timeout, connector.connect(name, tcp))
            .await
            .map_err(within("the TLS handshake"))?
            .map_err(|e| unreachable(format!("TLS: {e}")))?;
        Self::handshake(origin, secured).await
    }

    async fn handshake<I>(origin: Origin, io: I) -> Result<Conn, SourceError>
    where
        I: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    {
        // Cameras' HTTP is often sloppy: bad headers are dropped, not fatal.
        let (sender, connection) = http1::Builder::new()
            .max_buf_size(MAX_HEAD_BYTES)
            .ignore_invalid_headers_in_responses(true)
            .allow_spaces_after_header_name_in_responses(true)
            .allow_obsolete_multiline_headers_in_responses(true)
            .handshake(TokioIo::new(io))
            .await
            .map_err(unreachable)?;
        let task = tokio::spawn(async move {
            let _ = connection.await;
        });
        Ok(Conn { origin, sender, task })
    }

    /// The request for the current URL, with the answer to the last
    /// challenge when there is one.
    fn request(&mut self) -> Result<Request<Empty<Bytes>>, SourceError> {
        let path = self.url[Position::BeforePath..Position::AfterQuery].to_owned();
        let mut host = self.url.host_str().unwrap_or_default().to_owned();
        if let Some(port) = self.url.port() {
            host.push_str(&format!(":{port}"));
        }
        let mut request = Request::builder()
            .method("GET")
            .uri(&path)
            .header(HOST, host)
            .header(USER_AGENT, self.user_agent.clone())
            .header(ACCEPT, "*/*");
        if let (Some(client), Some(creds)) = (&mut self.auth, &self.creds) {
            let params = PasswordParams {
                username: &creds.username,
                password: &creds.password,
                uri: &path,
                method: "GET",
                body: Some(&[]),
            };
            let answer = client.respond(&params).map_err(|e| SourceError::Protocol(redact(&e)))?;
            let mut value =
                HeaderValue::from_str(&answer).map_err(|_| SourceError::Protocol("authorization".into()))?;
            value.set_sensitive(true);
            request = request.header(AUTHORIZATION, value);
        }
        request.body(Empty::new()).map_err(|_| SourceError::Protocol("a request this server cannot make".into()))
    }

    /// The origin's connection: the open one, or a new one. Whether it is an
    /// old one.
    async fn ensure_connection(&mut self) -> Result<bool, SourceError> {
        let origin = Origin::of(&self.url)?;
        if self.conn.as_ref().is_some_and(|c| c.origin == origin && !c.sender.is_closed()) {
            return Ok(true);
        }
        self.conn = None;
        self.conn = Some(self.connect(origin).await?);
        Ok(false)
    }

    /// The camera's response to a GET of the current URL, its head only: the
    /// body is the caller's to read before the next request.
    ///
    /// Answers a `401` once and follows redirects up to [`MAX_REDIRECTS`].
    pub(crate) async fn get(&mut self) -> Result<Response<Incoming>, SourceError> {
        let (mut answered, mut redirects, mut stale_retried) = (false, 0u8, false);
        loop {
            let reused = self.ensure_connection().await?;
            let request = self.request()?;
            let sent = match self.conn.as_mut() {
                Some(conn) => tokio::time::timeout(self.timeout, async {
                    conn.sender.ready().await?;
                    conn.sender.send_request(request).await
                })
                .await
                .map_err(|_| unreachable(format!("no answer within {}s", self.timeout.as_secs())))?,
                None => return Err(unreachable("no connection")),
            };
            let response = match sent {
                Ok(response) => response,
                // A kept-alive connection the camera closed meanwhile: once more, on a new one.
                Err(_) if reused && !stale_retried => {
                    stale_retried = true;
                    self.conn = None;
                    continue;
                }
                Err(error) => {
                    self.conn = None;
                    return Err(unreachable(error));
                }
            };
            let status = response.status();
            if status == StatusCode::UNAUTHORIZED {
                // The body is not read: the next request goes on a new connection.
                self.conn = None;
                if self.creds.is_none() || answered {
                    return Err(SourceError::Auth);
                }
                answered = true;
                self.auth = Some(challenge(&response)?);
                continue;
            }
            if status.is_redirection()
                && let Some(location) = response.headers().get(LOCATION)
            {
                redirects += 1;
                if redirects > MAX_REDIRECTS {
                    return Err(SourceError::Protocol(format!("more than {MAX_REDIRECTS} redirects")));
                }
                self.redirect(location)?;
                continue;
            }
            return Ok(response);
        }
    }

    /// Follows a redirect: to the same origin on this connection, to another
    /// on a new one and without the credentials.
    fn redirect(&mut self, location: &HeaderValue) -> Result<(), SourceError> {
        let location = location.to_str().map_err(|_| SourceError::Protocol("a redirect that is not text".into()))?;
        let mut next = self
            .url
            .join(location)
            .map_err(|_| SourceError::Protocol("a redirect to something that is not a URL".into()))?;
        if !matches!(next.scheme(), "http" | "https") {
            return Err(SourceError::Protocol("a redirect to something that is not HTTP".into()));
        }
        let _ = (next.set_username(""), next.set_password(None));
        if Origin::of(&next)? != Origin::of(&self.url)? {
            self.creds = None;
            self.auth = None;
            self.conn = None;
        }
        self.url = next;
        Ok(())
    }
}

/// The client for the challenges of a `401`: Digest when offered, else Basic.
fn challenge(response: &Response<Incoming>) -> Result<PasswordClient, SourceError> {
    let mut builder = PasswordClient::builder();
    for value in response.headers().get_all(WWW_AUTHENTICATE) {
        if let Ok(text) = value.to_str() {
            builder = builder.challenges(text);
        }
    }
    builder
        .build()
        .map_err(|_| SourceError::Protocol("the server asks for an authentication this client has not".into()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn origins_are_scheme_host_and_port() {
        let origin = |raw: &str| Origin::of(&Url::parse(raw).unwrap()).unwrap();
        assert_eq!(origin("http://cam/x"), Origin { tls: false, host: "cam".into(), port: 80 });
        assert_eq!(origin("https://cam/x"), Origin { tls: true, host: "cam".into(), port: 443 });
        assert_eq!(origin("http://[::1]:8080/x").host, "::1");
        assert_ne!(origin("http://cam/x"), origin("https://cam/x"));
        assert_ne!(origin("http://cam/x"), origin("http://cam:81/x"));
        assert_eq!(origin("http://cam/x"), origin("http://CAM:80/y?q"));
    }
}
