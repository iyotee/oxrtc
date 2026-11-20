//! The harness of the WAV reader, for the fuzz target (`fuzz/`) and the
//! property tests: any bytes, cut anywhere, give the same samples, within
//! the bounds, without a panic.

use crate::wav::{Format, MAX_HEADER_BYTES, Reader};

/// What the reader gives for `stream` fed in pieces of `chunk` bytes: the
/// format, the samples, and whether it refused the stream.
fn read(chunk: usize, stream: &[u8]) -> Result<(Option<Format>, Vec<u8>, bool), String> {
    let mut reader = Reader::new();
    let mut samples = Vec::new();
    let mut refused = false;
    for piece in stream.chunks(chunk.max(1)) {
        let before = samples.len();
        if reader.push(piece, &mut samples).is_err() {
            refused = true;
            if samples.len() != before {
                return Err("a refused piece gave samples".into());
            }
            break;
        }
        let limit = if reader.format().is_some() {
            reader.format().map_or(1, |f| f.block_align())
        } else {
            MAX_HEADER_BYTES + 1
        };
        if reader.held() >= limit {
            return Err(format!("{} bytes held, bound {limit}", reader.held()));
        }
        if let Some(format) = reader.format()
            && samples.len() % format.block_align() != 0
        {
            return Err(format!("{} bytes of samples, not whole frames of {}", samples.len(), format.block_align()));
        }
    }
    if samples.len() > stream.len() {
        return Err("more samples than bytes".into());
    }
    Ok((reader.format(), samples, refused))
}

/// `stream` through the reader in pieces of `chunk` bytes and in one piece:
/// the bounds hold, the format is the same, and so are the samples (a header
/// cut where it is incomplete is read once it is complete).
pub fn wav(chunk: usize, stream: &[u8]) -> Result<(), String> {
    let (format, split, split_refused) = read(chunk, stream)?;
    let (whole_format, whole, whole_refused) = read(usize::MAX, stream)?;
    if split_refused != whole_refused {
        return Err(format!("refused in pieces of {chunk}: {split_refused}, in one piece: {whole_refused}"));
    }
    if format != whole_format {
        return Err(format!("format {format:?} in pieces of {chunk}, {whole_format:?} in one piece"));
    }
    if split != whole {
        return Err(format!("pieces of {chunk} gave {} bytes of samples, one piece {}", split.len(), whole.len()));
    }
    Ok(())
}

/// [`wav`] from bytes: the first picks the piece size, the rest is the stream.
pub fn wav_bytes(data: &[u8]) -> Result<(), String> {
    let Some((&chunk, stream)) = data.split_first() else { return Ok(()) };
    wav(usize::from(chunk) + 1, stream)
}

#[cfg(test)]
#[path = "fuzzing_tests.rs"]
mod tests;
