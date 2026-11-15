//! A minimal TURN server (RFC 8656 over UDP, long-term credentials) for the
//! tests, written apart from the crate's own STUN code so that the two check
//! each other: Allocate with a 401 challenge, Refresh, CreatePermission,
//! ChannelBind, Send/Data indications and ChannelData. One allocation per
//! client address, a relay socket each on 127.0.0.1.
#![allow(dead_code)]

use std::collections::{HashMap, HashSet};
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use md5::{Digest, Md5};
use oxrtc_core::CancellationToken;
use tokio::net::UdpSocket;

const COOKIE: u32 = 0x2112_A442;
pub const REALM: &str = "relay.test";
const NONCE: &str = "test-nonce";

#[derive(Default)]
pub struct Stats {
    pub allocations: AtomicUsize,
    pub refused: AtomicUsize,
    pub permissions: Mutex<HashSet<IpAddr>>,
    pub channels: AtomicUsize,
    /// Payload bytes relayed to peers, and back to clients.
    pub to_peers: AtomicU64,
    pub to_clients: AtomicU64,
    pub releases: AtomicUsize,
}

struct Alloc {
    relay: Arc<UdpSocket>,
    permissions: HashSet<IpAddr>,
    channels: HashMap<u16, SocketAddr>,
}

type Allocs = Arc<Mutex<HashMap<SocketAddr, Alloc>>>;

pub struct TurnServer {
    pub addr: SocketAddr,
    pub stats: Arc<Stats>,
    cancel: CancellationToken,
}

impl Drop for TurnServer {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

impl TurnServer {
    pub async fn start(username: &str, password: &str) -> Self {
        let socket = Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
        let addr = socket.local_addr().unwrap();
        let stats = Arc::new(Stats::default());
        let cancel = CancellationToken::new();
        let key = md5_key(username, REALM, password);
        tokio::spawn(serve(socket, username.to_owned(), key, stats.clone(), cancel.clone()));
        Self { addr, stats, cancel }
    }
}

pub fn md5_key(username: &str, realm: &str, password: &str) -> [u8; 16] {
    Md5::digest(format!("{username}:{realm}:{password}").as_bytes()).into()
}

fn hmac(key: &[u8], data: &[u8]) -> [u8; 20] {
    str0m::crypto::from_feature_flags().sha1_hmac_provider.sha1_hmac(key, &[data])
}

// ------------------------------------------------------------- the wire

struct Msg {
    kind: u16,
    tid: [u8; 12],
    attrs: Vec<(u16, Vec<u8>)>,
    /// The bytes before MESSAGE-INTEGRITY, and its value.
    integrity: Option<(Vec<u8>, Vec<u8>)>,
}

fn parse(data: &[u8]) -> Option<Msg> {
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
    fn get(&self, t: u16) -> Option<&[u8]> {
        self.attrs.iter().find(|(k, _)| *k == t).map(|(_, v)| v.as_slice())
    }

    fn peer(&self) -> Option<SocketAddr> {
        let v = self.get(0x0012)?;
        let port = u16::from_be_bytes([v[2], v[3]]) ^ (COOKIE >> 16) as u16;
        let ip = u32::from_be_bytes(v[4..8].try_into().ok()?) ^ COOKIE;
        Some(SocketAddr::new(IpAddr::V4(Ipv4Addr::from(ip)), port))
    }

    fn signed_by(&self, key: &[u8]) -> bool {
        self.integrity.as_ref().is_some_and(|(covered, mac)| hmac(key, covered).as_slice() == mac.as_slice())
    }
}

fn xor_v4(addr: SocketAddr) -> Vec<u8> {
    let IpAddr::V4(ip) = addr.ip() else { panic!("the test server is IPv4") };
    let mut v = vec![0, 1];
    v.extend_from_slice(&(addr.port() ^ (COOKIE >> 16) as u16).to_be_bytes());
    v.extend_from_slice(&(u32::from(ip) ^ COOKIE).to_be_bytes());
    v
}

/// A message of type `kind`, optionally signed with `key`, always fingerprinted.
fn build(kind: u16, tid: [u8; 12], attrs: &[(u16, Vec<u8>)], key: Option<&[u8]>) -> Vec<u8> {
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

fn error(kind: u16, tid: [u8; 12], code: u16, extra: &[(u16, Vec<u8>)]) -> Vec<u8> {
    let mut attrs = vec![(0x0009u16, vec![0, 0, (code / 100) as u8, (code % 100) as u8])];
    attrs.extend_from_slice(extra);
    build(kind | 0x0110, tid, &attrs, None)
}

// ------------------------------------------------------------- the server

async fn serve(socket: Arc<UdpSocket>, username: String, key: [u8; 16], stats: Arc<Stats>, cancel: CancellationToken) {
    let allocs: Allocs = Arc::default();
    let mut buf = vec![0u8; 65_536];
    loop {
        let (n, client) = tokio::select! {
            () = cancel.cancelled() => return,
            r = socket.recv_from(&mut buf) => match r { Ok(r) => r, Err(_) => continue },
        };
        let data = &buf[..n];
        if (0x40..=0x4F).contains(&data[0]) {
            let channel = u16::from_be_bytes([data[0], data[1]]);
            let len = u16::from_be_bytes([data[2], data[3]]) as usize;
            let relay =
                allocs.lock().unwrap().get(&client).and_then(|a| Some((a.relay.clone(), *a.channels.get(&channel)?)));
            if let (Some((relay, peer)), Some(payload)) = (relay, data.get(4..4 + len)) {
                stats.to_peers.fetch_add(payload.len() as u64, Ordering::Relaxed);
                let _ = relay.send_to(payload, peer).await;
            }
            continue;
        }
        let Some(msg) = parse(data) else { continue };
        let method = msg.kind & 0x3EEF;
        if msg.kind == 0x0016 {
            // Send indication.
            let relay = allocs.lock().unwrap().get(&client).and_then(|a| {
                let peer = msg.peer()?;
                a.permissions.contains(&peer.ip()).then(|| (a.relay.clone(), peer))
            });
            if let (Some((relay, peer)), Some(payload)) = (relay, msg.get(0x0013)) {
                stats.to_peers.fetch_add(payload.len() as u64, Ordering::Relaxed);
                let _ = relay.send_to(payload, peer).await;
            }
            continue;
        }
        if msg.kind & 0x0110 != 0 {
            continue; // not a request
        }
        let authenticated = msg.get(0x0006) == Some(username.as_bytes()) && msg.signed_by(&key);
        if !authenticated {
            if msg.get(0x0006).is_some() {
                stats.refused.fetch_add(1, Ordering::Relaxed);
            }
            let challenge = [(0x0014, REALM.as_bytes().to_vec()), (0x0015, NONCE.as_bytes().to_vec())];
            let _ = socket.send_to(&error(method, msg.tid, 401, &challenge), client).await;
            continue;
        }
        let ok = |attrs: &[(u16, Vec<u8>)]| build(method | 0x0100, msg.tid, attrs, Some(&key));
        let reply = match method {
            0x0003 => {
                let relay = Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
                let relayed = relay.local_addr().unwrap();
                allocs.lock().unwrap().insert(
                    client,
                    Alloc { relay: relay.clone(), permissions: HashSet::new(), channels: HashMap::new() },
                );
                stats.allocations.fetch_add(1, Ordering::Relaxed);
                tokio::spawn(relay_back(relay, socket.clone(), client, allocs.clone(), stats.clone(), cancel.clone()));
                ok(&[(0x0016, xor_v4(relayed)), (0x0020, xor_v4(client)), (0x000D, 600u32.to_be_bytes().to_vec())])
            }
            0x0004 => {
                let lifetime = msg.get(0x000D).map(|v| u32::from_be_bytes(v.try_into().unwrap())).unwrap_or(600);
                if lifetime == 0 {
                    allocs.lock().unwrap().remove(&client);
                    stats.releases.fetch_add(1, Ordering::Relaxed);
                }
                ok(&[(0x000D, lifetime.to_be_bytes().to_vec())])
            }
            0x0008 => {
                let Some(peer) = msg.peer() else { continue };
                if let Some(a) = allocs.lock().unwrap().get_mut(&client) {
                    a.permissions.insert(peer.ip());
                }
                stats.permissions.lock().unwrap().insert(peer.ip());
                ok(&[])
            }
            0x0009 => {
                let (Some(peer), Some(number)) = (msg.peer(), msg.get(0x000C)) else { continue };
                let number = u16::from_be_bytes([number[0], number[1]]);
                if let Some(a) = allocs.lock().unwrap().get_mut(&client) {
                    a.channels.insert(number, peer);
                    a.permissions.insert(peer.ip());
                }
                stats.channels.fetch_add(1, Ordering::Relaxed);
                ok(&[])
            }
            _ => error(method, msg.tid, 400, &[]),
        };
        let _ = socket.send_to(&reply, client).await;
    }
}

/// A peer's datagrams to a relayed address, back to the client: through
/// its channel when bound, in a Data indication otherwise; dropped without
/// a permission for the peer's IP.
async fn relay_back(
    relay: Arc<UdpSocket>,
    control: Arc<UdpSocket>,
    client: SocketAddr,
    allocs: Allocs,
    stats: Arc<Stats>,
    cancel: CancellationToken,
) {
    let mut buf = vec![0u8; 65_536];
    let mut tid = 0u64;
    loop {
        let (n, peer) = tokio::select! {
            () = cancel.cancelled() => return,
            r = relay.recv_from(&mut buf) => match r { Ok(r) => r, Err(_) => continue },
        };
        let route = {
            let allocs = allocs.lock().unwrap();
            let Some(a) = allocs.get(&client) else { return };
            if !a.permissions.contains(&peer.ip()) {
                continue;
            }
            a.channels.iter().find(|(_, p)| **p == peer).map(|(n, _)| *n)
        };
        let payload = &buf[..n];
        let datagram = match route {
            Some(channel) => {
                let mut out = channel.to_be_bytes().to_vec();
                out.extend_from_slice(&(n as u16).to_be_bytes());
                out.extend_from_slice(payload);
                out
            }
            None => {
                tid += 1;
                let mut id = [0u8; 12];
                id[4..].copy_from_slice(&tid.to_be_bytes());
                build(0x0017, id, &[(0x0012, xor_v4(peer)), (0x0013, payload.to_vec())], None)
            }
        };
        stats.to_clients.fetch_add(n as u64, Ordering::Relaxed);
        let _ = control.send_to(&datagram, client).await;
    }
}
