//! A str0m client playing the browser over `POST /api/webrtc`: the minimal
//! part of crates/webrtc's test client (copied, not shared: test code of
//! another crate is not a dependency).

use std::net::SocketAddr;
use std::time::{Duration, Instant};

use str0m::change::SdpAnswer;
use str0m::format::Codec;
use str0m::media::{Direction, MediaKind};
use str0m::net::{Protocol, Receive};
use str0m::{Candidate, Event, Input, Output, RtcConfig};
use tokio::net::UdpSocket;

use super::Request;

/// A UDP socket as a browser's: a receive buffer big enough for the burst of
/// a large keyframe (Windows gives 64 KiB by default, which drops the packets
/// of a 1080p picture sent at once and leaves the client with no keyframe).
fn browser_socket() -> UdpSocket {
    let raw = socket2::Socket::new(socket2::Domain::IPV4, socket2::Type::DGRAM, Some(socket2::Protocol::UDP)).unwrap();
    raw.set_recv_buffer_size(4 << 20).unwrap();
    raw.set_nonblocking(true).unwrap();
    raw.bind(&"127.0.0.1:0".parse::<SocketAddr>().unwrap().into()).unwrap();
    UdpSocket::from_std(raw.into()).unwrap()
}

/// What a session showed before `within` ran out.
#[derive(Debug, Default)]
pub struct Played {
    pub status: u16,
    /// From the POST to its answer.
    pub answer: Duration,
    pub connected: bool,
    pub video_frames: usize,
    pub first_keyframe: bool,
    /// How many of the video frames were keyframes.
    pub keyframes: usize,
    /// The position (from 0) of the first keyframe among the video frames.
    pub first_keyframe_at: Option<usize>,
    pub codecs: Vec<Codec>,
    /// The stream's `consumers` in `GET /api/streams` once connected.
    pub consumers: Vec<serde_json::Value>,
    /// The answer's `Location`: the session's resource.
    pub location: Option<String>,
    /// The answer's `Accept-Patch`.
    pub accept_patch: Option<String>,
    /// The datagrams the client sent that begin with a DTLS ClientHello: one,
    /// unless the client sent its hello again.
    pub client_hellos: usize,
    /// The RTP and RTCP datagrams that arrived before the client was connected:
    /// it held no key for them yet, and what they carried is lost to it (its
    /// DTLS ended after oxrtc's, which a retransmitted flight does to this
    /// client: see `play_as`).
    pub before_keys: usize,
}

/// Offers H.264 video (receive only) for stream `src`, and with `audio` an
/// Opus audio transceiver too, as openIVR's live view always does; then
/// drives the session until `frames` video frames arrived (and with `audio`
/// one audio packet) or `within` passed.
pub async fn play(
    api: SocketAddr,
    auth: (&str, &str),
    src: &str,
    audio: bool,
    frames: usize,
    within: Duration,
) -> Played {
    play_as(api, auth, src, None, audio, frames, within).await
}

/// [`play`], the offer sent with `user_agent` as its `User-Agent`, as openIVR
/// names its sessions; the stream's consumers are listed once connected.
pub async fn play_as(
    api: SocketAddr,
    auth: (&str, &str),
    src: &str,
    user_agent: Option<&str>,
    audio: bool,
    frames: usize,
    within: Duration,
) -> Played {
    let socket = browser_socket();
    let local = socket.local_addr().unwrap();
    let mut rtc = RtcConfig::new().clear_codecs().enable_h264(true).enable_opus(audio, false).build(Instant::now());
    rtc.add_local_candidate(Candidate::host(local, "udp").unwrap());
    let mut sdp = rtc.sdp_api();
    sdp.add_media(MediaKind::Video, Direction::RecvOnly, None, None, None);
    if audio {
        sdp.add_media(MediaKind::Audio, Direction::RecvOnly, None, None, None);
    }
    let (offer, pending) = sdp.apply().unwrap();
    let offer = offer.to_sdp_string();
    let target = format!("/api/webrtc?src={src}");
    let posted = Instant::now();
    let mut request = Request::new("POST", &target).auth(auth.0, auth.1).body("application/sdp", offer.as_bytes());
    if let Some(agent) = user_agent {
        request = request.header("User-Agent", agent);
    }
    let reply = request.send(api).await;
    let mut played = Played { status: reply.status, answer: posted.elapsed(), ..Played::default() };
    if !matches!(reply.status, 200 | 201) {
        eprintln!("webrtc answered {}: {}", reply.status, reply.text());
        return played;
    }
    assert_eq!(reply.header("content-type"), Some("application/sdp"));
    played.location = reply.header("location").map(str::to_owned);
    played.accept_patch = reply.header("accept-patch").map(str::to_owned);
    let answer = SdpAnswer::from_sdp_string(&reply.text()).unwrap();
    // The client's clock is brought to the present before the answer starts
    // its DTLS. The `Rtc` was built before the POST, and a cold stream answers
    // seconds later: left at the time it was built, the ClientHello's
    // retransmission timer (1 s from that time) has run out before the first
    // datagram leaves, and the client says hello twice. oxrtc's DTLS then puts
    // its Finished in one datagram behind the flight it sends again, this
    // client's DTLS drops that datagram whole, and the first pictures arrive
    // before the client holds the keys (a browser's clock runs, and it reads
    // every record of a datagram).
    rtc.handle_input(Input::Timeout(Instant::now())).unwrap();
    rtc.sdp_api().accept_answer(pending, answer).unwrap();

    let deadline = tokio::time::Instant::now() + within;
    let mut buf = vec![0u8; 2048];
    let mut list_consumers = false;
    loop {
        let timeout = loop {
            match rtc.poll_output() {
                Ok(Output::Timeout(t)) => break t,
                Ok(Output::Transmit(t)) => {
                    played.client_hellos += usize::from(is_client_hello(&t.contents));
                    let _ = socket.send_to(&t.contents, t.destination).await;
                }
                Ok(Output::Event(Event::Connected)) => {
                    played.connected = true;
                    list_consumers = true;
                }
                Ok(Output::Event(Event::MediaData(data))) => {
                    let codec = data.params.spec().codec;
                    if codec.is_video() {
                        if played.video_frames == 0 {
                            played.first_keyframe = data.is_keyframe();
                        }
                        played.video_frames += 1;
                        if data.is_keyframe() {
                            played.keyframes += 1;
                            played.first_keyframe_at.get_or_insert(played.video_frames - 1);
                        }
                    }
                    if !played.codecs.contains(&codec) {
                        played.codecs.push(codec);
                    }
                }
                Ok(Output::Event(_)) => {}
                Err(error) => panic!("str0m: {error}"),
            }
        };
        if std::mem::take(&mut list_consumers) {
            let listing = Request::new("GET", &format!("/api/streams?src={src}")).auth(auth.0, auth.1).send(api).await;
            if let Some(found) = listing.json().get("consumers").and_then(|c| c.as_array()) {
                played.consumers.clone_from(found);
            }
        }
        let heard = !audio || played.codecs.contains(&Codec::Opus);
        if (played.video_frames >= frames && heard) || !rtc.is_alive() || tokio::time::Instant::now() >= deadline {
            return played;
        }
        let wake = tokio::time::Instant::from_std(timeout.max(Instant::now())).min(deadline);
        tokio::select! {
            received = socket.recv_from(&mut buf) => {
                let Ok((n, source)) = received else { continue };
                // RTP and RTCP begin with the version, 2: 128 to 191 (RFC 7983).
                let media = buf.first().is_some_and(|b| (128..192).contains(b));
                played.before_keys += usize::from(media && !played.connected);
                let Ok(contents) = buf[..n].try_into() else { continue };
                let input = Input::Receive(Instant::now(), Receive { proto: Protocol::Udp, source, destination: local, contents });
                if rtc.handle_input(input).is_err() {
                    return played;
                }
            }
            () = tokio::time::sleep_until(wake) => {
                if rtc.handle_input(Input::Timeout(Instant::now())).is_err() {
                    return played;
                }
            }
        }
    }
}

/// Whether `datagram` begins with a DTLS record of a ClientHello: content
/// type 22 (handshake), and 1 (client_hello) after the record's 13-byte head.
fn is_client_hello(datagram: &[u8]) -> bool {
    datagram.first() == Some(&22) && datagram.get(13) == Some(&1)
}
