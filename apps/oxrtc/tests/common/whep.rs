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

/// What a session showed before `within` ran out.
#[derive(Debug, Default)]
pub struct Played {
    pub status: u16,
    /// From the POST to its answer.
    pub answer: Duration,
    pub connected: bool,
    pub video_frames: usize,
    pub first_keyframe: bool,
    pub codecs: Vec<Codec>,
    /// The stream's `consumers` in `GET /api/streams` once connected.
    pub consumers: Vec<serde_json::Value>,
    /// The answer's `Location`: the session's resource.
    pub location: Option<String>,
    /// The answer's `Accept-Patch`.
    pub accept_patch: Option<String>,
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
    let socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();
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
    rtc.sdp_api().accept_answer(pending, answer).unwrap();

    let deadline = tokio::time::Instant::now() + within;
    let mut buf = vec![0u8; 2048];
    let mut list_consumers = false;
    loop {
        let timeout = loop {
            match rtc.poll_output() {
                Ok(Output::Timeout(t)) => break t,
                Ok(Output::Transmit(t)) => {
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
