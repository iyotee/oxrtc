use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::Mutex;
use std::time::Instant;

use super::*;

fn v4(n: u8) -> SocketAddr {
    SocketAddr::new(IpAddr::V4(Ipv4Addr::new(10, 0, 0, n)), 80)
}

fn v6(n: u16) -> SocketAddr {
    SocketAddr::new(IpAddr::V6(Ipv6Addr::new(0x2001, 0xdb8, 0, 0, 0, 0, 0, n)), 80)
}

#[test]
fn the_families_alternate_starting_with_the_first_address() {
    assert_eq!(alternate(vec![v6(1), v6(2), v4(1), v4(2)]), [v6(1), v4(1), v6(2), v4(2)]);
    assert_eq!(alternate(vec![v4(1), v4(2), v4(3), v6(1)]), [v4(1), v6(1), v4(2), v4(3)]);
    assert_eq!(alternate(vec![v4(1), v4(2)]), [v4(1), v4(2)]);
    assert_eq!(alternate(vec![]), []);
}

#[test]
fn an_address_gets_a_third_of_the_time_and_at_least_two_seconds() {
    assert_eq!(slice(Duration::from_secs(30)), Duration::from_secs(10));
    assert_eq!(slice(Duration::from_secs(15)), Duration::from_secs(5));
    assert_eq!(slice(Duration::from_secs(3)), Duration::from_secs(2));
    assert_eq!(slice(Duration::from_secs(1)), Duration::from_secs(1), "never more than the whole");
}

#[tokio::test]
async fn an_address_that_never_answers_does_not_use_up_the_time_of_the_one_that_does() {
    let tried = Mutex::new(Vec::new());
    let started = Instant::now();
    let connected =
        first_that_connects(&[v6(1), v4(1)], Duration::from_secs(10), Duration::from_millis(100), |addr| {
            tried.lock().unwrap().push(addr);
            async move {
                if addr.is_ipv6() { std::future::pending::<Result<u8, String>>().await } else { Ok::<u8, String>(4) }
            }
        })
        .await;
    assert_eq!(connected, Ok(4));
    assert_eq!(*tried.lock().unwrap(), [v6(1), v4(1)]);
    assert!(started.elapsed() < Duration::from_secs(2), "{:?}", started.elapsed());
}

#[tokio::test]
async fn the_last_address_gets_what_is_left_and_the_whole_is_bounded() {
    let started = Instant::now();
    // Two that never answer: the first gets 50 ms, the last the rest of 300.
    let result = first_that_connects(&[v6(1), v4(1)], Duration::from_millis(300), Duration::from_millis(50), |_| {
        std::future::pending::<Result<(), String>>()
    })
    .await;
    assert!(result.unwrap_err().contains("took over"), "says why");
    let elapsed = started.elapsed();
    assert!(elapsed >= Duration::from_millis(280) && elapsed < Duration::from_secs(2), "{elapsed:?}");
}

#[tokio::test]
async fn a_refusal_goes_on_to_the_next_address_and_the_last_one_is_the_reason() {
    let result =
        first_that_connects(&[v6(1), v4(1)], Duration::from_secs(5), Duration::from_secs(1), |addr| async move {
            Err::<(), _>(if addr.is_ipv6() { "first refused" } else { "last refused" })
        })
        .await;
    assert_eq!(result, Err("last refused".to_owned()));
    let none =
        first_that_connects(&[], Duration::from_secs(1), Duration::from_secs(1), |_| async { Ok::<(), String>(()) })
            .await;
    assert_eq!(none, Err("the name has no address".to_owned()));
}

#[tokio::test]
async fn a_name_is_resolved_and_reached() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let accepted = tokio::spawn(async move { listener.accept().await.is_ok() });
    // `localhost` is ::1 and 127.0.0.1 on most systems; the listener is on the second.
    let stream = connect("localhost", port, Duration::from_secs(10)).await;
    assert!(stream.is_ok(), "{:?}", stream.err());
    assert!(accepted.await.unwrap());
    let refused = connect("127.0.0.1", 1, Duration::from_secs(5)).await;
    assert!(refused.is_err());
}
