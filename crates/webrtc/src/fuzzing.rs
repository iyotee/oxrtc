//! Harnesses for the fuzz targets (`fuzz/`, nightly, `cargo fuzz`) and for
//! the property tests that run the same code on stable in `cargo test`
//! ([`fuzzing_tests.rs`](self)): what a TURN or STUN server, or anyone who
//! can send from its address, makes this server parse.
//!
//! Compiled with the `fuzzing` feature or in tests; never part of a release
//! build. Each harness returns `Err` naming the invariant that broke; a panic
//! is a defect on its own (`CORE-001`).

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::time::{Duration, Instant};

use crate::stun::{
    BadFrame, ChannelData, Class, Encoder, Framer, Hmac, MAX_FRAME, Message, READ, StunError, attr, long_term_key,
    method,
};
use crate::turn::allocation::{Allocation, Credentials, Incoming};
use crate::turn::url;

const REALM: &str = "fuzz.test";
const USERNAME: &str = "1700000000:fuzz";
const PASSWORD: &str = "fuzz-password";

fn hmac() -> Hmac {
    str0m::crypto::from_feature_flags().sha1_hmac_provider
}

/// Any datagram through the STUN parser and every accessor, and through
/// the ChannelData parser.
pub fn stun_datagram(data: &[u8]) -> Result<(), String> {
    if let Ok(message) = Message::parse(data) {
        if message.raw().len() != data.len() {
            return Err("a parsed message is not the whole datagram".into());
        }
        for kind in [attr::XOR_MAPPED_ADDRESS, attr::XOR_RELAYED_ADDRESS, attr::XOR_PEER_ADDRESS] {
            let _ = message.xor_address(kind);
        }
        let _ = (message.u32(attr::LIFETIME), message.channel_number(), message.error_code());
        let _ = (message.text(attr::REALM, 763), message.text(attr::NONCE, 763), message.get(attr::DATA));
        let _ = (message.verify_integrity(b"key", hmac()), message.fingerprint_ok());
    }
    // The fragment a datagram from an unknown address is routed by.
    if let Some(ufrag) = crate::stun::ice_ufrag(data)
        && (ufrag.is_empty() || ufrag.contains(':'))
    {
        return Err(format!("routed by {ufrag:?}, not a fragment"));
    }
    if let Some(channel) = ChannelData::parse(data)
        && channel.data.len() + 4 > data.len()
    {
        return Err("ChannelData longer than its datagram".into());
    }
    Ok(())
}

/// An `ice_servers` URL: one that parses prints as a URL that parses the same.
pub fn ice_url(data: &[u8]) -> Result<(), String> {
    let Ok(text) = std::str::from_utf8(data) else { return Ok(()) };
    if let Ok(parsed) = url::parse(text) {
        let again = url::parse(&parsed.to_string()).map_err(|e| format!("{parsed} does not parse again: {e}"))?;
        if again != parsed {
            return Err(format!("{text:?} parsed as {parsed:?}, printed and parsed as {again:?}"));
        }
    }
    Ok(())
}

/// Pops `n` bytes off the front of `data`.
fn take<'a>(data: &mut &'a [u8], n: usize) -> &'a [u8] {
    let (head, rest) = data.split_at_checked(n.min(data.len())).unwrap_or((data, &[]));
    *data = rest;
    head
}

fn byte(data: &mut &[u8]) -> u8 {
    take(data, 1).first().copied().unwrap_or(0)
}

/// An allocation through its challenge and answer, unless `allocate` is false.
fn allocation(now: Instant, allocate: bool, reliable: bool) -> Allocation {
    let credentials = Credentials { username: USERNAME.into(), password: PASSWORD.into() };
    let mut allocation = if reliable {
        Allocation::reliable(credentials, hmac(), now)
    } else {
        Allocation::new(credentials, hmac(), now)
    };
    if !allocate {
        return allocation;
    }
    let reply = |request: Option<Vec<u8>>, f: &dyn Fn([u8; 12]) -> Vec<u8>| {
        let request = request.unwrap_or_default();
        Message::parse(&request).map(|m| f(m.transaction)).unwrap_or_default()
    };
    let challenge = reply(allocation.poll_transmit(), &|id| {
        Encoder::new(Class::Error, method::ALLOCATE, id)
            .error(401, "Unauthorized")
            .attr(attr::REALM, REALM.as_bytes())
            .attr(attr::NONCE, b"nonce")
            .finish(None, true)
    });
    allocation.handle(now, &challenge);
    let key = long_term_key(USERNAME, REALM, PASSWORD);
    let success = reply(allocation.poll_transmit(), &|id| {
        Encoder::new(Class::Success, method::ALLOCATE, id)
            .xor_address(attr::XOR_RELAYED_ADDRESS, SocketAddr::from(([203, 0, 113, 1], 50_000)))
            .u32(attr::LIFETIME, 600)
            .finish(Some((&key, hmac())), true)
    });
    allocation.handle(now, &success);
    allocation
}

/// A TURN allocation fed what a server (or a spoofer) sends: the input is a
/// script of steps, each a mode byte and its operands - a datagram as is, a
/// datagram made an answer to a request in flight, data sent to a peer, a
/// permission, time passing, over UDP or TCP. Checks the bounds and that no timer is due
/// right after its timeout was handled (a busy loop in the driver).
pub fn turn_datagrams(data: &[u8]) -> Result<(), String> {
    let mut data = data;
    let mut now = Instant::now();
    // Bit 0: past the challenge; bit 1: over a TCP connection (requests sent
    // once, ChannelData padded).
    let mode = byte(&mut data);
    let mut allocation = allocation(now, mode & 1 == 0, mode & 2 != 0);
    let mut steps = 0;
    while !data.is_empty() && steps < 64 {
        steps += 1;
        match byte(&mut data) % 5 {
            mode @ (0 | 1) => {
                let len = usize::from(byte(&mut data)) * 4 + usize::from(byte(&mut data) % 4);
                let mut datagram = take(&mut data, len).to_vec();
                if mode == 1
                    && let Some(id) = allocation.in_flight().first()
                    && let Some(slot) = datagram.get_mut(8..20)
                {
                    slot.copy_from_slice(id);
                }
                stun_datagram(&datagram)?;
                if let Incoming::Relayed { data: relayed, .. } = allocation.handle(now, &datagram)
                    && relayed.len() > datagram.len()
                {
                    return Err("relayed data longer than its datagram".into());
                }
            }
            2 => {
                let peer = SocketAddr::from(([198, 51, 100, byte(&mut data)], u16::from(byte(&mut data)) + 1));
                let len = usize::from(byte(&mut data));
                let payload = take(&mut data, len).to_vec();
                if let Some(out) = allocation.send(now, peer, &payload)
                    && out.len() > payload.len() + 40
                {
                    return Err("a relayed datagram larger than an indication".into());
                }
            }
            3 => allocation.permit(now, IpAddr::V4(Ipv4Addr::new(10, 0, byte(&mut data), byte(&mut data)))),
            _ => {
                now += Duration::from_millis(u64::from(byte(&mut data)) * 250);
                allocation.handle_timeout(now);
                if allocation.poll_timeout().is_some_and(|at| at <= now) {
                    return Err("a timer is still due right after its timeout".into());
                }
            }
        }
        let mut out = 0;
        while allocation.poll_transmit().is_some() {
            out += 1;
        }
        let (permissions, channels) = allocation.peer_counts();
        if out > 256 || permissions > crate::turn::peers::MAX_PERMISSIONS || channels > crate::turn::peers::MAX_CHANNELS
        {
            return Err(format!("unbounded: {out} datagrams, {permissions} permissions, {channels} channels"));
        }
        while allocation.poll_event().is_some() {}
    }
    Ok(())
}

/// What cutting a stream into messages came to: the messages, and why it
/// stopped (the stream is not TURN's), what is left held, and how many bytes
/// were read past (messages beyond [`MAX_FRAME`], never held).
type Cut = (Vec<Vec<u8>>, Option<BadFrame>, usize, usize);

/// Feeds `stream` to a framer in reads of the sizes `size` gives (at most
/// [`READ`]), as the connection's reader does, checking its invariants.
fn cut(stream: &[u8], size: impl Fn(usize) -> usize) -> Result<Cut, String> {
    let mut framer = Framer::default();
    let mut frames = Vec::new();
    let (mut at, mut reads) = (0, 0);
    while at < stream.len() {
        let end = (at + size(reads).clamp(1, READ)).min(stream.len());
        framer.push(stream.get(at..end).unwrap_or_default());
        at = end;
        reads += 1;
        loop {
            match framer.ready() {
                Ok(Some(total)) => {
                    if framer.frame().len() != total || !(4..=MAX_FRAME).contains(&total) {
                        return Err(format!("a message of {total} bytes, framed as {}", framer.frame().len()));
                    }
                    frames.push(framer.frame().to_vec());
                }
                Ok(None) => break,
                Err(bad) => return Ok((frames, Some(bad), framer.buffered(), framer.skipped())),
            }
        }
        if framer.buffered() >= MAX_FRAME {
            return Err(format!("{} bytes held for a message of at most {MAX_FRAME}", framer.buffered()));
        }
    }
    Ok((frames, None, framer.buffered(), framer.skipped()))
}

/// The byte stream of a TURN server over TCP (or of anyone who can send on
/// that connection): cut into messages by every read size, the same messages
/// whatever the reads, each of them a STUN message or a ChannelData message
/// the codec reads as framed and the allocation takes without a panic;
/// nothing buffered beyond a message; every byte in a message, held, or read
/// past (a message beyond the bound, once the stream has spoken).
pub fn turn_stream(data: &[u8]) -> Result<(), String> {
    let mut data = data;
    let mode = usize::from(byte(&mut data));
    let now = Instant::now();
    let mut allocation = allocation(now, mode & 1 == 0, true);
    let (frames, stopped, left, skipped) =
        cut(data, |read| 1 + mode.wrapping_mul(read + 7).wrapping_mul(2_654_435_761) % READ)?;
    let (whole, whole_stopped, _, _) = cut(data, |_| READ)?;
    let (single, single_stopped, _, _) = cut(data, |_| 1)?;
    // Which way a bad stream is refused may depend on how much was read when
    // (the cookie or the length first), that it is refused may not.
    let refused = (stopped.is_some(), whole_stopped.is_some(), single_stopped.is_some());
    if frames != whole || frames != single || refused.0 != refused.1 || refused.0 != refused.2 {
        return Err("the messages depend on how the stream was read".into());
    }
    let taken: usize = frames.iter().map(Vec::len).sum();
    if stopped.is_none() && taken + left + skipped != data.len() {
        return Err(format!("{taken} bytes in messages, {left} held and {skipped} read past, of {} read", data.len()));
    }
    for frame in &frames {
        stun_datagram(frame)?;
        match frame.first() {
            Some(0x40..=0x4F) => {
                if ChannelData::parse(frame).is_none() {
                    return Err("a framed ChannelData message the codec does not read".into());
                }
            }
            _ => {
                if let Err(error @ (StunError::Length { .. } | StunError::Short | StunError::NotStun)) =
                    Message::parse(frame).map(|_| ())
                {
                    return Err(format!("a framed STUN message the codec refuses for its framing: {error}"));
                }
            }
        }
        if let Incoming::Relayed { data: relayed, .. } = allocation.handle(now, frame)
            && relayed.len() > frame.len()
        {
            return Err("relayed data longer than its message".into());
        }
        while allocation.poll_transmit().is_some() {}
        while allocation.poll_event().is_some() {}
    }
    Ok(())
}

#[cfg(test)]
#[path = "fuzzing_tests.rs"]
mod tests;
