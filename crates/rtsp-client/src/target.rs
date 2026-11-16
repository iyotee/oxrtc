//! What an `rtsp://` source points at, decided without any I/O: our own relay
//! (a loopback URL on one of our ports) or a camera.

use std::borrow::Cow;
use std::fmt;
use std::net::IpAddr;
use std::time::Duration;

use oxrtc_core::{SourceSpec, SpecError};
use percent_encoding::percent_decode_str;
use retina::client::Credentials;
use url::Url;

/// Port used when an `rtsp://` URL names none (RFC 2326).
pub const DEFAULT_RTSP_PORT: u16 = 554;

/// Port used when an `rtsps://` URL names none (IANA's `rtsps`, which is also
/// the one openIVR's TLS policy probes).
pub const DEFAULT_RTSPS_PORT: u16 = 322;

/// Where a source points.
pub(crate) enum Target {
    /// A stream of this server: read in-process.
    Loopback {
        /// The stream name (the URL path without its leading `/`).
        name: String,
        /// The go2rtc query (`video`, `audio=opus`...), possibly empty.
        query: String,
    },
    /// A camera.
    Camera(CameraTarget),
}

/// A camera to connect to.
pub(crate) struct CameraTarget {
    /// The URL handed to retina, user information removed.
    pub(crate) url: Url,
    /// The user information of the source, percent-decoded.
    pub(crate) creds: Option<Credentials>,
    /// No frame for this long ends the run with `Stalled`.
    pub(crate) stall_timeout: Duration,
    /// `rtsps://`: RTSP over TLS.
    pub(crate) tls: bool,
    /// `#verify`: an `rtsps` camera's certificate is checked (D20).
    pub(crate) verify: bool,
}

impl fmt::Debug for CameraTarget {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // The URL carries no user information any more; the credentials are
        // never printed, not even the user name.
        f.debug_struct("CameraTarget")
            .field("url", &self.url.as_str())
            .field("creds", &self.creds.as_ref().map(|_| "***"))
            .field("stall_timeout", &self.stall_timeout)
            .field("tls", &self.tls)
            .field("verify", &self.verify)
            .finish()
    }
}

fn invalid(reason: impl Into<String>) -> SpecError {
    SpecError::Invalid(reason.into())
}

/// Whether a URL host is this machine.
pub(crate) fn is_loopback_host(host: &str) -> bool {
    let bare = host.strip_prefix('[').and_then(|h| h.strip_suffix(']')).unwrap_or(host);
    host.eq_ignore_ascii_case("localhost") || bare.parse::<IpAddr>().is_ok_and(|ip| ip.is_loopback())
}

fn decode(part: &str, what: &str) -> Result<String, SpecError> {
    percent_decode_str(part)
        .decode_utf8()
        .map(Cow::into_owned)
        .map_err(|_| invalid(format!("the {what} of the source is not valid UTF-8 once decoded")))
}

/// Whether an `#<name>` flag is on: bare, or with a value other than `0` and `false`.
fn flag(spec: &SourceSpec, name: &str) -> bool {
    spec.fragment(name).is_some_and(|value| !matches!(value, "0" | "false"))
}

/// Whether the certificate of an `rtsps` camera is checked: only with
/// `#verify` (D20). `#insecure`, the flag go2rtc's and this server's `https`
/// sources use for "check nothing", is what an `rtsps` source already does, so
/// it is accepted and changes nothing; both at once say two things.
fn verification(spec: &SourceSpec, tls: bool) -> Result<bool, SpecError> {
    let (verify, insecure) = (flag(spec, "verify"), flag(spec, "insecure"));
    if verify && insecure {
        return Err(invalid("#verify and #insecure contradict each other"));
    }
    if verify && !tls {
        return Err(invalid("#verify is for rtsps:// sources"));
    }
    Ok(verify)
}

/// Classifies `spec` (scheme `rtsp` or `rtsps`).
pub(crate) fn parse(spec: &SourceSpec, loopback_ports: &[u16], default_stall: Duration) -> Result<Target, SpecError> {
    // The url crate's messages never quote the input, so they cannot leak a password.
    let mut url = Url::parse(spec.base()).map_err(|e| invalid(format!("not a URL: {e}")))?;
    let tls = match url.scheme() {
        "rtsp" => false,
        "rtsps" => true,
        other => return Err(invalid(format!("scheme {other:?} is not RTSP"))),
    };
    let host = url.host_str().filter(|h| !h.is_empty()).ok_or_else(|| invalid("the URL has no host"))?;
    let port = url.port().unwrap_or(DEFAULT_RTSP_PORT);
    if !tls && is_loopback_host(host) && loopback_ports.contains(&port) {
        let name = url.path().strip_prefix('/').unwrap_or(url.path()).to_owned();
        let query = url.query().unwrap_or_default().to_owned();
        return Ok(Target::Loopback { name, query });
    }
    let stall_timeout = spec.timeout(default_stall)?;
    let verify = verification(spec, tls)?;
    let creds = if url.username().is_empty() && url.password().is_none() {
        None
    } else {
        let username = decode(url.username(), "user name")?;
        // retina writes the user name as is into a Digest `Authorization`
        // header and unwraps the result: a control or non-ASCII character
        // (`%0D%0A`, `%C3%A9`) would panic the session, or inject a header.
        if !username.bytes().all(|b| matches!(b, 0x20..=0x7E)) || username.contains('"') {
            return Err(invalid("the user name of the source must be printable ASCII without '\"'"));
        }
        let password = decode(url.password().unwrap_or_default(), "password")?;
        url.set_username("").map_err(|()| invalid("cannot remove the user name from the URL"))?;
        url.set_password(None).map_err(|()| invalid("cannot remove the password from the URL"))?;
        Some(Credentials { username, password })
    };
    Ok(Target::Camera(CameraTarget { url, creds, stall_timeout, tls, verify }))
}

#[cfg(test)]
mod tests {
    use super::*;

    const PORTS: &[u16] = &[8554, 18_123];
    const DEFAULT: Duration = Duration::from_secs(15);

    fn target(raw: &str) -> Result<Target, SpecError> {
        parse(&SourceSpec::parse(raw).unwrap(), PORTS, DEFAULT)
    }

    fn camera(raw: &str) -> CameraTarget {
        match target(raw).unwrap() {
            Target::Camera(c) => c,
            Target::Loopback { .. } => panic!("{raw} taken for a loopback"),
        }
    }

    fn loopback(raw: &str) -> (String, String) {
        match target(raw).unwrap() {
            Target::Loopback { name, query } => (name, query),
            Target::Camera(c) => panic!("{raw} taken for a camera: {c:?}"),
        }
    }

    #[test]
    fn our_own_ports_on_loopback_hosts_are_the_relay() {
        for host in ["127.0.0.1", "localhost", "LOCALHOST", "[::1]", "127.0.0.2"] {
            let (name, query) = loopback(&format!("rtsp://{host}:8554/camera_1_src?video#timeout=20"));
            assert_eq!(name, "camera_1_src", "{host}");
            assert_eq!(query, "video", "{host}");
        }
        assert_eq!(loopback("rtsp://127.0.0.1:18123/cam?video=h264&audio=opus").1, "video=h264&audio=opus");
        assert_eq!(loopback("rtsp://127.0.0.1:8554/cam").1, "");
    }

    #[test]
    fn other_ports_hosts_and_tls_are_cameras() {
        for raw in [
            "rtsp://127.0.0.1:9554/cam",
            "rtsp://127.0.0.1/cam",
            "rtsp://192.168.1.10:8554/cam",
            "rtsp://camera.local:8554/cam",
            "rtsps://127.0.0.1:8554/cam",
        ] {
            let c = camera(raw);
            assert_eq!(c.tls, raw.starts_with("rtsps"), "{raw}");
        }
        assert_eq!(camera("rtsp://cam/live").url.port(), None);
    }

    #[test]
    fn credentials_are_decoded_and_removed_from_the_url() {
        let c = camera("rtsp://admin:p%40ss%3Aw0rd@192.168.1.100:554/live/ch00_0?x=1#timeout=15");
        let creds = c.creds.unwrap();
        assert_eq!(creds.username, "admin");
        assert_eq!(creds.password, "p@ss:w0rd");
        assert_eq!(c.url.as_str(), "rtsp://192.168.1.100:554/live/ch00_0?x=1");
        assert_eq!(c.url.username(), "");
        assert_eq!(c.url.password(), None);

        let c = camera("rtsp://admin:p@ss@cam/live");
        assert_eq!(c.creds.unwrap().password, "p@ss");
        assert_eq!(c.url.as_str(), "rtsp://cam/live");

        let c = camera("rtsp://user@cam/live");
        let creds = c.creds.unwrap();
        assert_eq!((creds.username.as_str(), creds.password.as_str()), ("user", ""));

        assert!(camera("rtsp://cam/live").creds.is_none());
    }

    #[test]
    fn debug_never_shows_credentials() {
        let c = camera("rtsp://admin:hunter2@cam/live");
        let shown = format!("{c:?}");
        assert!(!shown.contains("hunter2") && !shown.contains("admin"), "{shown}");
    }

    #[test]
    fn timeout_fragment_sets_the_stall_timeout() {
        assert_eq!(camera("rtsp://cam/live#timeout=20").stall_timeout, Duration::from_secs(20));
        assert_eq!(camera("rtsp://cam/live#raw#timeout=3").stall_timeout, Duration::from_secs(3));
        assert_eq!(camera("rtsp://cam/live").stall_timeout, DEFAULT);
        assert_eq!(camera("rtsp://cam/live#timeout=3600").stall_timeout, Duration::from_secs(3600));
        for bad in
            ["rtsp://cam/x#timeout=", "rtsp://cam/x#timeout=abc", "rtsp://cam/x#timeout=0", "rtsp://cam/x#timeout=3601"]
        {
            assert!(matches!(target(bad), Err(SpecError::Invalid(_))), "{bad}");
        }
    }

    #[test]
    fn bad_urls_are_refused() {
        for bad in ["rtsp:", "rtsp:cam", "rtsp:///live", "rtsp://", "rtsp://[zz]/x", "rtsp://cam:99999/x"] {
            assert!(matches!(target(bad), Err(SpecError::Invalid(_))), "{bad} accepted");
        }
    }

    #[test]
    fn bad_percent_encoding_in_credentials_is_refused() {
        assert!(matches!(target("rtsp://admin:%FF%FE@cam/x"), Err(SpecError::Invalid(_))));
    }

    #[test]
    fn user_names_that_cannot_be_a_header_are_refused() {
        // SourceSpec refuses raw control characters; percent-decoding must not
        // bring them back into the Digest header retina builds.
        for bad in [
            "rtsp://a%0D%0Ab:pw@cam/x",
            "rtsp://ad%00min:pw@cam/x",
            "rtsp://jos%C3%A9:pw@cam/x",
            "rtsp://a%22b:pw@cam/x",
        ] {
            assert!(matches!(target(bad), Err(SpecError::Invalid(_))), "{bad}");
        }
        // The password is never written into a header as is: anything goes.
        assert_eq!(camera("rtsp://admin:p%0A%C3%A9%22@cam/x").creds.unwrap().password, "p\n\u{e9}\"");
        assert_eq!(camera("rtsp://my%20user:pw@cam/x").creds.unwrap().username, "my user");
    }

    #[test]
    fn refusal_messages_never_quote_the_password() {
        for bad in ["rtsp://admin:hunter2@[zz]/x", "rtsp://admin:hunter2@cam:99999/x", "rtsp://a:hunter2@c/x#timeout=x"]
        {
            let Err(SpecError::Invalid(why)) = target(bad) else { panic!("{bad} accepted") };
            assert!(!why.contains("hunter2"), "{why}");
        }
    }

    #[test]
    fn only_verify_turns_the_check_of_the_certificate_on() {
        for (raw, verify) in [
            ("rtsps://cam/x", false),
            ("rtsps://cam/x#insecure", false),
            ("rtsps://cam/x#verify", true),
            ("rtsps://cam/x#verify=1#timeout=5", true),
            ("rtsps://cam/x#verify=0", false),
            ("rtsps://cam/x#verify=false", false),
        ] {
            let camera = camera(raw);
            assert_eq!((camera.tls, camera.verify), (true, verify), "{raw}");
        }
        assert!(!camera("rtsp://cam/x").verify);
        for bad in ["rtsps://cam/x#verify#insecure", "rtsp://cam/x#verify"] {
            assert!(matches!(target(bad), Err(SpecError::Invalid(_))), "{bad}");
        }
    }

    #[test]
    fn loopback_hosts() {
        assert!(is_loopback_host("127.0.0.1"));
        assert!(is_loopback_host("[::1]"));
        assert!(is_loopback_host("localhost"));
        assert!(!is_loopback_host("192.168.1.1"));
        assert!(!is_loopback_host("localhost.evil.com"));
        assert!(!is_loopback_host("[::2]"));
    }
}
