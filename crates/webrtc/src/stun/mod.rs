//! STUN messages (RFC 8489) with the TURN methods and attributes (RFC 8656),
//! and TURN's ChannelData framing: the wire format of the TURN client.
//!
//! str0m parses only the binding requests of its own ICE agent, so this is a
//! codec of its own, strict by construction: a message must be exactly as
//! long as its header says, its attributes must fit, at most
//! [`MAX_ATTRIBUTES`] are read, and nothing after `MESSAGE-INTEGRITY` but a
//! `FINGERPRINT` is believed. Every accessor returns an `Option`; nothing
//! here panics on input (`fuzzing::stun_datagram`, run by
//! `fuzz/fuzz_targets/turn_datagrams.rs` and its proptests).

mod channel;
mod encode;
mod integrity;
mod message;

pub(crate) use channel::{ChannelData, channel_data, is_channel_number};
pub(crate) use encode::Encoder;
pub(crate) use integrity::{Hmac, long_term_key};
pub(crate) use message::Message;

/// The fixed value of every STUN header (RFC 8489 section 5).
pub(crate) const MAGIC_COOKIE: u32 = 0x2112_A442;
/// A STUN header.
pub(crate) const HEADER: usize = 20;
/// Attributes read from one message at most; the rest refuse the message.
pub(crate) const MAX_ATTRIBUTES: usize = 32;

/// A transaction id: 96 random bits.
pub(crate) type TransactionId = [u8; 12];

/// The class of a message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Class {
    Request,
    Indication,
    Success,
    Error,
}

impl Class {
    fn bits(self) -> u16 {
        match self {
            Self::Request => 0b00,
            Self::Indication => 0b01,
            Self::Success => 0b10,
            Self::Error => 0b11,
        }
    }

    fn from_bits(bits: u16) -> Self {
        match bits & 0b11 {
            0b00 => Self::Request,
            0b01 => Self::Indication,
            0b10 => Self::Success,
            _ => Self::Error,
        }
    }
}

/// The methods this client sends or reads.
pub(crate) mod method {
    pub(crate) const BINDING: u16 = 0x001;
    pub(crate) const ALLOCATE: u16 = 0x003;
    pub(crate) const REFRESH: u16 = 0x004;
    pub(crate) const SEND: u16 = 0x006;
    pub(crate) const DATA: u16 = 0x007;
    pub(crate) const CREATE_PERMISSION: u16 = 0x008;
    pub(crate) const CHANNEL_BIND: u16 = 0x009;
}

/// The attributes this client writes or reads.
pub(crate) mod attr {
    pub(crate) const USERNAME: u16 = 0x0006;
    pub(crate) const MESSAGE_INTEGRITY: u16 = 0x0008;
    pub(crate) const ERROR_CODE: u16 = 0x0009;
    pub(crate) const CHANNEL_NUMBER: u16 = 0x000C;
    pub(crate) const LIFETIME: u16 = 0x000D;
    pub(crate) const XOR_PEER_ADDRESS: u16 = 0x0012;
    pub(crate) const DATA: u16 = 0x0013;
    pub(crate) const REALM: u16 = 0x0014;
    pub(crate) const NONCE: u16 = 0x0015;
    pub(crate) const XOR_RELAYED_ADDRESS: u16 = 0x0016;
    pub(crate) const REQUESTED_TRANSPORT: u16 = 0x0019;
    pub(crate) const XOR_MAPPED_ADDRESS: u16 = 0x0020;
    #[cfg(test)]
    pub(crate) const SOFTWARE: u16 = 0x8022;
    pub(crate) const FINGERPRINT: u16 = 0x8028;
}

/// The message type field of `method` in `class` (RFC 8489 section 5: the
/// class bits are interleaved with the method's).
pub(crate) fn message_type(class: Class, method: u16) -> u16 {
    let c = class.bits();
    (method & 0x000F) | ((method & 0x0070) << 1) | ((method & 0x0F80) << 2) | ((c & 0b01) << 4) | ((c & 0b10) << 7)
}

/// The class and method of a message type field.
pub(crate) fn split_type(kind: u16) -> (Class, u16) {
    let class = Class::from_bits(((kind >> 4) & 0b01) | ((kind >> 7) & 0b10));
    let method = (kind & 0x000F) | ((kind >> 1) & 0x0070) | ((kind >> 2) & 0x0F80);
    (class, method)
}

/// Longest `USERNAME` read (RFC 8489 section 14.3: under 513 bytes).
const MAX_USERNAME: usize = 512;

/// The ICE user name fragment a binding request is addressed to: the part of
/// its `USERNAME` before the colon (RFC 8445 section 7.2.2, the receiver's
/// fragment first). `None` for anything else - another method or class, no
/// user name, a datagram that is not one STUN message.
pub(crate) fn ice_ufrag(data: &[u8]) -> Option<&str> {
    if !looks_like_stun(data) {
        return None;
    }
    let message = Message::parse(data).ok()?;
    if message.class != Class::Request || message.method != method::BINDING {
        return None;
    }
    let (ufrag, _) = message.text(attr::USERNAME, MAX_USERNAME)?.split_once(':')?;
    (!ufrag.is_empty()).then_some(ufrag)
}

/// Whether a datagram looks like a STUN message rather than DTLS, RTP or
/// ChannelData (RFC 7983's first-byte ranges, and the cookie).
pub(crate) fn looks_like_stun(data: &[u8]) -> bool {
    match data.get(..8) {
        Some([first, _, _, _, c0, c1, c2, c3]) => {
            *first < 4 && u32::from_be_bytes([*c0, *c1, *c2, *c3]) == MAGIC_COOKIE
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests;
