// Test code: a failed expectation is a panic by design.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]

use rustls::internal::msgs::codec::{Codec, Reader};

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

/// What the camera signs in a TLS 1.2 handshake (the transcript, here a stand-in).
const TRANSCRIPT: &[u8] = b"oxrtc handshake transcript";

/// A self-signed certificate (DER, hex) whose RSA key is 1024 bits, as the
/// cameras of a few years ago made for themselves (`openssl req -x509 -newkey
/// rsa:1024 -sha256`).
const RSA_1024_CERT: &str = "308201f830820161a00302010202142c2a6a88f65ddabeacff9e6dcfa8f80fba225266300d06092a864886f70d01010b0500300e310c300a06035504030c0363616d301e170d3236313131363132343332315a170d3236313231363132343332315a300e310c300a06035504030c0363616d30819f300d06092a864886f70d010101050003818d0030818902818100d9ed01008b823cda8ba304f6781e816c89f23ee095a735fff7d3af611727b7824effbefdf3ed706c3ace1c83dfa6eb3696c53cd1372e6e0e3cd4196f9ded2a69e27d78e96adaf3c94ea846f6ef2025e977f0873ed9c3d7aa125a2bb1bdab3e01613c89867c48b78aa9850940a8bf7211ddef7d417f0369e04627efa42870bc430203010001a3533051301d0603551d0e04160414047f8429eab57fd65a89371f90bfd04fab42420a301f0603551d23041830168014047f8429eab57fd65a89371f90bfd04fab42420a300f0603551d130101ff040530030101ff300d06092a864886f70d01010b05000381810069d929b97f8e6102c6412b7f80f64f5b64a1e2e6fd72b61fbd60615e2dd21055bef7a1c5d1f891d69ebf308c7b406e13af66774674dcc54c2ee94e9b432b6c04ca2b3f89b445057015f7e3185b2015b2487e14346a28b7e6b044dad25e3cc75153596d731624d772214863380ad62a4b65f0ae0b371d65fdde5537904cd77dd3";

/// The signature (RSASSA-PKCS1-v1_5, SHA-256, hex) of [`TRANSCRIPT`] by that
/// certificate's key: what a TLS 1.2 server sends to prove it holds it.
const RSA_1024_SIGNATURE: &str = "4ab4101085cd9f4f7dba801e3dc9bbd73d5df145648f7fcc10cfc53f2f655058a50491ab6357f2aa49254def20fa3d9924888a1f4c64573320d1b7729362bfc8f898accba4352316c61c00f43176e2b6d216804f9405839c4956247a94150c64199268b058fab19c68cc7727f610ca222326866eb20894281d75a1c104b8bb92";

fn unhex(hex: &str) -> Vec<u8> {
    (0..hex.len()).step_by(2).map(|i| u8::from_str_radix(&hex[i..i + 2], 16).unwrap()).collect()
}

fn verifier() -> AnyCertificate {
    AnyCertificate(Arc::new(rustls::crypto::aws_lc_rs::default_provider()))
}

/// The signed struct as it is on the wire: `rsa_pkcs1_sha256` (0x0401), a length, the signature.
fn signed(signature: &[u8]) -> DigitallySignedStruct {
    let mut wire = vec![0x04, 0x01];
    wire.extend_from_slice(&u16::try_from(signature.len()).unwrap().to_be_bytes());
    wire.extend_from_slice(signature);
    DigitallySignedStruct::read(&mut Reader::init(&wire)).unwrap()
}

/// go2rtc takes the self-signed 1024-bit RSA certificate of an older camera
/// (Go's `InsecureSkipVerify`, measured against 1.9.15); rustls will not verify
/// a signature of such a key, so the first version of this verifier, which
/// did, turned the camera away with `invalid peer certificate: BadSignature`
/// and an unverified `rtsps` source never played.
#[test]
fn a_certificate_with_a_1024_bit_rsa_key_is_taken_like_any_other() {
    let cert = CertificateDer::from(unhex(RSA_1024_CERT));
    let dss = signed(&unhex(RSA_1024_SIGNATURE));
    // The premise: the checking verifier refuses this very signature.
    let provider = rustls::crypto::aws_lc_rs::default_provider();
    assert!(
        rustls::crypto::verify_tls12_signature(TRANSCRIPT, &cert, &dss, &provider.signature_verification_algorithms)
            .is_err(),
        "rustls verifies a 1024-bit RSA signature: the premise of this test is gone"
    );
    let any = verifier();
    assert!(any.verify_tls12_signature(TRANSCRIPT, &cert, &dss).is_ok());
    assert!(any.verify_tls13_signature(TRANSCRIPT, &cert, &dss).is_ok());
}

/// A certificate no parser takes (not even DER) is as good as any.
#[test]
fn a_certificate_that_is_not_even_der_is_taken() {
    let cert = CertificateDer::from(vec![0x30, 0x03, 0x01]);
    let dss = signed(&[0; 16]);
    let any = verifier();
    let name = server_name("192.168.1.5").unwrap();
    assert!(any.verify_server_cert(&cert, &[], &name, &[], UnixTime::now()).is_ok());
    assert!(any.verify_tls12_signature(TRANSCRIPT, &cert, &dss).is_ok());
}
