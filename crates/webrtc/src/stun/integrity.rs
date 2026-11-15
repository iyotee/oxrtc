//! `MESSAGE-INTEGRITY` with long-term credentials, and `FINGERPRINT`.

use md5::{Digest, Md5};
use str0m::crypto::Sha1HmacProvider;

use super::{HEADER, Message};

/// HMAC-SHA1, from the crypto provider str0m already uses for ICE.
pub(crate) type Hmac = &'static dyn Sha1HmacProvider;

/// XORed into the CRC-32 of a fingerprinted message (RFC 8489 section 14.7).
const FINGERPRINT_XOR: u32 = 0x5354_554E;
const CRC32: crc::Crc<u32> = crc::Crc::<u32>::new(&crc::CRC_32_ISO_HDLC);

/// The long-term credential key (RFC 8489 section 9.2.2):
/// `MD5(username ":" realm ":" password)`. The password is used as given
/// (no SASLprep): TURN REST credentials are base64, ASCII by construction.
pub(crate) fn long_term_key(username: &str, realm: &str, password: &str) -> [u8; 16] {
    let mut md5 = Md5::new();
    md5.update(username.as_bytes());
    md5.update(b":");
    md5.update(realm.as_bytes());
    md5.update(b":");
    md5.update(password.as_bytes());
    md5.finalize().into()
}

/// The header of `message` with its length field replaced by `length`.
fn header_with_length(message: &[u8], length: usize) -> Option<[u8; HEADER]> {
    let mut header: [u8; HEADER] = message.get(..HEADER)?.try_into().ok()?;
    let length = u16::try_from(length).ok()?.to_be_bytes();
    let field = header.get_mut(2..4)?;
    field.copy_from_slice(&length);
    Some(header)
}

/// The HMAC of `message` up to an integrity attribute at offset `at`
/// (header length counting up to the end of that attribute).
pub(crate) fn integrity_of(message: &[u8], at: usize, key: &[u8], hmac: Hmac) -> Option<[u8; 20]> {
    let header = header_with_length(message, at + 4 + 20 - HEADER)?;
    let body = message.get(HEADER..at)?;
    Some(hmac.sha1_hmac(key, &[&header, body]))
}

/// The fingerprint of `message` up to a fingerprint attribute at `at`.
pub(crate) fn fingerprint_of(message: &[u8], at: usize) -> Option<u32> {
    let header = header_with_length(message, at + 8 - HEADER)?;
    let mut digest = CRC32.digest();
    digest.update(&header);
    digest.update(message.get(HEADER..at)?);
    Some(digest.finalize() ^ FINGERPRINT_XOR)
}

/// Constant-time equality: the comparison of an HMAC must not tell an
/// attacker how many of its leading bytes were right.
fn same(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

impl Message<'_> {
    /// Whether the message carries a `MESSAGE-INTEGRITY` made with `key`.
    pub(crate) fn verify_integrity(&self, key: &[u8], hmac: Hmac) -> bool {
        let Some((at, value)) = self.integrity() else { return false };
        integrity_of(self.raw(), at, key, hmac).is_some_and(|expected| same(&expected, value))
    }

    /// Whether the fingerprint, when present, is right (absent is fine).
    pub(crate) fn fingerprint_ok(&self) -> bool {
        match self.fingerprint() {
            None => !self.has_fingerprint(),
            Some((at, value)) => fingerprint_of(self.raw(), at) == Some(value),
        }
    }
}
