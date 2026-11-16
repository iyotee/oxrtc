//! Which input URLs of an `ffmpeg:` source connect to this server's own RTSP relay.

/// Whether `url` is an input that connects to the RTSP server at `relay`
/// (`host:port`, as the plan was given it): its own spelling of that address,
/// or `localhost` on its port when it is on loopback, which is what the RTSP
/// client takes for the relay too. A URL naming a user has `@` before its first
/// `/`, so its authority never starts with the relay's.
pub(crate) fn reads_relay(url: &str, relay: &str) -> bool {
    if url.starts_with(&format!("rtsp://{relay}/")) {
        return true;
    }
    let Some((relay_host, port)) = relay.rsplit_once(':') else { return false };
    let on_loopback = relay_host
        .strip_prefix('[')
        .and_then(|h| h.strip_suffix(']'))
        .unwrap_or(relay_host)
        .parse::<std::net::IpAddr>()
        .is_ok_and(|ip| ip.is_loopback());
    let authority = url.strip_prefix("rtsp://").and_then(|rest| rest.split_once('/')).map(|(authority, _)| authority);
    on_loopback
        && authority
            .and_then(|a| a.rsplit_once(':'))
            .is_some_and(|(host, p)| host.eq_ignore_ascii_case("localhost") && p == port)
}
