//! The wire format of the test TURN server, written apart from the crate's
//! own STUN code so that the two check each other: STUN messages with
//! long-term credentials, and the framing of a TCP stream (RFC 8656
//! section 3.1: STUN by its header's length, ChannelData by its length
//! padded to four bytes - no length in front, unlike RFC 4571's).
#![allow(dead_code)]

use std::net::{IpAddr, Ipv4Addr, SocketAddr};

use md5::{Digest, Md5};
use tokio::io::{AsyncRead, AsyncReadExt};

pub const COOKIE: u32 = 0x2112_A442;
pub const REALM: &str = "relay.test";
pub const NONCE: &str = "test-nonce";

pub fn md5_key(username: &str, realm: &str, password: &str) -> [u8; 16] {
    Md5::digest(format!("{username}:{realm}:{password}").as_bytes()).into()
}

pub fn hmac(key: &[u8], data: &[u8]) -> [u8; 20] {
    str0m::crypto::from_feature_flags().sha1_hmac_provider.sha1_hmac(key, &[data])
}

// ------------------------------------------------------------- the wire

pub struct Msg {
    pub kind: u16,
    pub tid: [u8; 12],
    pub attrs: Vec<(u16, Vec<u8>)>,
    /// The bytes before MESSAGE-INTEGRITY, and its value.
    integrity: Option<(Vec<u8>, Vec<u8>)>,
}

pub fn parse(data: &[u8]) -> Option<Msg> {
    if data.len() < 20 || u32::from_be_bytes(data[4..8].try_into().ok()?) != COOKIE {
        return None;
    }
    let kind = u16::from_be_bytes([data[0], data[1]]);
    let length = u16::from_be_bytes([data[2], data[3]]) as usize;
    if data.len() != 20 + length {
        return None;
    }
    let mut msg = Msg { kind, tid: data[8..20].try_into().ok()?, attrs: Vec::new(), integrity: None };
    let mut at = 20;
    while at + 4 <= data.len() {
        let t = u16::from_be_bytes([data[at], data[at + 1]]);
        let l = u16::from_be_bytes([data[at + 2], data[at + 3]]) as usize;
        let value = data.get(at + 4..at + 4 + l)?.to_vec();
        if t == 0x0008 {
            // The bytes it covers, with the length up to its end.
            let mut covered = data[..at].to_vec();
            covered[2..4].copy_from_slice(&((at + 24 - 20) as u16).to_be_bytes());
            msg.integrity = Some((covered, value.clone()));
        }
        msg.attrs.push((t, value));
        at += 4 + l.div_ceil(4) * 4;
    }
    Some(msg)
}

impl Msg {
    pub fn get(&self, t: u16) -> Option<&[u8]> {
        self.attrs.iter().find(|(k, _)| *k == t).map(|(_, v)| v.as_slice())
    }

    pub fn peer(&self) -> Option<SocketAddr> {
        let v = self.get(0x0012)?;
        let port = u16::from_be_bytes([v[2], v[3]]) ^ (COOKIE >> 16) as u16;
        let ip = u32::from_be_bytes(v[4..8].try_into().ok()?) ^ COOKIE;
        Some(SocketAddr::new(IpAddr::V4(Ipv4Addr::from(ip)), port))
    }

    pub fn signed_by(&self, key: &[u8]) -> bool {
        self.integrity.as_ref().is_some_and(|(covered, mac)| hmac(key, covered).as_slice() == mac.as_slice())
    }
}

pub fn xor_v4(addr: SocketAddr) -> Vec<u8> {
    let IpAddr::V4(ip) = addr.ip() else { panic!("the test server is IPv4") };
    let mut v = vec![0, 1];
    v.extend_from_slice(&(addr.port() ^ (COOKIE >> 16) as u16).to_be_bytes());
    v.extend_from_slice(&(u32::from(ip) ^ COOKIE).to_be_bytes());
    v
}

/// A message of type `kind`, optionally signed with `key`, always fingerprinted.
pub fn build(kind: u16, tid: [u8; 12], attrs: &[(u16, Vec<u8>)], key: Option<&[u8]>) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&kind.to_be_bytes());
    out.extend_from_slice(&[0, 0]);
    out.extend_from_slice(&COOKIE.to_be_bytes());
    out.extend_from_slice(&tid);
    let push = |out: &mut Vec<u8>, t: u16, v: &[u8]| {
        out.extend_from_slice(&t.to_be_bytes());
        out.extend_from_slice(&(v.len() as u16).to_be_bytes());
        out.extend_from_slice(v);
        out.resize(out.len().div_ceil(4) * 4, 0);
    };
    for (t, v) in attrs {
        push(&mut out, *t, v);
    }
    if let Some(key) = key {
        let len = (out.len() + 24 - 20) as u16;
        out[2..4].copy_from_slice(&len.to_be_bytes());
        let mac = hmac(key, &out);
        push(&mut out, 0x0008, &mac);
    }
    let len = (out.len() + 8 - 20) as u16;
    out[2..4].copy_from_slice(&len.to_be_bytes());
    let crc = crc::Crc::<u32>::new(&crc::CRC_32_ISO_HDLC).checksum(&out) ^ 0x5354_554E;
    push(&mut out, 0x8028, &crc.to_be_bytes());
    out
}

pub fn error(kind: u16, tid: [u8; 12], code: u16, extra: &[(u16, Vec<u8>)]) -> Vec<u8> {
    let mut attrs = vec![(0x0009u16, vec![0, 0, (code / 100) as u8, (code % 100) as u8])];
    attrs.extend_from_slice(extra);
    build(kind | 0x0110, tid, &attrs, None)
}

/// A ChannelData message; padded to four bytes on a stream.
pub fn channel_message(channel: u16, payload: &[u8], padded: bool) -> Vec<u8> {
    let mut out = channel.to_be_bytes().to_vec();
    out.extend_from_slice(&(payload.len() as u16).to_be_bytes());
    out.extend_from_slice(payload);
    if padded {
        out.resize(out.len().div_ceil(4) * 4, 0);
    }
    out
}

/// The next message of a byte stream, as TURN frames it. `Err` when the
/// stream ends or what it holds is neither STUN nor ChannelData.
pub async fn read_message<R: AsyncRead + Unpin>(stream: &mut R) -> std::io::Result<Vec<u8>> {
    let mut head = [0u8; 4];
    stream.read_exact(&mut head).await?;
    let length = u16::from_be_bytes([head[2], head[3]]) as usize;
    let total = match head[0] {
        0x00..=0x3F => 20 + length,
        0x40..=0x4F => 4 + length.div_ceil(4) * 4,
        other => return Err(std::io::Error::other(format!("not TURN: first byte {other:#x}"))),
    };
    let mut message = head.to_vec();
    message.resize(total, 0);
    stream.read_exact(&mut message[4..]).await?;
    Ok(message)
}
