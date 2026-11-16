//! The harness of the multipart reader, for the fuzz target (`fuzz/`) and the
//! property tests: any bytes, cut anywhere, give the same pictures, within the
//! bounds, without a panic.

use crate::multipart::{Parser, body_of};

/// Pictures of at most this many bytes, in the harness: small enough for a
/// short input to reach the bound.
const MAX_PART_STEP: usize = 64;

/// Everything the reader gives for `stream` fed in chunks of `chunk` bytes.
fn read(boundary: Option<&str>, max_part: usize, chunk: usize, stream: &[u8]) -> Result<Vec<Vec<u8>>, String> {
    let mut parser = Parser::new(boundary, max_part);
    let mut parts = Vec::new();
    let mut ended = false;
    for piece in stream.chunks(chunk.max(1)) {
        parser.push(piece);
        loop {
            match parser.next_part() {
                Ok(Some(part)) => {
                    if part.data.is_empty() || part.data.len() > max_part {
                        return Err(format!("a part of {} bytes, bound {max_part}", part.data.len()));
                    }
                    parts.push(part.data.to_vec());
                }
                Ok(None) => break,
                // An error ends the stream: what came before it stands.
                Err(_) => {
                    ended = true;
                    break;
                }
            }
        }
        if ended {
            break;
        }
        if parser.buffered() > parser.limit() {
            return Err(format!("{} bytes held, bound {}", parser.buffered(), parser.limit()));
        }
    }
    Ok(parts)
}

/// `stream` through the reader in chunks of `chunk` bytes and in one piece:
/// the bounds hold, and the parts are the same whatever the cuts (up to where
/// the stream is refused, which a cut may only make earlier).
pub fn multipart(boundary: Option<&str>, max_part: usize, chunk: usize, stream: &[u8]) -> Result<(), String> {
    let split = read(boundary, max_part, chunk, stream)?;
    let whole = read(boundary, max_part, usize::MAX, stream)?;
    // The same pictures up to the shorter list; a refusal may come sooner when cut.
    let common = split.len().min(whole.len());
    if split.get(..common) != whole.get(..common) {
        return Err(format!("chunks of {chunk} gave {split:?}, one piece {whole:?}"));
    }
    if split.len() != whole.len() && split.len() > whole.len() {
        return Err(format!("cutting the stream made pictures out of nothing: {split:?} against {whole:?}"));
    }
    Ok(())
}

/// [`multipart`] from bytes: the first picks the boundary, the next two the
/// bound and the chunk size, the rest is the stream. Also reads the first 200
/// bytes as a `Content-Type`.
pub fn multipart_bytes(data: &[u8]) -> Result<(), String> {
    let Some((&[pick, max, chunk], stream)) = data.split_first_chunk::<3>() else { return Ok(()) };
    let boundary = match pick % 5 {
        0 => None,
        1 => Some("frame"),
        2 => Some("--myboundary"),
        3 => Some("video boundary--"),
        _ => Some("a"),
    };
    let _ = body_of(std::str::from_utf8(stream.get(..stream.len().min(200)).unwrap_or_default()).ok());
    multipart(boundary, MAX_PART_STEP * (usize::from(max) + 1), usize::from(chunk) + 1, stream)
}

#[cfg(test)]
#[path = "fuzzing_tests.rs"]
mod tests;
