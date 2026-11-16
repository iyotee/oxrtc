use super::*;

fn pairs(header: &str) -> Option<Vec<(String, String)>> {
    parse_params(header)
}

#[test]
fn md5_matches_the_known_vector() {
    assert_eq!(hex_md5(""), "d41d8cd98f00b204e9800998ecf8427e");
    assert_eq!(hex_md5("abc"), "900150983cd24fb0d6963f7d28e17f72");
}

#[test]
fn the_rfc_2617_example_response_is_what_this_formula_gives() {
    // RFC 2617 section 3.5, with its own realm: the formula of `Gate::digest`.
    let a1 = hex_md5("Mufasa:testrealm@host.com:Circle Of Life");
    let a2 = hex_md5("GET:/dir/index.html");
    let response = hex_md5(&format!("{a1}:dcd98b7102dd2f0e8b11d0f600bfb0c093:00000001:0a4f113b:auth:{a2}"));
    assert_eq!(response, "6629fae49393a05397450978507c4ef1");
}

#[test]
fn digest_parameters_read_quoted_and_bare_values() {
    let got = pairs(
        r#"username="op", realm="oxrtc", nonce="ab12", uri="rtsp://h/x?a=b,c", qop=auth, nc=00000001, response="ff""#,
    )
    .unwrap();
    let get = |k: &str| got.iter().find(|(key, _)| key == k).map(|(_, v)| v.as_str());
    assert_eq!(get("username"), Some("op"));
    assert_eq!(get("uri"), Some("rtsp://h/x?a=b,c"), "a comma inside quotes is part of the value");
    assert_eq!(get("qop"), Some("auth"));
    assert_eq!(get("nc"), Some("00000001"));
    assert_eq!(get("response"), Some("ff"));
}

#[test]
fn an_escaped_quote_is_part_of_the_value() {
    let got = pairs(r#"username="a\"b", nonce="n""#).unwrap();
    assert_eq!(got.first().map(|(_, v)| v.as_str()), Some("a\"b"));
}

#[test]
fn headers_that_are_not_pairs_are_refused() {
    for bad in ["username", "=x", "a b=c", r#"username="unterminated"#, "x=\"a\\"] {
        assert!(pairs(bad).is_none(), "{bad:?}");
    }
}

#[test]
fn too_many_pairs_are_refused() {
    let many: Vec<String> = (0..=MAX_DIGEST_PARAMS).map(|i| format!("k{i}=v")).collect();
    assert!(pairs(&many.join(", ")).is_none());
    let ok: Vec<String> = (0..MAX_DIGEST_PARAMS).map(|i| format!("k{i}=v")).collect();
    assert_eq!(pairs(&ok.join(", ")).map(|p| p.len()), Some(MAX_DIGEST_PARAMS));
}

#[test]
fn nonces_are_128_bit_hex_and_distinct() {
    let (a, b) = (new_nonce(), new_nonce());
    assert_eq!(a.len(), 32);
    assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
    assert_ne!(a, b);
}

#[test]
fn loopback_is_seen_through_an_ipv4_mapped_address() {
    assert!(is_loopback("127.0.0.1".parse().unwrap()));
    assert!(is_loopback("::1".parse().unwrap()));
    assert!(is_loopback("::ffff:127.0.0.1".parse().unwrap()));
    assert!(!is_loopback("192.168.1.2".parse().unwrap()));
}

#[test]
fn the_debug_form_hides_the_password() {
    let text = format!("{:?}", ReadAuth::new("operator", "hunter2"));
    assert!(text.contains("operator"));
    assert!(!text.contains("hunter2"));
}

mod gate {
    use std::net::SocketAddr;

    use oxrtc_core::{Drivers, Hub, Limits, Refusals};
    use rtsp_types::headers::CSEQ;
    use rtsp_types::{Method, Request, Version};

    use super::super::{Gate, ReadAuth, Verdict};
    use crate::publish::Publishers;
    use crate::{RtspServerConfig, Shared};

    fn shared(auth: Option<ReadAuth>) -> Shared {
        let publishers = Publishers::new(SocketAddr::from(([127, 0, 0, 1], 8554)), auth.is_some());
        Shared {
            hub: Hub::new(Drivers::new(), Limits::default()),
            publishers,
            config: RtspServerConfig::default(),
            auth,
            bad_credentials: Refusals::new(),
        }
    }

    fn describe(path: &str) -> Request<Vec<u8>> {
        Request::builder(Method::Describe, Version::V1_0)
            .request_uri(rtsp_types::Url::parse(&format!("rtsp://127.0.0.1:8554/{path}")).unwrap())
            .header(CSEQ, "1")
            .build(Vec::new())
    }

    fn verdict(shared: &Shared, peer: &str, path: &str) -> &'static str {
        match Gate::default().check(shared, peer.parse::<SocketAddr>().unwrap(), &describe(path)) {
            Verdict::Allowed => "allowed",
            Verdict::Challenge(_) => "challenge",
            Verdict::Refused(_) => "refused",
        }
    }

    #[tokio::test]
    async fn nothing_is_asked_when_no_credential_is_configured() {
        let shared = shared(None);
        assert_eq!(verdict(&shared, "192.168.1.9:50000", "cam"), "allowed");
        assert_eq!(verdict(&shared, "127.0.0.1:50000", "cam"), "allowed");
    }

    #[tokio::test]
    async fn loopback_authenticates_unless_local_auth_is_off_and_others_always_do() {
        let on = shared(Some(ReadAuth::new("u", "p")));
        for peer in ["127.0.0.1:1", "[::1]:1", "192.168.1.9:1", "[2001:db8::1]:1"] {
            assert_eq!(verdict(&on, peer, "cam"), "challenge", "{peer}");
        }
        let off = shared(Some(ReadAuth::new("u", "p").local(false)));
        for peer in ["127.0.0.1:1", "[::1]:1", "[::ffff:127.0.0.1]:1"] {
            assert_eq!(verdict(&off, peer, "cam"), "allowed", "{peer}");
        }
        for peer in ["192.168.1.9:1", "[2001:db8::1]:1", "10.0.0.1:1", "[::ffff:192.168.1.9]:1"] {
            assert_eq!(verdict(&off, peer, "cam"), "challenge", "{peer}");
        }
    }

    #[tokio::test]
    async fn a_publication_path_is_the_tokens_to_check_not_the_gates() {
        let on = shared(Some(ReadAuth::new("u", "p")));
        assert_eq!(verdict(&on, "127.0.0.1:1", "_publish/0123"), "allowed");
        // Only the path's first segment is the prefix: a stream that merely contains it is a stream.
        assert_eq!(verdict(&on, "127.0.0.1:1", "cam/_publish/x"), "challenge");
        assert_eq!(verdict(&on, "127.0.0.1:1", "x_publish/x"), "challenge");
    }
}
