//! The TCP connection to a camera: every address its name resolves to, in
//! turn, each given its share of the time.
//!
//! `TcpStream::connect` on a name tries the addresses one after the other and
//! waits for each as long as the system does (21 s on Windows), so a name with
//! an unreachable first address (an IPv6 one on a network that does not route
//! it) used up the whole connect timeout before the address that works was
//! tried: the camera never connected. Go's dialer, which go2rtc uses, falls
//! back to the other family after a fraction of a second. Here the addresses
//! alternate between the families, and each but the last gets a slice of the
//! timeout, so the camera that is reachable is reached within it.

use std::fmt::Display;
use std::future::Future;
use std::net::SocketAddr;
use std::time::Duration;

use tokio::net::TcpStream;
use tokio::time::Instant;

/// Shortest slice an address gets before the next is tried.
const MIN_SLICE: Duration = Duration::from_secs(2);

/// The slice of `total` an address gets when there are others to try.
pub(crate) fn slice(total: Duration) -> Duration {
    (total / 3).max(MIN_SLICE).min(total)
}

/// `addrs` with the families alternating, the first address first: the other
/// family is tried second, not after every address of the first.
pub(crate) fn alternate(addrs: Vec<SocketAddr>) -> Vec<SocketAddr> {
    let Some(first) = addrs.first().map(SocketAddr::is_ipv6) else { return addrs };
    let (same, other): (Vec<_>, Vec<_>) = addrs.into_iter().partition(|a| a.is_ipv6() == first);
    let mut other = other.into_iter();
    let mut out = Vec::with_capacity(same.len() + other.len());
    for addr in same {
        out.push(addr);
        out.extend(other.next());
    }
    out.extend(other);
    out
}

/// The first of `addrs` for which `connect` succeeds within `total` overall,
/// each but the last given at most `per_attempt`. `Err` is why the last one
/// failed.
pub(crate) async fn first_that_connects<T, E, F, Fut>(
    addrs: &[SocketAddr],
    total: Duration,
    per_attempt: Duration,
    connect: F,
) -> Result<T, String>
where
    E: Display,
    F: Fn(SocketAddr) -> Fut,
    Fut: Future<Output = Result<T, E>>,
{
    let deadline = Instant::now() + total;
    let mut why = "the name has no address".to_owned();
    for (index, addr) in addrs.iter().enumerate() {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            break;
        }
        let last = index + 1 == addrs.len();
        let share = if last { left } else { left.min(per_attempt) };
        match tokio::time::timeout(share, connect(*addr)).await {
            Ok(Ok(connected)) => return Ok(connected),
            Ok(Err(error)) => why = error.to_string(),
            Err(_) => why = format!("connecting took over {}s", share.as_secs().max(1)),
        }
    }
    Err(why)
}

/// A connection to `host:port`, within `total`.
pub(crate) async fn connect(host: &str, port: u16, total: Duration) -> Result<TcpStream, String> {
    let resolved = tokio::time::timeout(total, tokio::net::lookup_host((host, port)))
        .await
        .map_err(|_| format!("resolving took over {}s", total.as_secs()))?
        .map_err(|e| e.to_string())?;
    let addrs = alternate(resolved.collect());
    first_that_connects(&addrs, total, slice(total), TcpStream::connect).await
}

#[cfg(test)]
#[path = "connect_tests.rs"]
mod tests;
