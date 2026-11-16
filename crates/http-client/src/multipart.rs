//! `multipart/x-mixed-replace`: the stream of JPEG pictures a webcam serves
//! over one HTTP response, cut into its parts as the bytes arrive.
//!
//! The reader is as lenient as the cameras it has to read. The delimiter is
//! `--` and the declared boundary; leading dashes of the declared boundary are
//! optional (`boundary=--video` is sent as `--video` by some, and as
//! `----video` by others), so the delimiter is searched for without them and
//! up to as many dashes are taken off the end of the part before it. A part
//! with a `Content-Length` is cut at it; one without ends at the next
//! delimiter. Anything between a part and the next delimiter is junk and is
//! dropped (a preamble, a stray line break, a part that lied about its
//! length). Parts that say they are not images are skipped.
//!
//! Everything is bounded: the buffer holds one part at most plus its headers,
//! and an input that would take more is an error, not memory.

use bytes::{Buf, Bytes, BytesMut};
use thiserror::Error;

/// Longest header block of a part.
const MAX_HEAD: usize = 8 * 1024;
/// Longest boundary accepted (RFC 2046 allows 70 characters).
const MAX_BOUNDARY: usize = 200;
/// Longest the rest of the delimiter line may be.
const MAX_DELIMITER_LINE: usize = 256;

/// Why the stream cannot be read on.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub(crate) enum MultipartError {
    /// A part, or the stretch before a delimiter, is longer than allowed.
    #[error("a part over {0} bytes")]
    TooLarge(usize),
    /// A header block longer than allowed, or a delimiter without its line end.
    #[error("malformed part headers")]
    Malformed,
    /// The first delimiter could not be found to name the boundary.
    #[error("no boundary")]
    NoBoundary,
}

/// One part: its `Content-Type` as the camera wrote it (cleaned, bounded)
/// and its body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Part {
    pub(crate) content_type: Option<String>,
    pub(crate) data: Bytes,
}

/// What a response's `Content-Type` says its body is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Body {
    /// A stream of parts, with the declared boundary when there is one.
    Parts(Option<String>),
    /// One picture.
    Image,
    /// Something else: the value, cleaned and cut to a printable length.
    Other(String),
}

/// Reads a `Content-Type` value.
pub(crate) fn body_of(content_type: Option<&str>) -> Body {
    let Some(value) = content_type else { return Body::Image };
    let (media, params) = value.split_once(';').unwrap_or((value, ""));
    let media = media.trim().to_ascii_lowercase();
    if media.starts_with("multipart/") {
        return Body::Parts(boundary_of(params));
    }
    if media.starts_with("image/") || media == "application/octet-stream" || media.is_empty() {
        return Body::Image;
    }
    Body::Other(printable(&media, 60))
}

/// The `boundary` parameter of a `Content-Type`, unquoted.
fn boundary_of(params: &str) -> Option<String> {
    let at = params.to_ascii_lowercase().find("boundary")?;
    let rest = params.get(at + "boundary".len()..)?.trim_start().strip_prefix('=')?.trim();
    let value = match rest.strip_prefix('"') {
        Some(quoted) => quoted.split('"').next().unwrap_or_default(),
        None => rest.split(';').next().unwrap_or_default().trim(),
    };
    (!value.is_empty() && value.len() <= MAX_BOUNDARY && value.bytes().all(|b| (0x20..0x7f).contains(&b)))
        .then(|| value.to_owned())
}

/// `text` with anything but printable ASCII replaced and cut to `max` bytes:
/// a remote value that goes into an error message.
pub(crate) fn printable(text: &str, max: usize) -> String {
    text.chars().map(|c| if c.is_ascii_graphic() || c == ' ' { c } else { '?' }).take(max).collect()
}

#[derive(Debug)]
enum State {
    /// Looking for a delimiter; what is before it is junk.
    Delimiter,
    /// Past a delimiter, looking for the end of its line.
    Line,
    /// Reading the headers of a part.
    Head,
    /// Reading the body of a part.
    Body { content_type: Option<String>, length: Option<usize> },
    /// The closing delimiter was read.
    Done,
}

/// The incremental reader. Feed it with [`Self::push`], take parts out with
/// [`Self::next_part`] until it has none, push again.
#[derive(Debug)]
pub(crate) struct Parser {
    /// `--` and the boundary; found in the first line when none is declared.
    delimiter: Option<Vec<u8>>,
    /// Leading dashes the declared boundary had, which a part may end with.
    extra_dashes: usize,
    buf: BytesMut,
    state: State,
    max_part: usize,
    /// How far into `buf` a part's body was searched for the next delimiter.
    searched: usize,
}

impl Parser {
    /// A reader for parts of at most `max_part` bytes. `boundary` is the
    /// declared one, if any; without it the first line that starts with `--`
    /// names it.
    pub(crate) fn new(boundary: Option<&str>, max_part: usize) -> Self {
        let token = boundary.map(|b| b.trim_start_matches('-')).filter(|t| !t.is_empty());
        let extra_dashes = boundary.map_or(0, |b| b.len() - b.trim_start_matches('-').len());
        Self {
            delimiter: token.map(|t| [b"--", t.as_bytes()].concat()),
            extra_dashes,
            buf: BytesMut::new(),
            state: State::Delimiter,
            max_part,
            searched: 0,
        }
    }

    /// Adds bytes read from the camera.
    pub(crate) fn push(&mut self, data: &[u8]) {
        self.buf.extend_from_slice(data);
    }

    /// Bytes held: at most [`Self::limit`] once [`Self::next_part`] has said
    /// it has nothing more.
    #[cfg(any(test, feature = "fuzzing"))]
    pub(crate) fn buffered(&self) -> usize {
        self.buf.len()
    }

    /// What the buffer may hold between two pushes.
    pub(crate) fn limit(&self) -> usize {
        self.max_part + MAX_HEAD + self.delimiter.as_ref().map_or(MAX_BOUNDARY, Vec::len) + MAX_DELIMITER_LINE
    }

    /// Whether the closing delimiter was read: nothing more will come.
    pub(crate) fn is_done(&self) -> bool {
        matches!(self.state, State::Done)
    }

    /// The next complete part, or `None` when more bytes are needed.
    pub(crate) fn next_part(&mut self) -> Result<Option<Part>, MultipartError> {
        loop {
            let step = match self.state {
                State::Delimiter => self.find_delimiter()?,
                State::Line => self.end_of_line()?,
                State::Head => self.head()?,
                State::Body { .. } => match self.body()? {
                    Some(Some(part)) => return Ok(Some(part)),
                    Some(None) => true,
                    None => false,
                },
                State::Done => false,
            };
            if !step {
                if self.buf.len() > self.limit() {
                    return Err(MultipartError::TooLarge(self.max_part));
                }
                return Ok(None);
            }
        }
    }

    /// Drops junk up to the next delimiter. `true` when one was found.
    fn find_delimiter(&mut self) -> Result<bool, MultipartError> {
        if self.delimiter.is_none() {
            let Some(line) = self.first_line()? else { return Ok(false) };
            // A line `--token`, as the delimiter itself: its own dashes count.
            self.extra_dashes = 0;
            self.delimiter = Some(line);
        }
        let Some(delimiter) = &self.delimiter else { return Ok(false) };
        match memchr::memmem::find(&self.buf, delimiter) {
            Some(at) => {
                self.buf.advance(at + delimiter.len());
                self.state = State::Line;
                Ok(true)
            }
            None => {
                // A delimiter cannot start earlier than its length before the end.
                let keep = delimiter.len().saturating_sub(1).min(self.buf.len());
                self.buf.advance(self.buf.len() - keep);
                Ok(false)
            }
        }
    }

    /// With no declared boundary: the first line that starts with `--`.
    fn first_line(&mut self) -> Result<Option<Vec<u8>>, MultipartError> {
        let Some(at) = memchr::memmem::find(&self.buf, b"--") else {
            // Nothing that could start a delimiter: keep one byte, a dash.
            let keep = usize::from(self.buf.last() == Some(&b'-'));
            self.buf.advance(self.buf.len() - keep);
            return Ok(None);
        };
        self.buf.advance(at);
        let Some(end) = memchr::memchr(b'\n', &self.buf) else {
            return if self.buf.len() > MAX_BOUNDARY { Err(MultipartError::NoBoundary) } else { Ok(None) };
        };
        let line = self.buf.get(..end).unwrap_or_default().trim_ascii_end();
        if line.len() > MAX_BOUNDARY || line.len() <= 2 || !line.iter().all(|b| (0x20..0x7f).contains(b)) {
            return Err(MultipartError::NoBoundary);
        }
        Ok(Some(line.to_vec()))
    }

    /// Past the delimiter: a closing `--`, or the rest of the line.
    fn end_of_line(&mut self) -> Result<bool, MultipartError> {
        if self.buf.starts_with(b"--") {
            self.buf.clear();
            self.state = State::Done;
            return Ok(false);
        }
        if self.buf.len() < 2 && self.buf.first().is_none_or(|b| *b == b'-') {
            return Ok(false);
        }
        match memchr::memchr(b'\n', self.buf.get(..MAX_DELIMITER_LINE).unwrap_or(&self.buf)) {
            Some(at) => {
                self.buf.advance(at + 1);
                self.state = State::Head;
                Ok(true)
            }
            None if self.buf.len() >= MAX_DELIMITER_LINE => Err(MultipartError::Malformed),
            None => Ok(false),
        }
    }

    /// The header block of a part, up to its blank line.
    fn head(&mut self) -> Result<bool, MultipartError> {
        let mut start = 0;
        let end = loop {
            // A blank line ends the block: at the very start, or after a line.
            let rest = self.buf.get(start..).unwrap_or_default();
            if rest.starts_with(b"\r\n") {
                break start + 2;
            }
            if rest.starts_with(b"\n") {
                break start + 1;
            }
            match memchr::memchr(b'\n', rest) {
                Some(line) => start += line + 1,
                None if self.buf.len() > MAX_HEAD => return Err(MultipartError::Malformed),
                None => return Ok(false),
            }
            if start > MAX_HEAD {
                return Err(MultipartError::Malformed);
            }
        };
        let block = self.buf.get(..end).unwrap_or_default();
        let (mut content_type, mut length) = (None, None);
        for line in block.split(|b| *b == b'\n') {
            let Ok(line) = std::str::from_utf8(line.trim_ascii()) else { continue };
            let Some((name, value)) = line.split_once(':') else { continue };
            match name.trim().to_ascii_lowercase().as_str() {
                "content-type" => content_type = Some(printable(value.trim(), 100)),
                "content-length" => length = value.trim().parse::<usize>().ok(),
                _ => {}
            }
        }
        self.buf.advance(end);
        self.state = State::Body { content_type, length };
        Ok(true)
    }

    /// The body of a part. `Some(Some(part))` is a part, `Some(None)` a part
    /// skipped (not an image) with the state moved on, `None` more bytes needed.
    fn body(&mut self) -> Result<Option<Option<Part>>, MultipartError> {
        let State::Body { content_type, length } = &self.state else { return Ok(None) };
        let data = match *length {
            Some(length) if length > self.max_part => return Err(MultipartError::TooLarge(self.max_part)),
            Some(length) => {
                if self.buf.len() < length {
                    return Ok(None);
                }
                self.buf.split_to(length).freeze()
            }
            None => {
                let Some(delimiter) = &self.delimiter else { return Ok(None) };
                let from = self.searched.saturating_sub(delimiter.len());
                let Some(at) = memchr::memmem::find(self.buf.get(from..).unwrap_or_default(), delimiter) else {
                    self.searched = self.buf.len();
                    if self.buf.len() > self.max_part + delimiter.len() + 8 {
                        return Err(MultipartError::TooLarge(self.max_part));
                    }
                    return Ok(None);
                };
                let mut end = from + at;
                // The line break before the delimiter, and the dashes the
                // declared boundary had beyond its token, are not the picture's.
                let mut dashes = self.extra_dashes;
                while dashes > 0 && end > 0 && self.buf.get(end - 1) == Some(&b'-') {
                    end -= 1;
                    dashes -= 1;
                }
                end = end.saturating_sub(usize::from(end > 0 && self.buf.get(end - 1) == Some(&b'\n')));
                end = end.saturating_sub(usize::from(end > 0 && self.buf.get(end - 1) == Some(&b'\r')));
                if end > self.max_part {
                    return Err(MultipartError::TooLarge(self.max_part));
                }
                let data = self.buf.split_to(end).freeze();
                self.searched = 0;
                data
            }
        };
        let content_type = content_type.clone();
        self.state = State::Delimiter;
        self.searched = 0;
        if data.is_empty() || content_type.as_deref().is_some_and(|t| !t.to_ascii_lowercase().starts_with("image/")) {
            return Ok(Some(None));
        }
        Ok(Some(Some(Part { content_type, data })))
    }
}

#[cfg(test)]
#[path = "multipart_tests.rs"]
mod tests;
