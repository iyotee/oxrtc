//! A certificate of a camera's own, and the acceptor that serves it.
#![allow(dead_code, missing_docs, clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]

use std::sync::Arc;

use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use tokio_rustls::TlsAcceptor;

/// A self-signed certificate for `names`, and an acceptor serving it.
pub fn identity(names: &[&str]) -> (CertificateDer<'static>, TlsAcceptor) {
    let key = rcgen::generate_simple_self_signed(names.iter().map(|n| (*n).to_owned()).collect::<Vec<_>>()).unwrap();
    let cert = key.cert.der().clone();
    let private = PrivateKeyDer::try_from(key.signing_key.serialize_der()).unwrap();
    let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
    let config = rustls::ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(vec![cert.clone()], private)
        .unwrap();
    (cert, TlsAcceptor::from(Arc::new(config)))
}
