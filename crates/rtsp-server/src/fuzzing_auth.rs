//! The `Authorization` harness: any bytes through the Digest parameter reader
//! the server runs on a reader's header, and back.

use crate::auth::{MAX_DIGEST_PARAMS, parse_params};

/// Any bytes as an `Authorization` Digest parameter list. Never a panic; at
/// most [`MAX_DIGEST_PARAMS`] pairs; and what was read, written out as a
/// client writes it (quoted, with `\` before `"` and `\`) and read again, is the
/// same pairs.
pub fn authorization(data: &[u8]) -> Result<(), String> {
    let text = String::from_utf8_lossy(data);
    let Some(pairs) = parse_params(&text) else { return Ok(()) };
    if pairs.len() > MAX_DIGEST_PARAMS {
        return Err(format!("{} pairs read, the cap is {MAX_DIGEST_PARAMS}", pairs.len()));
    }
    let written: Vec<String> = pairs
        .iter()
        .map(|(key, value)| format!("{key}=\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\"")))
        .collect();
    let again = parse_params(&written.join(", ")).ok_or_else(|| format!("{pairs:?} written out is not read back"))?;
    if again != pairs {
        return Err(format!("{pairs:?} read back as {again:?}"));
    }
    Ok(())
}
