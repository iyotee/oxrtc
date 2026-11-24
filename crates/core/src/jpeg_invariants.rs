//! What cutting a byte stream of pictures must keep, whatever the reads:
//! the checks the fuzz target `jpeg_split` and its proptest twins run
//! (`oxrtc_core::jpeg::split_invariants`).

use bytes::Bytes;

use super::{EOI, SOI, SplitError, Splitter};

/// Every picture `data` holds, cut from reads of `read` bytes by a splitter
/// of `max`, and the first error. The fuzz target `jpeg_split` and its
/// proptest twins check what [`split_invariants`] says of it.
#[doc(hidden)]
#[must_use]
pub fn split_all(data: &[u8], read: usize, max: usize) -> (Vec<Bytes>, Option<SplitError>, usize) {
    let mut splitter = Splitter::new(max);
    let (mut pictures, mut error, mut most) = (Vec::new(), None, 0);
    for chunk in data.chunks(read.max(1)) {
        splitter.push(chunk);
        most = most.max(splitter.buffered());
        loop {
            match splitter.next_picture() {
                Ok(Some(picture)) => pictures.push(picture),
                Ok(None) => break,
                Err(e) => {
                    error.get_or_insert(e);
                    break;
                }
            }
        }
    }
    (pictures, error, most)
}

/// What cutting `data` must keep, whatever the reads: each picture runs from
/// a start to an end of image, at most `max` bytes, and stands in `data` in
/// order, without overlapping the one before; the bytes held never pass the
/// limit and one read; cut from reads of `read` bytes or from one, the same
/// pictures when neither met an error; and the pictures written one after
/// the other are cut again into themselves.
#[doc(hidden)]
pub fn split_invariants(data: &[u8], read: usize, max: usize) -> Result<usize, String> {
    let max = max.max(4);
    let (pictures, error, most) = split_all(data, read, max);
    if most > max + read.max(1) + 1 {
        return Err(format!("{most} bytes held for a limit of {max} and reads of {read}"));
    }
    let mut from = 0;
    for picture in &pictures {
        if !picture.starts_with(&[0xff, SOI]) || !picture.ends_with(&[0xff, EOI]) || picture.len() > max {
            return Err(format!("a picture of {} bytes that is not one", picture.len()));
        }
        let rest = data.get(from..).unwrap_or_default();
        let Some(at) = rest.windows(picture.len()).position(|w| w == &picture[..]) else {
            return Err("a picture that is not in the input, in order".to_owned());
        };
        from += at + picture.len();
    }
    let (whole, whole_error, _) = split_all(data, data.len(), max);
    if error.is_none() && whole_error.is_none() && whole != pictures {
        return Err(format!("{} pictures read whole, {} from reads of {read}", whole.len(), pictures.len()));
    }
    let again: Vec<u8> = pictures.iter().flat_map(|p| p.iter().copied()).collect();
    let (twice, twice_error, _) = split_all(&again, read, max);
    if twice_error.is_some() || twice != pictures {
        return Err("the pictures written again are not cut into themselves".to_owned());
    }
    Ok(pictures.len())
}
