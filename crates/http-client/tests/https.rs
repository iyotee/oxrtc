//! `https://` webcams: the certificate is verified against the roots the
//! factory trusts and the host the URL names, and nothing else.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]

mod common;

use std::sync::Arc;
use std::time::Duration;

use common::{Reply, Server, failure_of, hub_with, picture, stream_of, watch};
use oxrtc_core::Codec;
use oxrtc_http_client::HttpClientConfig;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use tokio_rustls::TlsAcceptor;

/// A self-signed certificate for `names`, and an acceptor serving it.
fn identity(names: &[&str]) -> (CertificateDer<'static>, TlsAcceptor) {
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

fn camera() -> impl Fn(&common::Req) -> Reply + Send + Sync + 'static {
    |_| stream_of("frame", "--frame", vec![picture(1, 500), picture(2, 800)], Duration::from_millis(20))
}

fn config(roots: Vec<Vec<u8>>) -> HttpClientConfig {
    HttpClientConfig { extra_roots: roots, ..HttpClientConfig::default() }
}

#[tokio::test]
async fn a_certificate_the_factory_trusts_for_the_host_gives_pictures() {
    let (cert, acceptor) = identity(&["127.0.0.1", "localhost"]);
    let server = Server::start_tls(acceptor, camera()).await;
    let hub = hub_with(config(vec![cert.to_vec()]));
    // By address and by name: both are in the certificate.
    for (name, host) in [("by-ip", "127.0.0.1"), ("by-name", "localhost")] {
        let stream = hub.upsert(name, &[format!("https://{host}:{}/video", server.port)]).unwrap();
        let (tracks, got) = watch(&stream, 3).await;
        assert_eq!(tracks[0].codec, Codec::Jpeg);
        assert_eq!(got.len(), 3);
    }
}

#[tokio::test]
async fn a_certificate_nobody_trusts_is_refused() {
    let (_, acceptor) = identity(&["127.0.0.1"]);
    let server = Server::start_tls(acceptor, camera()).await;
    let hub = hub_with(config(Vec::new()));
    let stream = hub.upsert("cam", &[format!("https://user:hunter2@127.0.0.1:{}/video", server.port)]).unwrap();
    let error = failure_of(&stream, Duration::from_secs(10)).await;
    assert!(error.contains("TLS") && error.contains("UnknownIssuer"), "{error}");
    assert!(!error.contains("hunter2"));
    assert!(server.requests().is_empty(), "a request went out over an unverified connection");
}

/// go2rtc 1.9.15 checks nothing (measured against a self-signed server of
/// its own): a camera that works there works here with `#insecure`, and
/// only with it.
#[tokio::test]
async fn a_source_that_says_insecure_takes_any_certificate() {
    let (_, acceptor) = identity(&["camera.example"]);
    let server = Server::start_tls(acceptor, camera()).await;
    let hub = hub_with(config(Vec::new()));
    let url = format!("https://127.0.0.1:{}/video", server.port);
    let stream = hub.upsert("insecure", &[format!("{url}#insecure")]).unwrap();
    let (tracks, got) = watch(&stream, 3).await;
    assert_eq!((tracks[0].codec, got.len()), (Codec::Jpeg, 3));
    // The same camera without the flag: refused, nothing requested.
    let stream = hub.upsert("checked", std::slice::from_ref(&url)).unwrap();
    let error = failure_of(&stream, Duration::from_secs(10)).await;
    assert!(error.contains("TLS"), "{error}");
    let asked = server.requests().len();
    let off = hub.upsert("off", &[format!("{url}#insecure=0")]).unwrap();
    assert!(failure_of(&off, Duration::from_secs(10)).await.contains("TLS"));
    assert_eq!(server.requests().len(), asked, "no request went out over an unverified connection");
}

#[tokio::test]
async fn a_trusted_certificate_for_another_name_is_refused() {
    let (cert, acceptor) = identity(&["camera.example"]);
    let server = Server::start_tls(acceptor, camera()).await;
    let hub = hub_with(config(vec![cert.to_vec()]));
    let stream = hub.upsert("cam", &[format!("https://127.0.0.1:{}/video", server.port)]).unwrap();
    let error = failure_of(&stream, Duration::from_secs(10)).await;
    assert!(error.contains("TLS") && error.contains("not valid for name"), "{error}");
    assert!(server.requests().is_empty());
}

#[tokio::test]
async fn plain_http_to_a_tls_port_and_the_reverse_fail_without_hanging() {
    let (cert, acceptor) = identity(&["127.0.0.1"]);
    let tls = Server::start_tls(acceptor, camera()).await;
    let hub = hub_with(config(vec![cert.to_vec()]));
    let stream = hub.upsert("plain", &[format!("http://127.0.0.1:{}/video#timeout=2", tls.port)]).unwrap();
    let error = failure_of(&stream, Duration::from_secs(10)).await;
    assert!(!error.is_empty());
    let plain = Server::start(camera()).await;
    let stream = hub.upsert("secured", &[format!("https://127.0.0.1:{}/video#timeout=2", plain.port)]).unwrap();
    let error = failure_of(&stream, Duration::from_secs(10)).await;
    assert!(error.contains("TLS"), "{error}");
}

/// The owner's `https` camera answers `HTTP/1.0` and ends what it sends by
/// closing its socket, as small cameras do: over TLS that is a TCP close with
/// no `close_notify`. Go (go2rtc) takes that at a record boundary for the
/// end of the body; rustls calls it an error, and a picture with no length
/// that ends this way must still be a picture.
#[tokio::test]
async fn a_picture_with_no_length_that_ends_with_the_socket_closing_is_still_a_picture() {
    let (cert, acceptor) = identity(&["127.0.0.1"]);
    let still = picture(7, 3000);
    let served = still.clone();
    let server = Server::start_tls(acceptor, move |_| {
        let mut out = b"HTTP/1.0 200 OK\r\nContent-Type: image/jpeg\r\nConnection: close\r\n\r\n".to_vec();
        out.extend_from_slice(&served);
        Reply::Abrupt(out)
    })
    .await;
    let hub = hub_with(config(vec![cert.to_vec()]));
    let stream = hub.upsert("cam", &[format!("https://127.0.0.1:{}/snapshot.jpg#interval=20", server.port)]).unwrap();
    let (_, got) = watch(&stream, 3).await;
    assert!(got.iter().all(|p| *p == still));
    assert!(server.connections.load(std::sync::atomic::Ordering::SeqCst) >= 3, "one connection per picture");
}

/// The same camera's stream: its end comes back as the camera, with the
/// pictures it gave before.
#[tokio::test]
async fn a_stream_over_tls_that_ends_with_the_socket_closing_comes_back() {
    let (cert, acceptor) = identity(&["127.0.0.1"]);
    let pictures = [picture(1, 900), picture(2, 900)];
    let served = pictures.clone();
    let server = Server::start_tls(acceptor, move |_| {
        let mut out = b"HTTP/1.0 200 OK\r\nContent-Type: multipart/x-mixed-replace; boundary=myboundary\r\n\
                        Connection: close\r\n\r\n"
            .to_vec();
        for p in &served {
            out.extend_from_slice(&common::part("--myboundary", p));
        }
        Reply::Abrupt(out)
    })
    .await;
    let hub = hub_with(config(vec![cert.to_vec()]));
    let stream = hub.upsert("cam", &[format!("https://127.0.0.1:{}/view/", server.port)]).unwrap();
    let (_, got) = watch(&stream, 6).await;
    assert_eq!(got.len(), 6);
    assert!(got.iter().all(|p| pictures.contains(p)));
    assert!(server.connections.load(std::sync::atomic::Ordering::SeqCst) >= 2, "it came back");
}

/// Taking a close without `close_notify` for an end does not make a short
/// body a picture: a response with a length still has to give all of it.
#[tokio::test]
async fn a_picture_cut_short_of_its_length_over_tls_is_still_an_error() {
    let (cert, acceptor) = identity(&["127.0.0.1"]);
    let whole = picture(7, 3000);
    let server = Server::start_tls(acceptor, move |_| {
        let mut out = format!("HTTP/1.1 200 OK\r\nContent-Type: image/jpeg\r\nContent-Length: {}\r\n\r\n", whole.len())
            .into_bytes();
        out.extend_from_slice(&whole[..1000]);
        Reply::Abrupt(out)
    })
    .await;
    let hub = hub_with(config(vec![cert.to_vec()]));
    let stream = hub.upsert("cam", &[format!("https://127.0.0.1:{}/snapshot.jpg", server.port)]).unwrap();
    let error = failure_of(&stream, Duration::from_secs(10)).await;
    assert!(error.contains("unreachable"), "{error}");
}
