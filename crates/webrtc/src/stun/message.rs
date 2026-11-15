//! Parsing a STUN message, and reading its attributes.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

use super::{Class, HEADER, MAGIC_COOKIE, MAX_ATTRIBUTES, TransactionId, attr, split_type};

/// Why a datagram is not a STUN message this client reads.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub(crate) enum StunError {
    #[error("shorter than a STUN header")]
    Short,
    #[error("not a STUN message (first bits or magic cookie)")]
    NotStun,
    #[error("length {declared} disagrees with the datagram's {actual}")]
    Length { declared: usize, actual: usize },
    #[error("an attribute runs past the message")]
    Truncated,
    #[error("more than {MAX_ATTRIBUTES} attributes")]
    TooManyAttributes,
    #[error("an attribute follows FINGERPRINT")]
    AfterFingerprint,
}

/// One attribute: its type, its value, and where it starts in the message.
#[derive(Debug, Clone, Copy)]
struct Raw<'a> {
    kind: u16,
    value: &'a [u8],
    offset: usize,
}

/// A parsed STUN message, borrowing the datagram.
#[derive(Debug, Clone)]
pub(crate) struct Message<'a> {
    pub(crate) class: Class,
    pub(crate) method: u16,
    pub(crate) transaction: TransactionId,
    raw: &'a [u8],
    attrs: Vec<Raw<'a>>,
    /// Offset of `MESSAGE-INTEGRITY`; attributes after it, but a
    /// fingerprint, are not kept.
    integrity: Option<usize>,
    fingerprint: Option<usize>,
}

fn u16_at(data: &[u8], at: usize) -> Option<u16> {
    let bytes = data.get(at..at.checked_add(2)?)?;
    Some(u16::from_be_bytes([*bytes.first()?, *bytes.get(1)?]))
}

fn u32_of(bytes: &[u8]) -> Option<u32> {
    let array: [u8; 4] = bytes.get(..4)?.try_into().ok()?;
    Some(u32::from_be_bytes(array))
}

impl<'a> Message<'a> {
    /// Parses `data`, which must be exactly one message (a UDP datagram).
    pub(crate) fn parse(data: &'a [u8]) -> Result<Self, StunError> {
        let (header, body) = data.split_at_checked(HEADER).ok_or(StunError::Short)?;
        let kind = u16_at(header, 0).ok_or(StunError::Short)?;
        let length = usize::from(u16_at(header, 2).ok_or(StunError::Short)?);
        let cookie = header.get(4..8).and_then(u32_of).ok_or(StunError::Short)?;
        if kind & 0xC000 != 0 || cookie != MAGIC_COOKIE {
            return Err(StunError::NotStun);
        }
        if length % 4 != 0 || length != body.len() {
            return Err(StunError::Length { declared: length, actual: body.len() });
        }
        let transaction: TransactionId =
            header.get(8..HEADER).and_then(|t| t.try_into().ok()).ok_or(StunError::Short)?;
        let (class, method) = split_type(kind);
        let mut message =
            Self { class, method, transaction, raw: data, attrs: Vec::new(), integrity: None, fingerprint: None };
        let mut at = HEADER;
        let mut count = 0usize;
        while at < data.len() {
            if message.fingerprint.is_some() {
                return Err(StunError::AfterFingerprint);
            }
            count += 1;
            if count > MAX_ATTRIBUTES {
                return Err(StunError::TooManyAttributes);
            }
            let kind = u16_at(data, at).ok_or(StunError::Truncated)?;
            let len = usize::from(u16_at(data, at + 2).ok_or(StunError::Truncated)?);
            let start = at + 4;
            let value = data.get(start..start + len).ok_or(StunError::Truncated)?;
            let next = start + len.next_multiple_of(4);
            if next > data.len() {
                return Err(StunError::Truncated);
            }
            let past_integrity = message.integrity.is_some();
            match kind {
                attr::FINGERPRINT => message.fingerprint = Some(at),
                attr::MESSAGE_INTEGRITY if !past_integrity => {
                    message.integrity = Some(at);
                    message.attrs.push(Raw { kind, value, offset: at });
                }
                // RFC 8489 section 14.5: what follows MESSAGE-INTEGRITY is
                // not covered by it, so only a fingerprint is read there.
                _ if past_integrity => {}
                _ => message.attrs.push(Raw { kind, value, offset: at }),
            }
            at = next;
        }
        Ok(message)
    }

    /// The whole datagram.
    pub(crate) fn raw(&self) -> &'a [u8] {
        self.raw
    }

    /// The first value of attribute `kind` (RFC 8489: only the first
    /// occurrence is processed).
    pub(crate) fn get(&self, kind: u16) -> Option<&'a [u8]> {
        self.attrs.iter().find(|a| a.kind == kind).map(|a| a.value)
    }

    /// Offset of `MESSAGE-INTEGRITY`, and its 20-byte value.
    pub(crate) fn integrity(&self) -> Option<(usize, &'a [u8])> {
        let at = self.integrity?;
        let value = self.attrs.iter().find(|a| a.offset == at)?.value;
        (value.len() == 20).then_some((at, value))
    }

    /// Whether a `FINGERPRINT` attribute is present, well formed or not.
    pub(crate) fn has_fingerprint(&self) -> bool {
        self.fingerprint.is_some()
    }

    /// Offset of `FINGERPRINT`, and its value.
    pub(crate) fn fingerprint(&self) -> Option<(usize, u32)> {
        let at = self.fingerprint?;
        let value = self.raw.get(at + 4..at + 8).and_then(u32_of)?;
        // Its length field must say 4.
        (u16_at(self.raw, at + 2)? == 4).then_some((at, value))
    }

    /// A `XOR-*-ADDRESS` attribute.
    pub(crate) fn xor_address(&self, kind: u16) -> Option<SocketAddr> {
        decode_xor_address(self.get(kind)?, &self.transaction)
    }

    /// A 32-bit attribute (`LIFETIME`).
    pub(crate) fn u32(&self, kind: u16) -> Option<u32> {
        let value = self.get(kind)?;
        (value.len() == 4).then(|| u32_of(value)).flatten()
    }

    /// `CHANNEL-NUMBER`: the number, then two reserved bytes (read by the
    /// tests and harnesses that play a server).
    #[cfg(any(test, feature = "fuzzing"))]
    pub(crate) fn channel_number(&self) -> Option<u16> {
        let value = self.get(attr::CHANNEL_NUMBER)?;
        (value.len() == 4).then(|| u16_at(value, 0)).flatten()
    }

    /// `ERROR-CODE`: the code (300 to 699) and its reason phrase.
    pub(crate) fn error_code(&self) -> Option<(u16, &'a str)> {
        let value = self.get(attr::ERROR_CODE)?;
        let class = u16::from(*value.get(2)? & 0x07);
        let number = u16::from(*value.get(3)?);
        if !(3..=6).contains(&class) || number > 99 {
            return None;
        }
        let reason = value.get(4..).and_then(|r| std::str::from_utf8(r).ok()).unwrap_or("");
        Some((class * 100 + number, reason))
    }

    /// A text attribute (`REALM`, `NONCE`, `SOFTWARE`), refused past
    /// `max` bytes or when not UTF-8.
    pub(crate) fn text(&self, kind: u16, max: usize) -> Option<&'a str> {
        let value = self.get(kind)?;
        if value.len() > max {
            return None;
        }
        std::str::from_utf8(value).ok()
    }
}

/// Reads an `XOR-*-ADDRESS` value (RFC 8489 section 14.2).
pub(crate) fn decode_xor_address(value: &[u8], transaction: &TransactionId) -> Option<SocketAddr> {
    let family = *value.get(1)?;
    let port = u16_at(value, 2)? ^ (MAGIC_COOKIE >> 16) as u16;
    match (family, value.len()) {
        (0x01, 8) => {
            let x = u32_of(value.get(4..8)?)? ^ MAGIC_COOKIE;
            Some(SocketAddr::new(IpAddr::V4(Ipv4Addr::from(x)), port))
        }
        (0x02, 20) => {
            let mut mask = [0u8; 16];
            let (cookie, tid) = mask.split_at_mut(4);
            cookie.copy_from_slice(&MAGIC_COOKIE.to_be_bytes());
            tid.copy_from_slice(transaction);
            let mut ip = [0u8; 16];
            for ((out, byte), m) in ip.iter_mut().zip(value.get(4..20)?).zip(mask) {
                *out = byte ^ m;
            }
            Some(SocketAddr::new(IpAddr::V6(Ipv6Addr::from(ip)), port))
        }
        _ => None,
    }
}
