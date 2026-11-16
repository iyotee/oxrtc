//! TLS for `https://` and `rtsps://` sources: rustls with the operating
//! system's trusted certificates, loaded once on the first source that asks
//! for verification, plus the certificates the configuration adds (a private
//! authority). A verified connection checks the certificate against the host
//! the URL names; an unverified one takes any (go2rtc verifies nothing: a
//! camera with a certificate of its own that works there has to be able to
//! work here). Which a source gets by default is the caller's decision (D19
//! for `https`, D20 for `rtsps`), not this module's.

use std::sync::{Arc, OnceLock};
use std::time::Duration;

use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::CryptoProvider;
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{ClientConfig, DigitallySignedStruct, RootCertStore, SignatureScheme};
use tokio::net::TcpStream;
use tokio_rustls::TlsConnector;

pub use tokio_rustls::client::TlsStream;

/// The trust of a factory's TLS sources.
pub struct Tls {
    /// Trusted besides the system's.
    extra_roots: Vec<CertificateDer<'static>>,
    verified: OnceLock<Result<TlsConnector, String>>,
    unverified: OnceLock<Result<TlsConnector, String>>,
}

impl std::fmt::Debug for Tls {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Tls").field("extra_roots", &self.extra_roots.len()).finish_non_exhaustive()
    }
}

impl Tls {
    /// A trust made of the system's roots and `extra_roots` (DER certificates).
    pub fn new(extra_roots: Vec<Vec<u8>>) -> Self {
        Self {
            extra_roots: extra_roots.into_iter().map(CertificateDer::from).collect(),
            verified: OnceLock::new(),
            unverified: OnceLock::new(),
        }
    }

    /// The connector, built on first use: one that checks the certificate or
    /// one that takes any. `Err` says why there are no roots.
    fn connector(&self, verify: bool) -> Result<TlsConnector, String> {
        if verify {
            self.verified.get_or_init(|| self.build()).clone()
        } else {
            self.unverified.get_or_init(Self::build_unverified).clone()
        }
    }

    /// Runs the client handshake over `tcp`, for the server named `host`,
    /// within `timeout`; `verify` says whether the certificate is checked.
    /// `Err` is a sentence for the operator, that never quotes the URL:
    /// "TLS: invalid peer certificate: UnknownIssuer", "the TLS handshake took
    /// over 15s", "TLS: peer closed connection without sending TLS close_notify".
    pub async fn handshake(
        &self,
        host: &str,
        verify: bool,
        tcp: TcpStream,
        timeout: Duration,
    ) -> Result<TlsStream<TcpStream>, String> {
        let connector = self.connector(verify)?;
        let name = server_name(host)?;
        match tokio::time::timeout(timeout, connector.connect(name, tcp)).await {
            Err(_) => Err(format!("the TLS handshake took over {}s", timeout.as_secs().max(1))),
            Ok(Err(e)) => Err(format!("TLS: {e}")),
            Ok(Ok(secured)) => Ok(secured),
        }
    }

    fn build_unverified() -> Result<TlsConnector, String> {
        let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
        let config = ClientConfig::builder_with_provider(provider.clone())
            .with_safe_default_protocol_versions()
            .map_err(|e| format!("TLS protocol versions: {e}"))?
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(AnyCertificate(provider)))
            .with_no_client_auth();
        Ok(TlsConnector::from(Arc::new(config)))
    }

    fn build(&self) -> Result<TlsConnector, String> {
        let loaded = rustls_native_certs::load_native_certs();
        let mut roots = RootCertStore::empty();
        let (added, _ignored) = roots.add_parsable_certificates(loaded.certs);
        let (extra, _ignored) = roots.add_parsable_certificates(self.extra_roots.iter().cloned());
        if added + extra == 0 {
            let why = loaded.errors.first().map_or_else(|| "none found".to_owned(), ToString::to_string);
            return Err(format!("no trusted certificates in the system's store ({why})"));
        }
        let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
        let config = ClientConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .map_err(|e| format!("TLS protocol versions: {e}"))?
            .with_root_certificates(roots)
            .with_no_client_auth();
        Ok(TlsConnector::from(Arc::new(config)))
    }
}

/// A verifier that takes any certificate (`#insecure`, and `rtsps` by default).
///
/// It checks nothing, **the handshake's signatures included**. They prove the
/// peer holds the key of the certificate it sent, which is worth nothing when
/// the certificate itself is taken on faith: whoever sits in the middle
/// presents a certificate of its own and signs with its key. Checking them
/// only turned away cameras that go2rtc (Go's `InsecureSkipVerify`) plays: a
/// self-signed certificate with a 1024-bit RSA key, which rustls refuses to
/// verify a signature of (`invalid peer certificate: BadSignature`), as it
/// does any certificate its parser finds fault with.
#[derive(Debug)]
pub(crate) struct AnyCertificate(Arc<CryptoProvider>);

impl ServerCertVerifier for AnyCertificate {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Ok(HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Ok(HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.0.signature_verification_algorithms.supported_schemes()
    }
}

/// The name a certificate must be valid for: the URL's host.
pub(crate) fn server_name(host: &str) -> Result<ServerName<'static>, String> {
    ServerName::try_from(host.to_owned()).map_err(|_| "the host is not a valid TLS server name".to_owned())
}

#[cfg(test)]
#[path = "tls_tests.rs"]
mod tests;
