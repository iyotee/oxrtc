#![allow(clippy::unwrap_used)]

use axum::http::{HeaderMap, HeaderValue, header};

use super::OriginPolicy;

fn headers(origin: Option<&str>, host: &str) -> HeaderMap {
    let mut headers = HeaderMap::new();
    headers.insert(header::HOST, HeaderValue::from_str(host).unwrap());
    if let Some(origin) = origin {
        headers.insert(header::ORIGIN, HeaderValue::from_str(origin).unwrap());
    }
    headers
}

#[test]
fn the_default_takes_a_page_of_the_same_host_on_any_port() {
    let policy = OriginPolicy::from_config(None);
    assert_eq!(policy, OriginPolicy::SameHost);
    assert_eq!(OriginPolicy::from_config(Some("")), OriginPolicy::SameHost);
    // Not a browser page: Home Assistant's library, a script.
    assert!(policy.allows(&headers(None, "192.168.1.5:1984")));
    assert!(policy.allows(&headers(Some("http://192.168.1.5:1984"), "192.168.1.5:1984")));
    // go2rtc's rule: the port is not compared (a proxy, an add-on's ingress).
    assert!(policy.allows(&headers(Some("http://192.168.1.5:8123"), "192.168.1.5:1984")));
    assert!(policy.allows(&headers(Some("https://NVR.lan"), "nvr.lan:1984")));
    assert!(policy.allows(&headers(Some("http://[::1]:8080"), "[::1]:1984")));
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
