//! The UDP sockets: binding them, the shared socket's receive loop, and the
//! route each of a session's datagrams leaves by (the shared socket, a TURN
//! relay, an ICE-TCP connection).

use std::io;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use str0m::net::{Protocol, Transmit};
use tokio::net::UdpSocket;
use tracing::{debug, warn};

use crate::registry::Via;
use crate::server::Shared;
use crate::stun::looks_like_stun;

/// Largest datagram read: the largest UDP payload. WebRTC stays under the
/// path MTU, but a smaller buffer turns an oversized datagram into a receive
/// *error* on Windows (`WSAEMSGSIZE`), which a stranger could repeat at will.
pub(crate) const RECV_BUFFER: usize = 65_536;
/// Kernel receive buffer asked for each socket. Every session's STUN, DTLS
/// and RTCP arrive on the shared socket; at the Windows default (64 KiB,
/// seven 9000-byte datagrams) a stranger's flood filled it and the kernel
/// dropped a real client's binding requests: 3 connections in 10 failed in
/// the test `oversized_junk_datagrams_do_not_stall_the_socket`, 10 in 10
/// succeeded in about 105 ms with 4 MiB. Linux clamps it to `rmem_max`.
const SOCKET_RECV_BUFFER: usize = 4 << 20;
/// Pause after a socket error that is not about one datagram.
pub(crate) const ERROR_PAUSE: Duration = Duration::from_millis(10);

/// Binds a UDP socket with a receive buffer of [`SOCKET_RECV_BUFFER`]. A
/// system that refuses the size keeps its default: a warning, not a failure.
pub(crate) fn bind_udp(listen: SocketAddr) -> io::Result<UdpSocket> {
    let raw =
        socket2::Socket::new(socket2::Domain::for_address(listen), socket2::Type::DGRAM, Some(socket2::Protocol::UDP))?;
    if let Err(error) = raw.set_recv_buffer_size(SOCKET_RECV_BUFFER) {
        warn!(%error, "a webrtc socket keeps the system's receive buffer");
    }
    debug!(bytes = raw.recv_buffer_size().unwrap_or(0), "webrtc receive buffer");
    raw.bind(&listen.into())?;
    raw.set_nonblocking(true)?;
    UdpSocket::from_std(raw.into())
}

/// Reads the shared socket until the server stops: a STUN server's answer
/// goes to its probe, everything else to the sessions.
pub(crate) async fn receive_loop(shared: Arc<Shared>) {
    let mut buf = vec![0u8; RECV_BUFFER];
    loop {
        let received = tokio::select! {
            biased;
            () = shared.cancel.cancelled() => break,
            received = shared.socket.recv_from(&mut buf) => received,
        };
        match received {
            Ok((len, source)) => {
                let Some(data) = buf.get(..len) else { continue };
                if looks_like_stun(data) && shared.ice.probe_answer(source, data) {
                    continue;
                }
                shared.dispatch(data, source, Via::Udp);
            }
            // About one datagram (an ICMP error of an earlier send, a
            // truncated datagram): the next one is read at once, or a
            // stranger could stall every session by repeating it.
            Err(error) if per_datagram(&error) => {}
            Err(error) => {
                debug!(%error, "udp receive failed");
                tokio::time::sleep(ERROR_PAUSE).await;
            }
        }
    }
    debug!("webrtc receive loop stopped");
}

/// Whether a receive error concerns a single datagram rather than the socket.
pub(crate) fn per_datagram(error: &io::Error) -> bool {
    // WSAEMSGSIZE and WSAENETRESET (TTL expired) on Windows.
    const WINDOWS_PER_DATAGRAM: [i32; 2] = [10040, 10052];
    matches!(error.kind(), io::ErrorKind::ConnectionReset | io::ErrorKind::ConnectionRefused)
        || (cfg!(windows) && error.raw_os_error().is_some_and(|code| WINDOWS_PER_DATAGRAM.contains(&code)))
}

/// Sends a session's datagrams, each by the route its local candidate
/// implies: a TCP candidate's connection, a host candidate's shared socket
/// (a server-reflexive one sends from its host base), a relayed candidate's
/// TURN server. A datagram from a relayed address whose allocation is gone
/// is dropped: sent from the shared socket, it would reach the peer from an
/// address it never checked.
pub(crate) async fn send(shared: &Shared, host: &[SocketAddr], transmits: &mut Vec<Transmit>) {
    for transmit in transmits.drain(..) {
        if transmit.proto != Protocol::Udp {
            if let Some(tcp) = &shared.tcp {
                tcp.send(transmit.destination, &transmit.contents);
            }
            continue;
        }
        if transmit.source == shared.local || host.contains(&transmit.source) {
            if let Err(error) = shared.socket.send_to(&transmit.contents, transmit.destination).await {
                debug!(%error, destination = %transmit.destination, "udp send failed");
            }
            continue;
        }
        match shared.ice.relay_for(transmit.source).and_then(|r| r.send(transmit.destination, &transmit.contents)) {
            Some(outgoing) => outgoing.send().await,
            None => debug!(source = %transmit.source, "no route for a datagram; dropped"),
        }
    }
}
