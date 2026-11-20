//! A WAV stream reader: the RIFF header, then the samples, fed as they come.
//!
//! What a server streams as `audio/wav` (openIVR's sound de-duplicator, a
//! camera's HTTP sound): a header whose sizes are often `0xFFFFFFFF` or 0
//! because the stream has no end, then samples for as long as the connection
//! lasts. Read here: PCM of 16 bits (little-endian), A-law and mu-law
//! (format tags 6 and 7), each also inside `WAVE_FORMAT_EXTENSIBLE`; one to
//! [`MAX_CHANNELS`] channels; rates of 1 to 192 kHz.
//!
//! Bounded: the header, chunks before `data` included, is read within
//! [`MAX_HEADER_BYTES`]; after it nothing is held but less than one sample
//! frame (`block_align` bytes). A `data` chunk with a real size ends the
//! stream at its end. Anything else is refused with a [`WavError`], never a
//! panic: the bytes come from the network.

/// The most bytes read before the first sample: the RIFF header, `fmt `,
/// and the chunks a writer puts before `data` (`LIST`, `fact`).
pub const MAX_HEADER_BYTES: usize = 4096;
/// The most channels a stream may have.
pub const MAX_CHANNELS: u16 = 8;

/// How the samples are coded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Encoding {
    /// 16-bit linear PCM, little-endian.
    Pcm16,
    /// G.711 A-law, a byte a sample.
    Alaw,
    /// G.711 mu-law, a byte a sample.
    Ulaw,
}

/// What a WAV header says.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Format {
    /// The samples' coding.
    pub encoding: Encoding,
    /// Samples per second, per channel.
    pub rate: u32,
    /// Interleaved channels.
    pub channels: u16,
}

impl Format {
    /// Bytes per sample frame (one sample of each channel).
    #[must_use]
    pub fn block_align(&self) -> usize {
        let bytes = if self.encoding == Encoding::Pcm16 { 2 } else { 1 };
        bytes * usize::from(self.channels)
    }
}

/// Why a stream is refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WavError {
    /// It does not start as a RIFF WAVE file.
    NotWav,
    /// Its header is longer than [`MAX_HEADER_BYTES`].
    HeaderTooLong,
    /// A `fmt ` chunk too short, or a `data` chunk before it.
    Malformed(&'static str),
    /// A coding, a sample size, a channel count or a rate this reader does not take.
    Unsupported(String),
}

impl std::fmt::Display for WavError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotWav => f.write_str("not a WAV stream"),
            Self::HeaderTooLong => write!(f, "a WAV header longer than {MAX_HEADER_BYTES} bytes"),
            Self::Malformed(why) => write!(f, "a malformed WAV header: {why}"),
            Self::Unsupported(what) => write!(f, "a WAV stream of {what}"),
        }
    }
}

impl std::error::Error for WavError {}

#[derive(Debug, Clone, PartialEq, Eq)]
enum State {
    /// Reading the header: every byte so far is in `held`.
    Header,
    /// Reading samples; `remaining` bytes of the `data` chunk are left, or
    /// it has no end.
    Data { format: Format, remaining: Option<u64> },
    /// The `data` chunk ended, or the stream was refused.
    Done,
}

/// Reads one WAV stream.
#[derive(Debug, Clone)]
pub struct Reader {
    state: State,
    held: Vec<u8>,
    /// What the header said, kept once the data chunk has ended.
    format: Option<Format>,
}

impl Default for Reader {
    fn default() -> Self {
        Self::new()
    }
}

fn u16_at(bytes: &[u8], at: usize) -> Option<u16> {
    bytes.get(at..at + 2).and_then(|b| b.first_chunk::<2>()).map(|b| u16::from_le_bytes(*b))
}

fn u32_at(bytes: &[u8], at: usize) -> Option<u32> {
    bytes.get(at..at + 4).and_then(|b| b.first_chunk::<4>()).map(|b| u32::from_le_bytes(*b))
}

/// A `data` size that says the stream has no end: the largest values, and
/// 0, which writers of a live stream put there too.
fn endless(size: u32) -> bool {
    size == 0 || size >= 0xFFFF_FF00
}

/// The format a `fmt ` chunk body says.
fn format_of(body: &[u8]) -> Result<Format, WavError> {
    let (Some(mut tag), Some(channels), Some(rate), Some(bits)) =
        (u16_at(body, 0), u16_at(body, 2), u32_at(body, 4), u16_at(body, 14))
    else {
        return Err(WavError::Malformed("a fmt chunk shorter than 16 bytes"));
    };
    if tag == 0xFFFE {
        // WAVE_FORMAT_EXTENSIBLE: the sub-format's first two bytes are the tag.
        tag = u16_at(body, 24).ok_or(WavError::Malformed("an extensible fmt chunk shorter than 40 bytes"))?;
    }
    let encoding = match (tag, bits) {
        (1, 16) => Encoding::Pcm16,
        (6, 8) => Encoding::Alaw,
        (7, 8) => Encoding::Ulaw,
        (tag, bits) => return Err(WavError::Unsupported(format!("format {tag} with {bits}-bit samples"))),
    };
    if channels == 0 || channels > MAX_CHANNELS {
        return Err(WavError::Unsupported(format!("{channels} channels")));
    }
    if !crate::resample::RATES.contains(&rate) {
        return Err(WavError::Unsupported(format!("{rate} samples a second")));
    }
    Ok(Format { encoding, rate, channels })
}

/// What a complete header in `bytes` says, and where its samples start;
/// `None` while it is not all there.
fn header(bytes: &[u8]) -> Result<Option<(Format, Option<u64>, usize)>, WavError> {
    let seen = bytes.len().min(4);
    if bytes.get(..seen) != b"RIFF".get(..seen) || bytes.get(8..12).is_some_and(|m| m != b"WAVE") {
        return Err(WavError::NotWav);
    }
    let mut at = 12;
    let mut format = None;
    loop {
        let (Some(id), Some(size)) = (bytes.get(at..at + 4), u32_at(bytes, at + 4)) else { return Ok(None) };
        let body = at + 8;
        if id == b"data" {
            let format = format.ok_or(WavError::Malformed("a data chunk before the fmt chunk"))?;
            let remaining = (!endless(size)).then_some(u64::from(size));
            return Ok(Some((format, remaining, body)));
        }
        let size = usize::try_from(size).map_err(|_| WavError::HeaderTooLong)?;
        // Chunks are padded to an even size.
        let end = body.checked_add(size).filter(|&end| end + (size & 1) <= MAX_HEADER_BYTES);
        let end = end.ok_or(WavError::HeaderTooLong)?;
        if id == b"fmt " {
            let Some(chunk) = bytes.get(body..end) else { return Ok(None) };
            format = Some(format_of(chunk)?);
        }
        at = end + (size & 1);
    }
}

impl Reader {
    /// A reader at the start of a stream.
    #[must_use]
    pub fn new() -> Self {
        Self { state: State::Header, held: Vec::new(), format: None }
    }

    /// The stream's format, once its header is read.
    #[must_use]
    pub fn format(&self) -> Option<Format> {
        self.format
    }

    /// Whether the `data` chunk has ended: nothing more will be read.
    #[must_use]
    pub fn finished(&self) -> bool {
        self.state == State::Done
    }

    /// Bytes held between two calls: at most [`MAX_HEADER_BYTES`] while the
    /// header is read, then less than one sample frame.
    #[must_use]
    pub fn held(&self) -> usize {
        self.held.len()
    }

    /// Reads `input`, which follows what came before, and appends the whole
    /// sample frames it completes to `samples`, still coded as the header says.
    ///
    /// # Errors
    ///
    /// The stream is not one this reader takes; it reads nothing more.
    pub fn push(&mut self, input: &[u8], samples: &mut Vec<u8>) -> Result<(), WavError> {
        let mut input = input;
        if self.state == State::Header {
            let room = MAX_HEADER_BYTES.saturating_sub(self.held.len());
            let (taken, rest) = input.split_at(input.len().min(room));
            self.held.extend_from_slice(taken);
            match header(&self.held) {
                Err(error) => {
                    self.state = State::Done;
                    self.held.clear();
                    return Err(error);
                }
                Ok(None) if self.held.len() >= MAX_HEADER_BYTES => {
                    self.state = State::Done;
                    self.held.clear();
                    return Err(WavError::HeaderTooLong);
                }
                Ok(None) => return Ok(()),
                Ok(Some((format, remaining, start))) => {
                    self.state = State::Data { format, remaining };
                    self.format = Some(format);
                    let after: Vec<u8> = self.held.get(start..).unwrap_or_default().to_vec();
                    self.held.clear();
                    self.data(&after, samples);
                    input = rest;
                }
            }
        }
        self.data(input, samples);
        Ok(())
    }

    fn data(&mut self, input: &[u8], samples: &mut Vec<u8>) {
        let State::Data { format, remaining } = &mut self.state else { return };
        let mut input = input;
        if let Some(left) = remaining {
            let take = usize::try_from(*left).unwrap_or(usize::MAX).min(input.len());
            input = input.get(..take).unwrap_or_default();
            *left -= take as u64;
        }
        let align = format.block_align().max(1);
        let ended = *remaining == Some(0);
        let total = self.held.len() + input.len();
        let whole = total - total % align;
        if self.held.is_empty() {
            samples.extend_from_slice(input.get(..whole).unwrap_or_default());
            self.held.extend_from_slice(input.get(whole..).unwrap_or_default());
        } else {
            self.held.extend_from_slice(input);
            samples.extend_from_slice(self.held.get(..whole).unwrap_or_default());
            self.held.drain(..whole);
        }
        if ended {
            self.held.clear();
            self.state = State::Done;
        }
    }
}

#[cfg(test)]
#[path = "wav_tests.rs"]
mod tests;
