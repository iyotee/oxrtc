//! Source descriptions, in go2rtc's notation.
//!
//! `scheme:rest#key=value#flag` - the scheme picks the driver, the fragments
//! after `#` are its options. A spec usually holds credentials
//! (`rtsp://user:pass@camera/...`), so its [`Display`](std::fmt::Display) and
//! [`Debug`] forms are redacted, and the raw text is only reachable through
//! [`SourceSpec::expose`], which a reviewer can grep for.

use std::fmt;

use crate::error::SpecError;

/// Longest accepted source string.
pub const MAX_SPEC_LEN: usize = 4096;

/// Query keys whose values are hidden by [`SourceSpec::redacted`].
const SECRET_KEYS: [&str; 9] = ["password", "pass", "pwd", "passwd", "token", "secret", "key", "auth", "credential"];

/// A parsed source description.
#[derive(Clone, PartialEq, Eq)]
pub struct SourceSpec {
    raw: String,
    scheme: String,
    base_end: usize,
}

impl SourceSpec {
    /// Parses a source. Leading and trailing whitespace is ignored; control
    /// characters anywhere are refused (they are how a header gets injected).
    pub fn parse(raw: &str) -> Result<Self, SpecError> {
        let raw = raw.trim();
        if raw.is_empty() {
            return Err(SpecError::Empty);
        }
        if raw.len() > MAX_SPEC_LEN {
            return Err(SpecError::TooLong(MAX_SPEC_LEN));
        }
        if raw.chars().any(char::is_control) {
            return Err(SpecError::ControlCharacter);
        }
        let (scheme, _) = raw.split_once(':').ok_or(SpecError::BadScheme)?;
        let mut chars = scheme.chars();
        let valid = chars.next().is_some_and(|c| c.is_ascii_alphabetic())
            && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'));
        if !valid {
            return Err(SpecError::BadScheme);
        }
        let base_end = raw.find('#').unwrap_or(raw.len());
        Ok(Self { scheme: scheme.to_ascii_lowercase(), raw: raw.to_owned(), base_end })
    }

    /// The scheme, lower-cased (`rtsp`, `ffmpeg`, `stream`...).
    pub fn scheme(&self) -> &str {
        &self.scheme
    }

    /// Everything after `scheme:` and before the first `#`. For
    /// `ffmpeg:rtsp://cam/x#video=h264` that is `rtsp://cam/x`. May hold
    /// credentials: never log it.
    pub fn target(&self) -> &str {
        let base = self.raw.get(..self.base_end).unwrap_or_default();
        base.split_once(':').map_or("", |(_, rest)| rest)
    }

    /// The source up to the first `#`, scheme included. May hold credentials.
    pub fn base(&self) -> &str {
        self.raw.get(..self.base_end).unwrap_or_default()
    }

    /// The `#` fragments in order, as `(key, value)`; a bare `#flag` has an
    /// empty value.
    pub fn fragments(&self) -> impl Iterator<Item = (&str, &str)> {
        self.raw
            .get(self.base_end..)
            .unwrap_or_default()
            .split('#')
            .filter(|f| !f.is_empty())
            .map(|f| f.split_once('=').unwrap_or((f, "")))
    }

    /// The value of the first fragment named `key`.
    pub fn fragment(&self, key: &str) -> Option<&str> {
        self.fragments().find(|(k, _)| *k == key).map(|(_, v)| v)
    }

    /// The values of every fragment named `key`.
    pub fn fragment_all<'a>(&'a self, key: &'a str) -> impl Iterator<Item = &'a str> + 'a {
        self.fragments().filter(move |(k, _)| *k == key).map(|(_, v)| v)
    }

    /// The raw text, credentials included. Only for handing to a driver.
    pub fn expose(&self) -> &str {
        &self.raw
    }

    /// The text with user information and secret query values replaced by `***`.
    pub fn redacted(&self) -> String {
        redact(&self.raw)
    }
}

impl fmt::Display for SourceSpec {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.redacted())
    }
}

impl fmt::Debug for SourceSpec {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("SourceSpec").field(&self.redacted()).finish()
    }
}

/// Redacts every `scheme://userinfo@` and every secret query value in `raw`.
///
/// Over-redaction is accepted: the user information is everything between
/// `://` and the last `@` before the next `://`, so a password holding a `/`,
/// a `?` or a raw `#` is still hidden (a `@` in a fragment hides a bit more).
/// A `://` met before any `/` or `@` is still inside the user information
/// (`rtsp://admin:ab://cd@cam`), so the segment runs on to the next one.
pub fn redact(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut rest = raw;
    while let Some(at) = rest.find("://") {
        let (head, tail) = rest.split_at(at + 3);
        out.push_str(head);
        let segment_end = userinfo_segment_end(tail);
        let segment = tail.get(..segment_end).unwrap_or_default();
        match segment.rfind('@') {
            Some(user_end) => {
                out.push_str("***");
                rest = tail.get(user_end..).unwrap_or_default();
            }
            None => rest = tail,
        }
    }
    out.push_str(rest);
    redact_query_values(&out)
}

/// Where the user-information search of one URL stops: the next `://`, unless
/// no `/` and no `@` came before it, in which case that `://` belongs to the
/// password and the search runs on, as long as an `@` is still ahead.
fn userinfo_segment_end(tail: &str) -> usize {
    let mut end = 0;
    loop {
        let from = tail.get(end..).unwrap_or_default();
        let Some(next) = from.find("://") else { return tail.len() };
        let candidate = end + next;
        let before = tail.get(..candidate).unwrap_or_default();
        let after = tail.get(candidate + 3..).unwrap_or_default();
        if before.contains(['/', '@']) || !after.contains('@') {
            return candidate;
        }
        end = candidate + 3;
    }
}

fn redact_query_values(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(sep) = rest.find(['?', '&']) {
        let (head, tail) = rest.split_at(sep + 1);
        out.push_str(head);
        let key_end = tail.find(['=', '&', '#', '?']).unwrap_or(tail.len());
        let key = tail.get(..key_end).unwrap_or_default();
        let after_key = tail.get(key_end..).unwrap_or_default();
        out.push_str(key);
        let secret = SECRET_KEYS.iter().any(|s| key.eq_ignore_ascii_case(s));
        match after_key.strip_prefix('=') {
            Some(value_and_more) if secret => {
                let value_end = value_and_more.find(['&', '#']).unwrap_or(value_and_more.len());
                out.push_str("=***");
                rest = value_and_more.get(value_end..).unwrap_or_default();
            }
            _ => rest = after_key,
        }
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_scheme_target_and_fragments() {
        let s = SourceSpec::parse("ffmpeg:camera_1_src#video=h264#width=1920#raw").unwrap();
        assert_eq!(s.scheme(), "ffmpeg");
        assert_eq!(s.target(), "camera_1_src");
        assert_eq!(s.fragment("video"), Some("h264"));
        assert_eq!(s.fragment("width"), Some("1920"));
        assert_eq!(s.fragment("raw"), Some(""));
        assert_eq!(s.fragment("height"), None);
    }

    #[test]
    fn keeps_query_in_target() {
        let s = SourceSpec::parse("rtsp://127.0.0.1:8554/camera_1_src?video#timeout=20").unwrap();
        assert_eq!(s.scheme(), "rtsp");
        assert_eq!(s.target(), "//127.0.0.1:8554/camera_1_src?video");
        assert_eq!(s.base(), "rtsp://127.0.0.1:8554/camera_1_src?video");
        assert_eq!(s.fragment("timeout"), Some("20"));
    }

    #[test]
    fn refuses_bad_input() {
        assert_eq!(SourceSpec::parse("  "), Err(SpecError::Empty));
        assert_eq!(SourceSpec::parse("no scheme"), Err(SpecError::BadScheme));
        assert_eq!(SourceSpec::parse("1rtsp://x"), Err(SpecError::BadScheme));
        assert_eq!(SourceSpec::parse("rtsp://x\r\nHost: evil"), Err(SpecError::ControlCharacter));
        assert_eq!(SourceSpec::parse(&format!("rtsp://{}", "a".repeat(5000))), Err(SpecError::TooLong(MAX_SPEC_LEN)));
    }

    #[test]
    fn redacts_userinfo_even_with_awkward_passwords() {
        for (raw, hidden) in [
            ("rtsp://admin:hunter2@192.168.1.10:554/live", "hunter2"),
            ("rtsp://admin:pa/ss?w@cam/live", "pa/ss?w"),
            ("ffmpeg:rtsp://u:s3cr3t@cam/x#video=h264", "s3cr3t"),
            ("http://cam/snap.jpg?user=admin&password=topsecret&w=1", "topsecret"),
            ("http://cam/x?token=abc123#raw", "abc123"),
            ("rtsp://admin:#abc@cam/x", "abc"),
            ("rtsp://admin:ab://cd@cam/x", "ab"),
            ("rtsp://admin:ab://cd@cam/x", "cd"),
            ("ffmpeg:rtsp://admin://pw@cam/x#video=h264", "pw"),
        ] {
            let s = SourceSpec::parse(raw).unwrap();
            let shown = format!("{s} {s:?}");
            assert!(!shown.contains(hidden), "{shown} leaks {hidden}");
        }
        let s = SourceSpec::parse("rtsp://admin:x@cam:554/live?video").unwrap();
        assert_eq!(s.redacted(), "rtsp://***@cam:554/live?video");
    }

    proptest::proptest! {
        #[test]
        fn redaction_never_shows_the_password(pass in "[A-Za-z0-9/?&=%._~-]{4,24}") {
            let s = SourceSpec::parse(&format!("rtsp://admin:{pass}@10.0.0.2:554/stream1")).unwrap();
            let shown = format!(":{pass}@");
            proptest::prop_assert!(!s.redacted().contains(&shown));
            proptest::prop_assert!(s.redacted().starts_with("rtsp://***@"));
        }

        #[test]
        fn parse_never_panics(raw in ".{0,300}") {
            let _ = SourceSpec::parse(&raw).map(|s| (s.redacted(), s.target().len(), s.fragments().count()));
        }
    }
}
