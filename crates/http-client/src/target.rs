//! What an `http://` or `https://` source points at, decided without any I/O.

use std::borrow::Cow;
use std::fmt;
use std::time::Duration;

use oxrtc_core::{SourceSpec, SpecError};
use percent_encoding::percent_decode_str;
use url::Url;

/// Pause between two requests of a camera that serves one picture per
/// request, when the source has no `#interval=`. go2rtc asks again at once,
/// which a small camera answers as fast as it can; this keeps the pace
/// bounded at ten pictures a second.
pub const DEFAULT_INTERVAL: Duration = Duration::from_millis(100);

/// Shortest and longest `#interval=` accepted, in milliseconds.
pub const INTERVAL_MS: std::ops::RangeInclusive<u64> = 10..=60_000;

/// The user information of the source, percent-decoded.
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct Credentials {
    pub(crate) username: String,
    pub(crate) password: String,
}

impl fmt::Debug for Credentials {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Credentials(***)")
    }
}

/// A webcam to read.
#[derive(Clone)]
pub(crate) struct Target {
    /// The URL, user information removed.
    pub(crate) url: Url,
    pub(crate) creds: Option<Credentials>,
    /// No picture for this long ends the run with `Stalled`; also the time
    /// one request may take.
    pub(crate) stall_timeout: Duration,
    /// Between two requests of a camera that serves one picture per request.
    pub(crate) interval: Duration,
    /// `#insecure`: an `https` camera's certificate is not checked.
    pub(crate) insecure: bool,
}

impl fmt::Debug for Target {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Target")
            .field("url", &self.url.as_str())
            .field("creds", &self.creds.as_ref().map(|_| "***"))
            .field("stall_timeout", &self.stall_timeout)
            .field("interval", &self.interval)
            .field("insecure", &self.insecure)
            .finish()
    }
}

fn invalid(reason: impl Into<String>) -> SpecError {
    SpecError::Invalid(reason.into())
}

fn decode(part: &str, what: &str) -> Result<String, SpecError> {
    percent_decode_str(part)
        .decode_utf8()
        .map(Cow::into_owned)
        .map_err(|_| invalid(format!("the {what} of the source is not valid UTF-8 once decoded")))
}

fn interval(spec: &SourceSpec) -> Result<Duration, SpecError> {
    let Some(raw) = spec.fragment("interval") else { return Ok(DEFAULT_INTERVAL) };
    raw.parse::<u64>().ok().filter(|ms| INTERVAL_MS.contains(ms)).map(Duration::from_millis).ok_or_else(|| {
        invalid(format!("#interval must be between {} and {} milliseconds", INTERVAL_MS.start(), INTERVAL_MS.end()))
    })
}

/// Reads `spec` (scheme `http` or `https`).
pub(crate) fn parse(spec: &SourceSpec, default_stall: Duration) -> Result<Target, SpecError> {
    // The url crate's messages never quote the input, so they cannot leak a password.
    let mut url = Url::parse(spec.base()).map_err(|e| invalid(format!("not a URL: {e}")))?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err(invalid(format!("scheme {:?} is not HTTP", url.scheme())));
    }
    if url.host_str().is_none_or(str::is_empty) {
        return Err(invalid("the URL has no host"));
    }
    let creds = if url.username().is_empty() && url.password().is_none() {
        None
    } else {
        let username = decode(url.username(), "user name")?;
        // The user name goes into an Authorization header as it is.
        if !username.bytes().all(|b| matches!(b, 0x20..=0x7E)) || username.contains(['"', ':']) {
            return Err(invalid("the user name of the source must be printable ASCII without '\"' or ':'"));
        }
        let password = decode(url.password().unwrap_or_default(), "password")?;
        url.set_username("").map_err(|()| invalid("cannot remove the user name from the URL"))?;
        url.set_password(None).map_err(|()| invalid("cannot remove the password from the URL"))?;
        Some(Credentials { username, password })
    };
    // Bare `#insecure`, as go2rtc's flags are written; `=0` and `=false` turn it off.
    let insecure = spec.flag("insecure");
    Ok(Target { url, creds, stall_timeout: spec.timeout(default_stall)?, interval: interval(spec)?, insecure })
}

#[cfg(test)]
mod tests {
    use super::*;

    const DEFAULT: Duration = Duration::from_secs(15);

    fn target(raw: &str) -> Result<Target, SpecError> {
        parse(&SourceSpec::parse(raw).unwrap(), DEFAULT)
    }

    #[test]
    fn credentials_are_decoded_and_removed_from_the_url() {
        let t = target("http://admin:p%40ss%3Aw0rd@192.168.1.100:8080/video?x=1#timeout=15").unwrap();
        let creds = t.creds.unwrap();
        assert_eq!((creds.username.as_str(), creds.password.as_str()), ("admin", "p@ss:w0rd"));
        assert_eq!(t.url.as_str(), "http://192.168.1.100:8080/video?x=1");
        assert!(target("https://cam/x").unwrap().creds.is_none());
        let t = target("https://user@cam/x").unwrap();
        assert_eq!(t.creds.unwrap().password, "");
    }

    #[test]
    fn debug_never_shows_credentials() {
        let shown = format!("{:?}", target("http://admin:hunter2@cam/video").unwrap());
        assert!(!shown.contains("hunter2") && !shown.contains("admin"), "{shown}");
    }

    #[test]
    fn the_options_are_read_and_bounded() {
        let t = target("http://cam/x#timeout=20#interval=250").unwrap();
        assert_eq!((t.stall_timeout, t.interval), (Duration::from_secs(20), Duration::from_millis(250)));
        let t = target("http://cam/x").unwrap();
        assert_eq!((t.stall_timeout, t.interval), (DEFAULT, DEFAULT_INTERVAL));
        for bad in
            ["timeout=0", "timeout=3601", "timeout=x", "interval=9", "interval=60001", "interval=", "interval=1.5"]
        {
            assert!(matches!(target(&format!("http://cam/x#{bad}")), Err(SpecError::Invalid(_))), "{bad}");
        }
        assert!(target("http://cam/x#interval=10").is_ok() && target("http://cam/x#interval=60000").is_ok());
        // An option go2rtc has and this does not is ignored, not refused.
        assert!(target("http://cam/x#video=h264").is_ok());
        for (raw, insecure) in [
            ("#insecure", true),
            ("#insecure=1", true),
            ("#insecure=0", false),
            ("#insecure=false", false),
            ("", false),
        ] {
            assert_eq!(target(&format!("https://cam/x{raw}")).unwrap().insecure, insecure, "{raw}");
        }
    }

    #[test]
    fn bad_urls_are_refused_without_quoting_the_password() {
        for bad in ["http:", "http://", "http://[zz]/x", "http://cam:99999/x"] {
            assert!(matches!(target(bad), Err(SpecError::Invalid(_))), "{bad} accepted");
        }
        for bad in ["http://admin:hunter2@[zz]/x", "http://admin:hunter2@cam:99999/x", "http://a:hunter2@c/x#timeout=x"]
        {
            let Err(SpecError::Invalid(why)) = target(bad) else { panic!("{bad} accepted") };
            assert!(!why.contains("hunter2"), "{why}");
        }
        assert!(matches!(target("ftp://cam/x"), Err(SpecError::Invalid(_))));
    }

    #[test]
    fn user_names_that_cannot_be_a_header_value_are_refused() {
        for bad in [
            "http://a%0D%0Ab:pw@cam/x",
            "http://ad%00min:pw@cam/x",
            "http://jos%C3%A9:pw@cam/x",
            "http://a%22b:pw@cam/x",
        ] {
            assert!(matches!(target(bad), Err(SpecError::Invalid(_))), "{bad}");
        }
        assert!(matches!(target("http://a%3Ab:pw@cam/x"), Err(SpecError::Invalid(_))), "a colon in the user name");
        assert_eq!(target("http://admin:p%0A%C3%A9%22@cam/x").unwrap().creds.unwrap().password, "p\n\u{e9}\"");
        assert_eq!(target("http://my%20user:pw@cam/x").unwrap().creds.unwrap().username, "my user");
        assert!(matches!(target("http://admin:%FF%FE@cam/x"), Err(SpecError::Invalid(_))));
    }
}
