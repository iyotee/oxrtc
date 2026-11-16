//! A minimal TURN server (RFC 8656, long-term credentials) for the tests,
//! over UDP and over TCP, written apart from the crate's own STUN code so
//! that the two check each other: Allocate with a 401 challenge, Refresh,
//! CreatePermission, ChannelBind, Send/Data indications and ChannelData. One
//! allocation per client address (a TCP client is its connection's address),
//! a relay socket each on 127.0.0.1. Over TCP the stream is framed as TURN
//! frames it (`turn_wire::read_message`): a client that does not pad its
//! ChannelData loses the stream's alignment and its allocation with it.
#![allow(dead_code)]

use std::collections::{HashMap, HashSet};
use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use oxrtc_core::CancellationToken;
use tokio::io::AsyncWriteExt;
use tokio::net::{TcpListener, UdpSocket};
use tokio::sync::mpsc;

use super::turn_wire::{NONCE, REALM, build, channel_message, error, md5_key, parse, read_message, xor_v4};

#[derive(Default)]
pub struct Stats {
    /// Allocations made, over either transport; `tcp_allocations` of them over TCP.
    pub allocations: AtomicUsize,
    pub tcp_allocations: AtomicUsize,
    /// TCP connections accepted.
    pub tcp_connections: AtomicUsize,
    pub refused: AtomicUsize,
    pub permissions: Mutex<HashSet<IpAddr>>,
    pub channels: AtomicUsize,
    /// Payload bytes relayed to peers, and back to clients.
    pub to_peers: AtomicU64,
    pub to_clients: AtomicU64,
    pub releases: AtomicUsize,
    /// The user name of each allocation made, in order.
    pub allocated_as: Mutex<Vec<String>>,
}

/// Where a client's answers go.
#[derive(Clone)]
enum Out {
    Udp(Arc<UdpSocket>),
    Tcp(mpsc::UnboundedSender<Vec<u8>>),
}

impl Out {
    async fn send(&self, message: Vec<u8>, client: SocketAddr) {
        match self {
            Self::Udp(socket) => {
                let _ = socket.send_to(&message, client).await;
            }
            Self::Tcp(queue) => {
                let _ = queue.send(message);
            }
        }
    }

    fn is_tcp(&self) -> bool {
        matches!(self, Self::Tcp(_))
    }
}

struct Alloc {
    relay: Arc<UdpSocket>,
    permissions: HashSet<IpAddr>,
    channels: HashMap<u16, SocketAddr>,
}

type Allocs = Arc<Mutex<HashMap<SocketAddr, Alloc>>>;

struct Shared {
    /// The user names the server accepts, with their keys.
    users: Mutex<Vec<(String, [u8; 16])>>,
    stats: Arc<Stats>,
    allocs: Allocs,
    cancel: CancellationToken,
}

pub struct TurnServer {
    /// The UDP address; `tcp_addr` is the TCP one (another port).
    pub addr: SocketAddr,
    pub tcp_addr: SocketAddr,
    pub stats: Arc<Stats>,
    shared: Arc<Shared>,
    cancel: CancellationToken,
    /// The token of the TCP connections open now: cancelled by a reset.
    connections: Arc<Mutex<CancellationToken>>,
}

impl Drop for TurnServer {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

impl TurnServer {
    pub async fn start(username: &str, password: &str) -> Self {
        let socket = Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let (addr, tcp_addr) = (socket.local_addr().unwrap(), listener.local_addr().unwrap());
        let stats = Arc::new(Stats::default());
        let cancel = CancellationToken::new();
        let shared = Arc::new(Shared {
            users: Mutex::new(vec![(username.to_owned(), md5_key(username, REALM, password))]),
            stats: stats.clone(),
            allocs: Arc::default(),
            cancel: cancel.clone(),
        });
        let connections = Arc::new(Mutex::new(CancellationToken::new()));
        tokio::spawn(serve_udp(socket, shared.clone()));
        tokio::spawn(accept(listener, shared.clone(), connections.clone()));
        Self { addr, tcp_addr, stats, shared, cancel, connections }
    }

    /// Accepts one more user name, as a relay does when its credentials rotate.
    pub fn accept_also(&self, username: &str, password: &str) {
        let key = md5_key(username, REALM, password);
        self.shared.users.lock().unwrap().push((username.to_owned(), key));
    }

    /// Stops accepting a user name: its allocations are refused from now on.
    pub fn forget(&self, username: &str) {
        self.shared.users.lock().unwrap().retain(|(name, _)| name != username);
    }

    /// Resets every TCP connection open now, as a NAT that forgot it would.
    pub fn reset_tcp_connections(&self) {
        let mut current = self.connections.lock().unwrap();
        current.cancel();
        *current = CancellationToken::new();
    }
}

// ------------------------------------------------------------- the server

async fn serve_udp(socket: Arc<UdpSocket>, shared: Arc<Shared>) {
    let out = Out::Udp(socket.clone());
    let mut buf = vec![0u8; 65_536];
    loop {
        let (n, client) = tokio::select! {
            () = shared.cancel.cancelled() => return,
            r = socket.recv_from(&mut buf) => match r { Ok(r) => r, Err(_) => continue },
        };
        handle(&shared, &buf[..n], client, &out).await;
    }
}

async fn accept(listener: TcpListener, shared: Arc<Shared>, connections: Arc<Mutex<CancellationToken>>) {
    loop {
        let (stream, client) = tokio::select! {
            () = shared.cancel.cancelled() => return,
            r = listener.accept() => match r { Ok(r) => r, Err(_) => continue },
        };
        shared.stats.tcp_connections.fetch_add(1, Ordering::Relaxed);
        let reset = connections.lock().unwrap().clone();
        tokio::spawn(serve_tcp(stream, client, shared.clone(), reset));
    }
}

/// One TCP connection: its allocation ends with it (RFC 8656 section 6.1: the
/// allocation belongs to the 5-tuple).
async fn serve_tcp(stream: tokio::net::TcpStream, client: SocketAddr, shared: Arc<Shared>, reset: CancellationToken) {
    let (mut reader, mut writer) = stream.into_split();
    let (queue, mut messages) = mpsc::unbounded_channel::<Vec<u8>>();
    let out = Out::Tcp(queue);
    let end = CancellationToken::new();
    let write_end = end.clone();
    tokio::spawn(async move {
        loop {
            tokio::select! {
                () = write_end.cancelled() => return,
                message = messages.recv() => {
                    let Some(message) = message else { return };
                    if writer.write_all(&message).await.is_err() {
                        return;
                    }
                }
            }
        }
    });
    loop {
        let message = tokio::select! {
            () = shared.cancel.cancelled() => break,
            () = reset.cancelled() => break,
            message = read_message(&mut reader) => message,
        };
        let Ok(message) = message else { break };
        handle(&shared, &message, client, &out).await;
    }
    shared.allocs.lock().unwrap().remove(&client);
    end.cancel();
}

/// One message from `client`.
async fn handle(shared: &Arc<Shared>, data: &[u8], client: SocketAddr, out: &Out) {
    let (stats, allocs) = (&shared.stats, &shared.allocs);
    let Some(&first) = data.first() else { return };
    if (0x40..=0x4F).contains(&first) {
        let channel = u16::from_be_bytes([data[0], data[1]]);
        let len = u16::from_be_bytes([data[2], data[3]]) as usize;
        let relay =
            allocs.lock().unwrap().get(&client).and_then(|a| Some((a.relay.clone(), *a.channels.get(&channel)?)));
        if let (Some((relay, peer)), Some(payload)) = (relay, data.get(4..4 + len)) {
            stats.to_peers.fetch_add(payload.len() as u64, Ordering::Relaxed);
            let _ = relay.send_to(payload, peer).await;
        }
        return;
    }
    let Some(msg) = parse(data) else { return };
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
        return;
    }
    if msg.kind & 0x0110 != 0 {
        return; // not a request
    }
    let named = msg.get(0x0006).and_then(|name| {
        let users = shared.users.lock().unwrap();
        users.iter().find(|(user, _)| user.as_bytes() == name).map(|(user, key)| (user.clone(), *key))
    });
    let (user, key) = named.unwrap_or_default();
    let authenticated = !user.is_empty() && msg.signed_by(&key);
    if !authenticated {
        if msg.get(0x0006).is_some() {
            stats.refused.fetch_add(1, Ordering::Relaxed);
        }
        let challenge = [(0x0014, REALM.as_bytes().to_vec()), (0x0015, NONCE.as_bytes().to_vec())];
        out.send(error(method, msg.tid, 401, &challenge), client).await;
        return;
    }
    let ok = |attrs: &[(u16, Vec<u8>)]| build(method | 0x0100, msg.tid, attrs, Some(&key));
    let reply = match method {
        0x0003 => {
            let relay = Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
            let relayed = relay.local_addr().unwrap();
            allocs
                .lock()
                .unwrap()
                .insert(client, Alloc { relay: relay.clone(), permissions: HashSet::new(), channels: HashMap::new() });
            stats.allocations.fetch_add(1, Ordering::Relaxed);
            stats.allocated_as.lock().unwrap().push(user.clone());
            if out.is_tcp() {
                stats.tcp_allocations.fetch_add(1, Ordering::Relaxed);
            }
            tokio::spawn(relay_back(relay, out.clone(), client, shared.clone()));
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
            let Some(peer) = msg.peer() else { return };
            if let Some(a) = allocs.lock().unwrap().get_mut(&client) {
                a.permissions.insert(peer.ip());
            }
            stats.permissions.lock().unwrap().insert(peer.ip());
            ok(&[])
        }
        0x0009 => {
            let (Some(peer), Some(number)) = (msg.peer(), msg.get(0x000C)) else { return };
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
    out.send(reply, client).await;
}

/// A peer's datagrams to a relayed address, back to the client: through
/// its channel when bound, in a Data indication otherwise; dropped without
/// a permission for the peer's IP.
async fn relay_back(relay: Arc<UdpSocket>, out: Out, client: SocketAddr, shared: Arc<Shared>) {
    let mut buf = vec![0u8; 65_536];
    let mut tid = 0u64;
    loop {
        let (n, peer) = tokio::select! {
            () = shared.cancel.cancelled() => return,
            r = relay.recv_from(&mut buf) => match r { Ok(r) => r, Err(_) => continue },
        };
        let route = {
            let allocs = shared.allocs.lock().unwrap();
            let Some(a) = allocs.get(&client) else { return };
            if !a.permissions.contains(&peer.ip()) {
                continue;
            }
            a.channels.iter().find(|(_, p)| **p == peer).map(|(n, _)| *n)
        };
        let payload = &buf[..n];
        let datagram = match route {
            Some(channel) => channel_message(channel, payload, out.is_tcp()),
            None => {
                tid += 1;
                let mut id = [0u8; 12];
                id[4..].copy_from_slice(&tid.to_be_bytes());
                build(0x0017, id, &[(0x0012, xor_v4(peer)), (0x0013, payload.to_vec())], None)
            }
        };
        shared.stats.to_clients.fetch_add(n as u64, Ordering::Relaxed);
        out.send(datagram, client).await;
    }
}
