//! TLS for `https://` sources: rustls with the operating system's trusted
//! certificates, loaded once on the first `https` source that runs, plus the
//! certificates the configuration adds (a private authority). Certificates
//! are verified, against the host the URL names, unless the source says
//! `#insecure` (go2rtc verifies nothing: a camera with a certificate of its
//! own that works there has to be able to work here).

use std::sync::{Arc, OnceLock};

use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::CryptoProvider;
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{ClientConfig, DigitallySignedStruct, RootCertStore, SignatureScheme};
use tokio_rustls::TlsConnector;

/// The trust of a factory's `https` sources.
pub(crate) struct Tls {
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
    pub(crate) fn new(extra_roots: Vec<Vec<u8>>) -> Self {
        Self {
            extra_roots: extra_roots.into_iter().map(CertificateDer::from).collect(),
            verified: OnceLock::new(),
            unverified: OnceLock::new(),
        }
    }

    /// The connector, built on first use: one that checks the certificate,
    /// or, for a source that said `#insecure`, one that takes any. `Err` says
    /// why there are no roots.
    pub(crate) fn connector(&self, verify: bool) -> Result<TlsConnector, String> {
        if verify {
            self.verified.get_or_init(|| self.build()).clone()
        } else {
            self.unverified.get_or_init(Self::build_unverified).clone()
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

/// A verifier that takes any certificate (`#insecure`). The handshake's own
/// signatures are still checked, so the peer holds the key of what it sent;
/// who it is, is not.
#[derive(Debug)]
struct AnyCertificate(Arc<CryptoProvider>);

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
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(message, cert, dss, &self.0.signature_verification_algorithms)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(message, cert, dss, &self.0.signature_verification_algorithms)
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
mod tests {
    use super::*;

    #[test]
    fn host_names_and_addresses_are_server_names() {
        for host in ["example.com", "cam.local", "192.168.1.5", "::1", "2001:db8::1"] {
            assert!(server_name(host).is_ok(), "{host}");
        }
        for host in ["", "bad host", "a..b", "exa\u{0}mple.com"] {
            assert!(server_name(host).is_err(), "{host:?}");
        }
    }
}
