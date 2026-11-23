#![allow(clippy::unwrap_used)]

use axum::http::{HeaderMap, HeaderValue, header};

use super::{OriginPolicy, origin_refusal};

fn headers(origin: Option<&str>, host: &str) -> HeaderMap {
    let mut headers = HeaderMap::new();
    headers.insert(header::HOST, HeaderValue::from_str(host).unwrap());
    if let Some(origin) = origin {
        headers.insert(header::ORIGIN, HeaderValue::from_str(origin).unwrap());
    }
    headers
}

#[test]
fn the_default_takes_a_page_of_the_request_s_own_host() {
    let policy = OriginPolicy::from_config(None);
    assert_eq!(policy, OriginPolicy::SameHost);
    assert_eq!(OriginPolicy::from_config(Some("")), OriginPolicy::SameHost);
    // Not a browser page: Home Assistant's library, a script.
    assert!(policy.allows(&headers(None, "192.168.1.5:1984")));
    assert!(policy.allows(&headers(Some("http://192.168.1.5:1984"), "192.168.1.5:1984")));
    assert!(policy.allows(&headers(Some("https://NVR.lan:1984"), "nvr.lan:1984")));
    // go2rtc's rule (measured on 1.9.15 with a handshake's Host and Origin):
    // the page's port is left out only when the request's `Host` names none,
    // a proxy passing its own host name on (`Host: 127.0.0.1`, `Origin:
    // http://127.0.0.1:9999`: 101).
    assert!(policy.allows(&headers(Some("http://192.168.1.5:8123"), "192.168.1.5")));
    assert!(policy.allows(&headers(Some("https://nvr.lan"), "nvr.lan")));
    assert!(policy.allows(&headers(Some("http://[::1]:8080"), "[::1]")));
    // A page of another port where the `Host` names its own is another
    // program's page (`Host: 127.0.0.1:43384`, `Origin: http://127.0.0.1:9999`:
    // go2rtc 403), and so is a page with no port against a `Host` with one.
    assert!(!policy.allows(&headers(Some("http://192.168.1.5:8123"), "192.168.1.5:1984")));
    assert!(!policy.allows(&headers(Some("https://NVR.lan"), "nvr.lan:1984")));
    assert!(!policy.allows(&headers(Some("http://[::1]:8080"), "[::1]:1984")));
    assert!(!policy.allows(&headers(Some("http://192.168.1.5"), "192.168.1.5:1984")));
    assert!(!policy.allows(&headers(Some("http://192.168.1.5:8123"), "192.168.1.6")));
    assert!(!policy.allows(&headers(Some("http://evil.example"), "192.168.1.5:1984")));
    assert!(!policy.allows(&headers(Some("http://192.168.1.5.evil.example"), "192.168.1.5:1984")));
    assert!(!policy.allows(&headers(Some("null"), "192.168.1.5:1984")));
    assert!(!policy.allows(&headers(Some("http://192.168.1.5:1984/page"), "192.168.1.5:1984")));
    assert!(!policy.allows(&headers(Some("http://u@192.168.1.5"), "192.168.1.5:1984")));
}

#[test]
fn any_takes_every_page_and_another_value_the_exact_host() {
    let any = OriginPolicy::from_config(Some("*"));
    assert_eq!(any, OriginPolicy::Any);
    assert!(any.allows(&headers(Some("http://evil.example"), "192.168.1.5:1984")));
    let exact = OriginPolicy::from_config(Some("http://nvr.lan"));
    assert_eq!(exact, OriginPolicy::SameHostAndPort);
    assert!(exact.allows(&headers(Some("http://192.168.1.5:1984"), "192.168.1.5:1984")));
    assert!(!exact.allows(&headers(Some("http://192.168.1.5:8123"), "192.168.1.5:1984")));
}

#[test]
fn a_page_without_a_host_to_compare_is_refused() {
    let mut no_host = HeaderMap::new();
    no_host.insert(header::ORIGIN, HeaderValue::from_static("http://192.168.1.5"));
    assert!(!OriginPolicy::SameHost.allows(&no_host));
    assert!(OriginPolicy::Any.allows(&no_host));
}

#[test]
fn a_refusal_quotes_the_page_and_the_host_cleaned_and_cut() {
    let said = origin_refusal(&headers(Some("http://nvr.lan:8000"), "127.0.0.1:1984"));
    assert!(said.starts_with("origin not allowed: a page of http://nvr.lan:8000 may not open a websocket of host 127.0.0.1:1984 (api.origin);"), "{said}");
    assert!(said.contains("proxy_set_header Host $http_host"), "{said}");
    // What is not printable ASCII is left out, a long value cut.
    let long = format!("http://{}.example", "a".repeat(400));
    let said = origin_refusal(&headers(Some(&long), "h\tost:1"));
    assert!(!said.contains('\t') && said.contains("host host:1"), "{said}");
    assert!(said.len() < 600, "{}", said.len());
    let mut no_host = HeaderMap::new();
    no_host.insert(header::ORIGIN, HeaderValue::from_static("http://a"));
    assert!(origin_refusal(&no_host).contains("of host (none)"));
}
