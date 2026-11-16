//! The str0m client playing the browser, over UDP.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use oxrtc_core::Stream;
use oxrtc_core::frame::{AnnexBUnits, h264_nal_type};
use oxrtc_webrtc::{WebRtcServer, WhepError};
use str0m::change::SdpAnswer;
use str0m::format::Codec as RtcCodec;
use str0m::media::{Direction, KeyframeRequestKind, MediaKind as RtcKind, Mid};
use str0m::net::{Protocol, Receive};
use str0m::{Candidate, Event, Input, Output, Rtc, RtcConfig};
use tokio::net::UdpSocket;
use tokio::sync::{mpsc, oneshot};

#[derive(Debug, Clone, PartialEq)]
pub enum ClientEvent {
    Connected,
    /// `params`: the frame carries its parameter sets (SPS for H.264, VPS for H.265);
    /// `time`: its RTP time, in its clock; `at`: when its first packet arrived.
    Media {
        kind: RtcKind,
        codec: RtcCodec,
        keyframe: bool,
        len: usize,
        params: bool,
        time: u64,
        at: Instant,
    },
    Closed,
}

pub enum Control {
    /// Close properly (DTLS close_notify, RTCP BYE), then stop.
    Close,
    /// Stop answering, as a browser tab that crashed.
    Vanish,
}

/// What a test asks of a running client, besides stopping it.
pub enum Action {
    /// Ask for a keyframe of the picture (RTCP PLI), as a browser whose
    /// decoder lost its state.
    RequestKeyframe,
    /// Drop every datagram received for this long: a loss on the way.
    Blackout(Duration),
}

pub struct Client {
    pub events: mpsc::Receiver<ClientEvent>,
    control: Option<oneshot::Sender<Control>>,
    actions: Option<mpsc::Sender<Action>>,
    pub local: SocketAddr,
}

impl Client {
    /// Hands `action` to the client's driver (only one from [`connect`]).
    pub async fn act(&self, action: Action) {
        self.actions.as_ref().expect("a client driven by connect").send(action).await.unwrap();
    }

    pub fn stop(&mut self, how: Control) {
        if let Some(tx) = self.control.take() {
            let _ = tx.send(how);
        }
    }

    /// Waits for an event matching `f`.
    pub async fn wait_for(&mut self, within: Duration, mut f: impl FnMut(&ClientEvent) -> bool) -> Option<ClientEvent> {
        let deadline = tokio::time::Instant::now() + within;
        loop {
            match tokio::time::timeout_at(deadline, self.events.recv()).await {
                Ok(Some(event)) if f(&event) => return Some(event),
                Ok(Some(_)) => continue,
                _ => return None,
            }
        }
    }
}

/// The client codec setup: H.264 (+H.265) video, Opus audio.
pub fn browser(h265: bool) -> RtcConfig {
    RtcConfig::new().clear_codecs().enable_h264(true).enable_h265(h265).enable_opus(true, false)
}

/// The `User-Agent` [`connect`] and its variants offer with, as openIVR
/// names a session (`openIVR-whep/<session id>`).
pub const USER_AGENT: &str = "openIVR-whep/test-session";

pub async fn connect(server: &WebRtcServer, stream: Arc<Stream>, config: RtcConfig) -> Result<Client, WhepError> {
    let socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    connect_on(server, stream, config, socket).await
}

/// [`connect`] from `socket`, whose address is also the one the WHEP caller
/// names (as the API passes its HTTP peer).
pub async fn connect_on(
    server: &WebRtcServer,
    stream: Arc<Stream>,
    config: RtcConfig,
    socket: UdpSocket,
) -> Result<Client, WhepError> {
    connect_late(server, stream, config, socket, Duration::ZERO).await
}

/// [`connect_on`], the client starting its connectivity checks only `late`
/// after the answer: a session that connects slowly (a relay, a phone).
pub async fn connect_late(
    server: &WebRtcServer,
    stream: Arc<Stream>,
    config: RtcConfig,
    socket: UdpSocket,
    late: Duration,
) -> Result<Client, WhepError> {
    let local = socket.local_addr().unwrap();
    let mut rtc = config.build(Instant::now());
    rtc.add_local_candidate(Candidate::host(local, "udp").unwrap());
    let mut api = rtc.sdp_api();
    api.add_media(RtcKind::Video, Direction::RecvOnly, None, None, None);
    api.add_media(RtcKind::Audio, Direction::RecvOnly, None, None, None);
    let (offer, pending) = api.apply().unwrap();
    let answer = server.answer_as(stream, &offer.to_sdp_string(), Some(local), Some(USER_AGENT)).await?;
    assert!(answer.contains("a=candidate"), "the answer carries its candidates");
    rtc.sdp_api().accept_answer(pending, SdpAnswer::from_sdp_string(&answer).unwrap()).unwrap();
    let (events_tx, events) = mpsc::channel(4096);
    let (control_tx, control_rx) = oneshot::channel();
    let (actions_tx, actions_rx) = mpsc::channel(16);
    tokio::spawn(async move {
        tokio::time::sleep(late).await;
        drive_with(rtc, socket, events_tx, control_rx, None, Some(actions_rx)).await;
    });
    Ok(Client { events, control: Some(control_tx), actions: Some(actions_tx), local })
}

impl Client {
    /// A client driven elsewhere (`paths.rs`).
    pub fn new(local: SocketAddr) -> (Self, mpsc::Sender<ClientEvent>, oneshot::Receiver<Control>) {
        let (events_tx, events) = mpsc::channel(4096);
        let (control_tx, control_rx) = oneshot::channel();
        (Client { events, control: Some(control_tx), actions: None, local }, events_tx, control_rx)
    }
}

/// What the str0m client reports of an `Rtc` event.
pub fn client_event(event: Event) -> Option<ClientEvent> {
    match event {
        Event::Connected => Some(ClientEvent::Connected),
        Event::MediaData(data) => Some(ClientEvent::Media {
            kind: if data.params.spec().codec.is_video() { RtcKind::Video } else { RtcKind::Audio },
            codec: data.params.spec().codec,
            keyframe: data.is_keyframe(),
            len: data.data.len(),
            params: carries_parameter_sets(data.params.spec().codec, &data.data),
            time: data.time.numer(),
            at: data.network_time,
        }),
        Event::Closed => Some(ClientEvent::Closed),
        _ => None,
    }
}

/// Drives a client on `socket`; with `only_from`, every datagram from
/// another address is dropped (a network where only that path works).
pub async fn drive(
    rtc: Rtc,
    socket: UdpSocket,
    events: mpsc::Sender<ClientEvent>,
    control: oneshot::Receiver<Control>,
    only_from: Option<SocketAddr>,
) {
    drive_with(rtc, socket, events, control, only_from, None).await;
}

/// [`drive`], also taking [`Action`]s.
pub async fn drive_with(
    mut rtc: Rtc,
    socket: UdpSocket,
    events: mpsc::Sender<ClientEvent>,
    mut control: oneshot::Receiver<Control>,
    only_from: Option<SocketAddr>,
    mut actions: Option<mpsc::Receiver<Action>>,
) {
    let local = socket.local_addr().unwrap();
    let mut buf = vec![0u8; 2048];
    let mut video: Option<Mid> = None;
    let mut blackout_until: Option<Instant> = None;
    loop {
        let timeout = loop {
            match rtc.poll_output() {
                Ok(Output::Timeout(t)) => break t,
                Ok(Output::Transmit(t)) => {
                    let _ = socket.send_to(&t.contents, t.destination).await;
                }
                Ok(Output::Event(event)) => {
                    if let Event::MediaData(data) = &event
                        && data.params.spec().codec.is_video()
                    {
                        video = Some(data.mid);
                    }
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
                match how {
                    Ok(Control::Close) => {
                        let _ = rtc.close();
                        for _ in 0..64 {
                            match rtc.poll_output() {
                                Ok(Output::Transmit(t)) => { let _ = socket.send_to(&t.contents, t.destination).await; }
                                Ok(Output::Event(_)) => {}
                                _ => break,
                            }
                        }
                    }
                    Ok(Control::Vanish) | Err(_) => {}
                }
                return;
            }
            Some(action) = next_action(&mut actions) => match action {
                Action::RequestKeyframe => {
                    if let Some(mut writer) = video.and_then(|mid| rtc.writer(mid)) {
                        writer.request_keyframe(None, KeyframeRequestKind::Pli).unwrap();
                    }
                }
                Action::Blackout(period) => blackout_until = Some(Instant::now() + period),
            },
            received = socket.recv_from(&mut buf) => {
                let Ok((n, source)) = received else { continue };
                if only_from.is_some_and(|only| only != source) {
                    continue;
                }
                if blackout_until.is_some_and(|until| Instant::now() < until) {
                    continue;
                }
                let Ok(contents) = buf[..n].try_into() else { continue };
                let input = Input::Receive(Instant::now(), Receive { proto: Protocol::Udp, source, destination: local, contents });
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

async fn next_action(actions: &mut Option<mpsc::Receiver<Action>>) -> Option<Action> {
    match actions {
        Some(actions) => actions.recv().await,
        None => std::future::pending().await,
    }
}

pub fn carries_parameter_sets(codec: RtcCodec, annex_b: &[u8]) -> bool {
    AnnexBUnits::new(annex_b).any(|nal| match codec {
        RtcCodec::H264 => h264_nal_type(nal) == Some(7),
        RtcCodec::H265 => oxrtc_core::frame::h265_nal_type(nal) == Some(32),
        _ => false,
    })
}
