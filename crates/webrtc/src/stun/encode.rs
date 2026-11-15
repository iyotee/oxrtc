//! Writing a STUN message.

use std::net::{IpAddr, SocketAddr};

use super::integrity::{fingerprint_of, integrity_of};
use super::{Class, HEADER, Hmac, MAGIC_COOKIE, TransactionId, attr, message_type};

/// Builds one message; attributes are appended in order and padded.
pub(crate) struct Encoder {
    buf: Vec<u8>,
    transaction: TransactionId,
}

impl Encoder {
    pub(crate) fn new(class: Class, method: u16, transaction: TransactionId) -> Self {
        let mut buf = Vec::with_capacity(128);
        buf.extend_from_slice(&message_type(class, method).to_be_bytes());
        buf.extend_from_slice(&[0, 0]);
        buf.extend_from_slice(&MAGIC_COOKIE.to_be_bytes());
        buf.extend_from_slice(&transaction);
        Self { buf, transaction }
    }

    /// Sets the header's length to what follows it.
    fn set_length(&mut self) {
        let length = u16::try_from(self.buf.len().saturating_sub(HEADER)).unwrap_or(u16::MAX).to_be_bytes();
        if let Some(field) = self.buf.get_mut(2..4) {
            field.copy_from_slice(&length);
        }
    }

    /// Appends an attribute. A value longer than an attribute can hold is cut
    /// at 65 532 bytes (callers never pass one: the largest is a datagram).
    pub(crate) fn attr(mut self, kind: u16, value: &[u8]) -> Self {
        let value = value.get(..value.len().min(0xFFFC)).unwrap_or_default();
        self.buf.extend_from_slice(&kind.to_be_bytes());
        self.buf.extend_from_slice(&u16::try_from(value.len()).unwrap_or(0).to_be_bytes());
        self.buf.extend_from_slice(value);
        let pad = value.len().next_multiple_of(4) - value.len();
        self.buf.extend(std::iter::repeat_n(0u8, pad));
        self.set_length();
        self
    }

    pub(crate) fn u32(self, kind: u16, value: u32) -> Self {
        self.attr(kind, &value.to_be_bytes())
    }

    /// An `XOR-*-ADDRESS` attribute.
    pub(crate) fn xor_address(self, kind: u16, addr: SocketAddr) -> Self {
        let value = encode_xor_address(addr, &self.transaction);
        self.attr(kind, &value)
    }

    /// `CHANNEL-NUMBER`.
    pub(crate) fn channel(self, number: u16) -> Self {
        let [a, b] = number.to_be_bytes();
        self.attr(attr::CHANNEL_NUMBER, &[a, b, 0, 0])
    }

    /// `REQUESTED-TRANSPORT` UDP (protocol 17).
    pub(crate) fn udp_transport(self) -> Self {
        self.attr(attr::REQUESTED_TRANSPORT, &[17, 0, 0, 0])
    }

    /// `ERROR-CODE` (used by tests and harnesses that play a server).
    #[cfg(any(test, feature = "fuzzing"))]
    pub(crate) fn error(self, code: u16, reason: &str) -> Self {
        let mut value = vec![0, 0, u8::try_from(code / 100).unwrap_or(0), u8::try_from(code % 100).unwrap_or(0)];
        value.extend_from_slice(reason.as_bytes());
        self.attr(attr::ERROR_CODE, &value)
    }

    /// The message, signed with `key` when given, then fingerprinted.
    pub(crate) fn finish(mut self, key: Option<(&[u8], Hmac)>, fingerprint: bool) -> Vec<u8> {
        if let Some((key, hmac)) = key {
            let at = self.buf.len();
            // Placeholder, so the length covers the attribute while it is computed.
            self = self.attr(attr::MESSAGE_INTEGRITY, &[0; 20]);
            if let Some(mac) = integrity_of(&self.buf, at, key, hmac)
                && let Some(slot) = self.buf.get_mut(at + 4..at + 24)
            {
                slot.copy_from_slice(&mac);
            }
        }
        if fingerprint {
            let at = self.buf.len();
            self = self.attr(attr::FINGERPRINT, &[0; 4]);
            if let Some(crc) = fingerprint_of(&self.buf, at)
                && let Some(slot) = self.buf.get_mut(at + 4..at + 8)
            {
                slot.copy_from_slice(&crc.to_be_bytes());
            }
        }
        self.buf
    }
}

/// An `XOR-*-ADDRESS` value (RFC 8489 section 14.2).
pub(crate) fn encode_xor_address(addr: SocketAddr, transaction: &TransactionId) -> Vec<u8> {
    let port = addr.port() ^ (MAGIC_COOKIE >> 16) as u16;
    let mut value = Vec::with_capacity(20);
    match addr.ip() {
        IpAddr::V4(ip) => {
            value.extend_from_slice(&[0, 0x01]);
            value.extend_from_slice(&port.to_be_bytes());
            value.extend_from_slice(&(u32::from(ip) ^ MAGIC_COOKIE).to_be_bytes());
        }
        IpAddr::V6(ip) => {
            value.extend_from_slice(&[0, 0x02]);
            value.extend_from_slice(&port.to_be_bytes());
            let mask = MAGIC_COOKIE.to_be_bytes().into_iter().chain(transaction.iter().copied());
            value.extend(ip.octets().into_iter().zip(mask).map(|(b, m)| b ^ m));
        }
    }
    value
}
