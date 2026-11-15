//! Clients that reach the server by one path only: through a TURN relay
//! (the answer's other candidates removed, and every datagram from anywhere
//! but the relay dropped), or over ICE-TCP (no UDP at all).

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Instant;

use oxrtc_core::Stream;
use oxrtc_webrtc::{WebRtcServer, WhepError};
use str0m::change::SdpAnswer;
use str0m::media::{Direction, MediaKind as RtcKind};
use str0m::net::{Protocol, Receive, TcpType};
use str0m::{Candidate, Input, Output, Rtc, RtcConfig};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpStream, UdpSocket};
use tokio::sync::{mpsc, oneshot};

use super::{Client, ClientEvent, Control, client_event, drive};

/// The answer's candidate lines of type `kind` (`relay`, `host`, `srflx`), as addresses.
pub fn candidates_of(answer: &str, kind: &str, proto: &str) -> Vec<SocketAddr> {
    answer
        .lines()
        .filter_map(|line| line.trim().strip_prefix("a="))
        .filter_map(|attr| Candidate::from_sdp_string(attr).ok())
        .filter(|c| c.to_sdp_string().contains(&format!(" typ {kind}")) && c.proto().to_string() == proto)
        .map(|c| c.addr())
        .collect()
}

/// Keeps only the answer's candidate lines that `keep` accepts.
fn filter_candidates(answer: &str, keep: impl Fn(&str) -> bool) -> String {
    answer.split("\r\n").filter(|line| !line.starts_with("a=candidate:") || keep(line)).collect::<Vec<_>>().join("\r\n")
}

fn offer(rtc: &mut Rtc) -> (str0m::change::SdpOffer, str0m::change::SdpPendingOffer) {
    let mut api = rtc.sdp_api();
    api.add_media(RtcKind::Video, Direction::RecvOnly, None, None, None);
    api.add_media(RtcKind::Audio, Direction::RecvOnly, None, None, None);
    api.apply().unwrap()
}

/// A client whose only path to the server is the answer's relayed
/// candidate. Returns the client and the relayed address it used.
pub async fn connect_relay_only(
    server: &WebRtcServer,
    stream: Arc<Stream>,
    config: RtcConfig,
) -> Result<(Client, SocketAddr), WhepError> {
    let socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let local = socket.local_addr().unwrap();
    let mut rtc = config.build(Instant::now());
    rtc.add_local_candidate(Candidate::host(local, "udp").unwrap());
    let (offer, pending) = offer(&mut rtc);
    let answer = server.answer(stream, &offer.to_sdp_string(), Some(local)).await?;
    let relayed = *candidates_of(&answer, "relay", "udp").first().expect("the answer offers a relayed candidate");
    let answer = filter_candidates(&answer, |line| line.contains(" typ relay"));
    rtc.sdp_api().accept_answer(pending, SdpAnswer::from_sdp_string(&answer).unwrap()).unwrap();
    let (client, events, control) = Client::new(local);
    tokio::spawn(drive(rtc, socket, events, control, Some(relayed)));
    Ok((client, relayed))
}

/// A client with no UDP: it connects to the answer's ICE-TCP passive
/// candidate and runs ICE over that connection.
pub async fn connect_tcp_only(
    server: &WebRtcServer,
    stream: Arc<Stream>,
    config: RtcConfig,
) -> Result<Client, WhepError> {
    let mut rtc = config.build(Instant::now());
    let (offer, pending) = offer(&mut rtc);
    let answer = server.answer(stream, &offer.to_sdp_string(), None).await?;
    assert!(answer.contains("tcptype passive"), "the answer offers an ICE-TCP passive candidate");
    let target = *candidates_of(&answer, "host", "tcp").first().unwrap();
    let answer = filter_candidates(&answer, |line| line.contains(" tcp "));
    rtc.sdp_api().accept_answer(pending, SdpAnswer::from_sdp_string(&answer).unwrap()).unwrap();
    let tcp = TcpStream::connect(target).await.unwrap();
    let local = tcp.local_addr().unwrap();
    rtc.add_local_candidate(Candidate::builder().tcp().host(local).tcptype(TcpType::Active).build().unwrap());
    let (client, events, control) = Client::new(local);
    tokio::spawn(drive_tcp(rtc, tcp, target, events, control));
    Ok(client)
}

async fn drive_tcp(
    mut rtc: Rtc,
    tcp: TcpStream,
    remote: SocketAddr,
    events: mpsc::Sender<ClientEvent>,
    mut control: oneshot::Receiver<Control>,
) {
    let local = tcp.local_addr().unwrap();
    let (mut reader, mut writer) = tcp.into_split();
    let (frames_tx, mut frames) = mpsc::channel::<Vec<u8>>(1024);
    tokio::spawn(async move {
        loop {
            let mut length = [0u8; 2];
            if reader.read_exact(&mut length).await.is_err() {
                return;
            }
            let mut frame = vec![0u8; u16::from_be_bytes(length) as usize];
            if reader.read_exact(&mut frame).await.is_err() || frames_tx.send(frame).await.is_err() {
                return;
            }
        }
    });
    loop {
        let timeout = loop {
            match rtc.poll_output() {
                Ok(Output::Timeout(t)) => break t,
                Ok(Output::Transmit(t)) => {
                    assert_eq!(t.proto, Protocol::Tcp, "a client without UDP sends on TCP only");
                    let mut framed = (t.contents.len() as u16).to_be_bytes().to_vec();
                    framed.extend_from_slice(&t.contents);
                    let _ = writer.write_all(&framed).await;
                }
                Ok(Output::Event(event)) => {
                    if let Some(e) = client_event(event) {
                        let _ = events.try_send(e);
                    }
                }
                Err(_) => return,
            }
        };
        if !rtc.is_alive() {
            return;
        }
        let wait = tokio::time::Instant::from_std(timeout.max(Instant::now()));
        tokio::select! {
            how = &mut control => {
                if let Ok(Control::Close) = how {
                    let _ = rtc.close();
                    while let Ok(Output::Transmit(t)) = rtc.poll_output() {
                        let mut framed = (t.contents.len() as u16).to_be_bytes().to_vec();
                        framed.extend_from_slice(&t.contents);
                        let _ = writer.write_all(&framed).await;
                    }
                }
                return;
            }
            frame = frames.recv() => {
                let Some(frame) = frame else { return };
                let Ok(contents) = frame.as_slice().try_into() else { continue };
                let input = Input::Receive(Instant::now(), Receive { proto: Protocol::Tcp, source: remote, destination: local, contents });
                if rtc.handle_input(input).is_err() {
                    return;
                }
            }
            () = tokio::time::sleep_until(wait) => {
                if rtc.handle_input(Input::Timeout(Instant::now())).is_err() {
                    return;
                }
            }
        }
    }
}
