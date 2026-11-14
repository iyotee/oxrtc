//! The driver against a scripted RTSP server on 127.0.0.1:0: failure mapping,
//! SETUP order, the bounds, and TEARDOWN on drop. Needs no external binary.
// Test code: a failed expectation is a panic by design.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]

mod common;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use bytes::Bytes;
use oxrtc_core::frame::validate_avcc;
use oxrtc_core::{Codec, CodecParams, ConsumerTag, Event, HubError, MediaKind, SourceState, SpecError, Stream, Want};
use oxrtc_rtsp_client::{MAX_RTSP_MESSAGE_BYTES, MAX_STALE_WAIT};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::task::JoinHandle;

/// How the fake camera behaves.
#[derive(Clone, Copy, PartialEq)]
enum Script {
    /// 401 to everything.
    Unauthorized,
    /// Accepts, reads, never answers.
    Silent,
    /// Answers DESCRIBE with a head announcing a body over the limit.
    Oversized,
    /// A full handshake (audio listed before video, plus ONVIF metadata),
    /// then no media at all.
    PlayThenSilence,
    /// The same handshake (no parameter sets in the SDP), then two H.264
    /// access units on interleaved channel 0: SPS + PPS + IDR, then a P slice.
    PlayThenFrames,
    /// The same handshake, then audio packets and video RTCP sender reports
    /// every 50 ms, but never a picture: a camera whose encoder froze.
    PlayThenAudioOnly,
    /// The same handshake, except that the audio SETUP is never answered.
    AudioSetupUnanswered,
    /// `PlayThenSilence`, but a TEARDOWN is never answered, as go2rtc 1.9.15's
    /// RTSP server does while it plays (measured on the bench's camera server:
    /// no answer within 8 s, three attempts out of three).
    IgnoresTeardown,
}

const SPS: &[u8] = &[
    0x67, 0x64, 0x00, 0x1e, 0xac, 0x2c, 0x6a, 0x82, 0xc0, 0xf6, 0x9b, 0x82, 0x80, 0x82, 0xa0, 0x00, 0x00, 0x03, 0x00,
    0x20, 0x00, 0x00, 0x03, 0x03, 0xd0, 0x80,
];
// Retina's own sample parameter sets, without their trailing zero byte (which it strips).
const PPS: &[u8] = &[0x68, 0xee, 0x31, 0xb2, 0x1b];
const IDR: &[u8] = &[0x65, 0x88, 0x84, 0x00, 0x33, 0xff];
const P_SLICE: &[u8] = &[0x41, 0x9a, 0x02, 0x04, 0x60];

/// One RTP packet carrying one NAL unit, framed for RTSP interleaved channel 0.
fn interleaved(seq: u16, timestamp: u32, marker: bool, nal: &[u8]) -> Vec<u8> {
    rtp(0, 96, 0x1234_5678, seq, timestamp, marker, nal)
}

/// Frames `body` for RTSP interleaved `channel`.
fn on_channel(channel: u8, body: &[u8]) -> Vec<u8> {
    let len = u16::try_from(body.len()).unwrap();
    let mut out = vec![b'$', channel];
    out.extend_from_slice(&len.to_be_bytes());
    out.extend_from_slice(body);
    out
}

/// One RTP packet, framed for RTSP interleaved `channel`.
fn rtp(channel: u8, pt: u8, ssrc: u32, seq: u16, timestamp: u32, marker: bool, payload: &[u8]) -> Vec<u8> {
    let mut body = vec![0x80, if marker { 0x80 | pt } else { pt }];
    body.extend_from_slice(&seq.to_be_bytes());
    body.extend_from_slice(&timestamp.to_be_bytes());
    body.extend_from_slice(&ssrc.to_be_bytes());
    body.extend_from_slice(payload);
    on_channel(channel, &body)
}

/// One RTCP sender report (no report blocks), framed for `channel`.
fn sender_report(channel: u8, ssrc: u32, rtp_timestamp: u32) -> Vec<u8> {
    let mut body = vec![0x80, 200, 0x00, 0x06];
    for word in [ssrc, 0xe000_0000, 0, rtp_timestamp, 0, 0] {
        body.extend_from_slice(&word.to_be_bytes());
    }
    on_channel(channel, &body)
}

/// Audio on channel 2 and video sender reports on channel 1, every 50 ms,
/// for 10 s or until the client goes away.
async fn talk_without_pictures(socket: &mut tokio::net::TcpStream) {
    for i in 0..200u16 {
        let mut out = rtp(2, 8, 0x0a0a_0a0a, 500 + i, u32::from(i) * 400, true, &[0xd5; 400]);
        out.extend(sender_report(1, 0x1234_5678, u32::from(i) * 4500));
        if socket.write_all(&out).await.is_err() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

const SDP: &str = "v=0\r\n\
o=- 0 0 IN IP4 127.0.0.1\r\n\
s=fake\r\n\
t=0 0\r\n\
m=audio 0 RTP/AVP 8\r\n\
a=rtpmap:8 PCMA/8000\r\n\
a=control:trackID=0\r\n\
m=application 0 RTP/AVP 107\r\n\
a=rtpmap:107 vnd.onvif.metadata/90000\r\n\
a=control:trackID=2\r\n\
m=video 0 RTP/AVP 96\r\n\
a=rtpmap:96 H264/90000\r\n\
a=fmtp:96 packetization-mode=1\r\n\
a=control:trackID=1\r\n";

struct FakeCamera {
    port: u16,
    requests: Arc<Mutex<Vec<String>>>,
    task: JoinHandle<()>,
}

impl Drop for FakeCamera {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl FakeCamera {
    async fn start(script: Script) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let log = requests.clone();
        let task = tokio::spawn(async move {
            while let Ok((socket, _)) = listener.accept().await {
                tokio::spawn(serve(socket, script, port, log.clone()));
            }
        });
        Self { port, requests, task }
    }

    fn requests(&self) -> Vec<String> {
        self.requests.lock().unwrap().clone()
    }

    fn url(&self, userinfo: &str, fragment: &str) -> String {
        format!("rtsp://{userinfo}127.0.0.1:{}/cam{fragment}", self.port)
    }
}

fn header<'a>(request: &'a str, name: &str) -> Option<&'a str> {
    request.lines().find_map(|l| {
        let (k, v) = l.split_once(':')?;
        k.trim().eq_ignore_ascii_case(name).then(|| v.trim())
    })
}

async fn serve(mut socket: tokio::net::TcpStream, script: Script, port: u16, log: Arc<Mutex<Vec<String>>>) {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        let Some(end) = buf.windows(4).position(|w| w == b"\r\n\r\n") else {
            match socket.read(&mut chunk).await {
                Ok(0) | Err(_) => return,
                Ok(n) => buf.extend_from_slice(&chunk[..n]),
            }
            continue;
        };
        let request = String::from_utf8_lossy(&buf[..end]).into_owned();
        buf.drain(..end + 4);
        log.lock().unwrap().push(request.clone());
        let cseq = header(&request, "CSeq").unwrap_or("0").to_owned();
        let method = request.split(' ').next().unwrap_or_default().to_owned();
        let reply = match (script, method.as_str()) {
            (Script::Silent, _) | (Script::IgnoresTeardown, "TEARDOWN") => continue,
            (Script::Unauthorized, _) => {
                format!("RTSP/1.0 401 Unauthorized\r\nCSeq: {cseq}\r\nWWW-Authenticate: Basic realm=\"cam\"\r\n\r\n")
            }
            (Script::Oversized, _) => format!(
                "RTSP/1.0 200 OK\r\nCSeq: {cseq}\r\nContent-Type: application/sdp\r\nContent-Length: {}\r\n\r\n",
                MAX_RTSP_MESSAGE_BYTES + 1
            ),
            // Every other script plays.
            (_, "DESCRIBE") => format!(
                "RTSP/1.0 200 OK\r\nCSeq: {cseq}\r\nContent-Base: rtsp://127.0.0.1:{port}/cam/\r\n\
                 Content-Type: application/sdp\r\nContent-Length: {}\r\n\r\n{SDP}",
                SDP.len()
            ),
            (Script::AudioSetupUnanswered, "SETUP") if request.contains("/cam/trackID=0 ") => continue,
            (_, "SETUP") => format!(
                "RTSP/1.0 200 OK\r\nCSeq: {cseq}\r\nSession: 12345678;timeout=60\r\nTransport: {}\r\n\r\n",
                header(&request, "Transport").unwrap_or_default()
            ),
            (_, _) => format!("RTSP/1.0 200 OK\r\nCSeq: {cseq}\r\nSession: 12345678\r\n\r\n"),
        };
        if socket.write_all(reply.as_bytes()).await.is_err() {
            return;
        }
        if script == Script::PlayThenAudioOnly && method == "PLAY" {
            talk_without_pictures(&mut socket).await;
            return;
        }
        if script == Script::PlayThenFrames && method == "PLAY" {
            let mut media = Vec::new();
            for (seq, ts, marker, nal) in
                [(100, 1000, false, SPS), (101, 1000, false, PPS), (102, 1000, true, IDR), (103, 4000, true, P_SLICE)]
            {
                media.extend(interleaved(seq, ts, marker, nal));
            }
            if socket.write_all(&media).await.is_err() {
                return;
            }
        }
    }
}

/// Creates demand on the stream's sources, as a viewer would.
fn demand(stream: &Arc<Stream>) -> JoinHandle<()> {
    let stream = stream.clone();
    tokio::spawn(async move {
        let _subscription = stream.subscribe(&Want::all(), ConsumerTag::new("test")).await;
        std::future::pending::<()>().await;
    })
}

#[tokio::test]
async fn refused_credentials_are_auth_and_travel_decoded_outside_the_url() {
    let camera = FakeCamera::start(Script::Unauthorized).await;
    let hub = common::hub(vec![]);
    let stream = hub.upsert("cam", &[camera.url("admin:p%40ss@", "")]).unwrap();
    let viewer = demand(&stream);
    assert_eq!(common::retry_error(&stream, Duration::from_secs(10)).await, "authentication refused");
    viewer.abort();
    let requests = camera.requests();
    assert!(requests.iter().all(|r| !r.contains("p@ss") && !r.contains("admin:")), "{requests:?}");
    // Basic base64("admin:p@ss"): the password reached the camera decoded, in the header only.
    assert!(requests.iter().any(|r| header(r, "Authorization") == Some("Basic YWRtaW46cEBzcw==")), "{requests:?}");
    assert!(requests.iter().all(|r| header(r, "User-Agent").is_some_and(|ua| ua.starts_with("oxrtc/"))));
}

#[tokio::test]
async fn a_closed_port_is_unreachable() {
    let port = common::free_port();
    let hub = common::hub(vec![]);
    let stream = hub.upsert("cam", &[format!("rtsp://127.0.0.1:{port}/cam")]).unwrap();
    let viewer = demand(&stream);
    let error = common::retry_error(&stream, Duration::from_secs(10)).await;
    viewer.abort();
    assert!(error.starts_with("unreachable: Unable to connect"), "{error}");
}

#[tokio::test]
async fn a_silent_camera_times_out_the_handshake_after_its_own_timeout() {
    let camera = FakeCamera::start(Script::Silent).await;
    let hub = common::hub(vec![]);
    let stream = hub.upsert("cam", &[camera.url("", "#timeout=1")]).unwrap();
    let viewer = demand(&stream);
    let error = common::retry_error(&stream, Duration::from_secs(5)).await;
    viewer.abort();
    assert_eq!(error, "unreachable: no answer to DESCRIBE within 1s");
}

#[tokio::test]
async fn an_oversized_answer_is_refused_before_it_is_read() {
    let camera = FakeCamera::start(Script::Oversized).await;
    let hub = common::hub(vec![]);
    let stream = hub.upsert("cam", &[camera.url("", "#timeout=5")]).unwrap();
    let viewer = demand(&stream);
    let error = common::retry_error(&stream, Duration::from_secs(10)).await;
    viewer.abort();
    // Without the bound the driver would wait for the body and time out instead.
    assert!(error.starts_with("protocol error: RTSP framing error"), "{error}");
}

#[tokio::test]
async fn video_is_set_up_first_metadata_never_and_silence_after_play_is_a_stall() {
    let camera = FakeCamera::start(Script::PlayThenSilence).await;
    let hub = common::hub(vec![]);
    let stream = hub.upsert("cam", &[camera.url("", "#timeout=1")]).unwrap();
    let viewer = demand(&stream);
    common::wait_state(&stream, Duration::from_secs(5), |_| !stream.snapshot().sources[0].tracks.is_empty()).await;
    let tracks = stream.snapshot().sources[0].tracks.clone();
    assert_eq!(tracks.len(), 2, "{tracks:?}");
    assert_eq!((tracks[0].kind, tracks[0].codec, tracks[0].clock_rate), (MediaKind::Video, Codec::H264, 90_000));
    assert_eq!((tracks[1].kind, tracks[1].codec, tracks[1].clock_rate), (MediaKind::Audio, Codec::Pcma, 8_000));

    let error = common::retry_error(&stream, Duration::from_secs(5)).await;
    assert_eq!(error, "no media within the stall timeout");
    viewer.abort();

    let setups: Vec<String> = camera.requests().into_iter().filter(|r| r.starts_with("SETUP ")).collect();
    assert!(setups.len() >= 2, "{setups:?}");
    assert!(setups[0].contains("/cam/trackID=1 "), "video must be set up first: {setups:?}");
    assert!(setups[1].contains("/cam/trackID=0 "), "{setups:?}");
    assert!(setups.iter().all(|s| !s.contains("trackID=2")), "metadata was set up: {setups:?}");
    assert!(setups.iter().all(|s| header(s, "Transport").is_some_and(|t| t.contains("RTP/AVP/TCP"))));
}

#[tokio::test]
async fn audio_and_rtcp_without_pictures_is_a_stall() {
    let camera = FakeCamera::start(Script::PlayThenAudioOnly).await;
    let hub = common::hub(vec![]);
    let stream = hub.upsert("cam", &[camera.url("", "#timeout=1")]).unwrap();
    let viewer = demand(&stream);
    // The microphone is heard (the hub's own stall timeout is 20 s, so only
    // the driver's 1 s can end this run)...
    common::wait_state(&stream, Duration::from_secs(5), |_| stream.snapshot().sources[0].frames >= 3).await;
    // ...but the picture never comes.
    let error = common::retry_error(&stream, Duration::from_secs(5)).await;
    viewer.abort();
    assert_eq!(error, "no media within the stall timeout");
}

#[tokio::test]
async fn an_unanswered_audio_setup_fails_the_session_instead_of_playing_on() {
    let camera = FakeCamera::start(Script::AudioSetupUnanswered).await;
    let hub = common::hub(vec![]);
    let stream = hub.upsert("cam", &[camera.url("", "#timeout=1")]).unwrap();
    let viewer = demand(&stream);
    let error = common::retry_error(&stream, Duration::from_secs(5)).await;
    viewer.abort();
    // Going on to PLAY would read the late SETUP answer as PLAY's.
    assert_eq!(error, "unreachable: no answer to SETUP within 1s");
    assert!(!camera.requests().iter().any(|r| r.starts_with("PLAY ")), "{:?}", camera.requests());
}

#[tokio::test]
async fn the_last_viewer_leaving_tears_the_session_down() {
    let camera = FakeCamera::start(Script::PlayThenSilence).await;
    let hub = common::hub(vec![]);
    let stream = hub.upsert("cam", &[camera.url("", "#timeout=30")]).unwrap();
    let viewer = demand(&stream);
    common::wait_state(&stream, Duration::from_secs(5), |_| !stream.snapshot().sources[0].tracks.is_empty()).await;
    viewer.abort();
    common::wait_state(&stream, Duration::from_secs(5), |s| *s == SourceState::Idle).await;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while !camera.requests().iter().any(|r| r.starts_with("TEARDOWN ")) {
        assert!(tokio::time::Instant::now() < deadline, "no TEARDOWN: {:?}", camera.requests());
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// A server that never answers TEARDOWN (go2rtc 1.9.15 while it plays) kept
/// retina's attempt, and the connection, open for 5 s, and the next viewer
/// waited all of it: 1.9 s to its WHEP answer in the bench's L03. The wait for
/// the previous session is bounded by `MAX_STALE_WAIT`, well under that.
#[tokio::test]
async fn an_unanswered_teardown_delays_the_next_viewer_by_the_stale_wait_at_most() {
    let camera = FakeCamera::start(Script::IgnoresTeardown).await;
    let hub = common::hub(vec![]);
    let stream = hub.upsert("cam", &[camera.url("", "#timeout=30")]).unwrap();
    let viewer = demand(&stream);
    common::wait_state(&stream, Duration::from_secs(5), |_| !stream.snapshot().sources[0].tracks.is_empty()).await;
    viewer.abort();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while !camera.requests().iter().any(|r| r.starts_with("TEARDOWN ")) {
        assert!(tokio::time::Instant::now() < deadline, "no TEARDOWN: {:?}", camera.requests());
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let describes = || camera.requests().iter().filter(|r| r.starts_with("DESCRIBE ")).count();
    let started = tokio::time::Instant::now();
    let viewer = demand(&stream);
    while describes() < 2 {
        assert!(started.elapsed() < Duration::from_secs(6), "no second DESCRIBE: {:?}", camera.requests());
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    viewer.abort();
    let waited = started.elapsed();
    // Absolute, not relative to the constant: retina's own attempt lasts 5 s.
    assert!(waited < Duration::from_secs(2), "the next viewer waited {waited:?} (stale wait {MAX_STALE_WAIT:?})");
}

#[tokio::test]
async fn sources_are_checked_when_the_stream_is_created() {
    let port = common::free_port();
    let hub = common::hub(vec![port]);
    let refused = |source: String| match hub.upsert("cam", std::slice::from_ref(&source)) {
        Err(HubError::Spec { error, .. }) => error,
        other => panic!("{source} gave {other:?}"),
    };
    assert_eq!(refused(format!("rtsp://127.0.0.1:{port}/cam")), SpecError::SelfReference);
    for bad in [
        format!("rtsp://127.0.0.1:{port}/not a name"),
        format!("rtsp://127.0.0.1:{port}/other?video=nope"),
        "rtsp:///live".to_owned(),
        "rtsp://cam/x#timeout=0".to_owned(),
    ] {
        assert!(matches!(refused(bad.clone()), SpecError::Invalid(_)), "{bad}");
    }
    assert!(hub.upsert("tls", &["rtsps://cam.example/live"]).is_ok());
}

#[tokio::test]
async fn rtsps_fails_for_good_instead_of_retrying() {
    let hub = common::hub(vec![]);
    let stream = hub.upsert("cam", &[format!("rtsps://127.0.0.1:{}/cam", common::free_port())]).unwrap();
    let viewer = demand(&stream);
    let state = common::wait_state(&stream, Duration::from_secs(5), |s| matches!(s, SourceState::Failed { .. })).await;
    viewer.abort();
    let SourceState::Failed { error } = state else { unreachable!() };
    assert!(error.starts_with("unsupported: rtsps"), "{error}");
}

#[tokio::test]
async fn a_url_on_our_own_port_is_read_in_process() {
    let camera = FakeCamera::start(Script::PlayThenSilence).await;
    // Nothing listens on this port: only the in-process path can work.
    let port = common::free_port();
    let hub = common::hub(vec![port]);
    hub.upsert("cam", &[camera.url("", "#timeout=30")]).unwrap();
    let relay = hub.upsert("relay", &[format!("rtsp://localhost:{port}/cam?audio")]).unwrap();
    let mut sub = tokio::time::timeout(Duration::from_secs(10), relay.subscribe(&Want::all(), ConsumerTag::new("t")))
        .await
        .unwrap()
        .unwrap();
    let Ok(Event::Tracks(tracks)) = sub.recv().await else { panic!("first event is not Tracks") };
    assert_eq!(tracks.iter().map(|t| t.codec).collect::<Vec<_>>(), [Codec::Pcma]);
    assert!(matches!(relay.snapshot().sources[0].state, SourceState::Connecting { .. } | SourceState::Running));
    assert_eq!(camera.requests().iter().filter(|r| r.starts_with("DESCRIBE ")).count(), 1);
}

#[tokio::test]
async fn parameters_found_in_band_publish_a_new_track_list_and_frames_are_avcc() {
    let camera = FakeCamera::start(Script::PlayThenFrames).await;
    let hub = common::hub(vec![]);
    let stream = hub.upsert("cam", &[camera.url("", "#timeout=30")]).unwrap();
    let want = Want::from_query("video").unwrap();
    let mut sub = tokio::time::timeout(Duration::from_secs(10), stream.subscribe(&want, ConsumerTag::new("t")))
        .await
        .unwrap()
        .unwrap();
    let mut tracks = None;
    let mut frames = Vec::new();
    while frames.len() < 2 {
        match tokio::time::timeout(Duration::from_secs(10), sub.recv()).await.unwrap().unwrap() {
            Event::Tracks(t) => tracks = Some(t),
            Event::Frame { frame, .. } => frames.push(frame),
            Event::Discontinuity => panic!("discontinuity"),
        }
    }
    // The SDP announced no parameter sets: these came from the first frame.
    let tracks = tracks.unwrap();
    assert_eq!(tracks[0].params, CodecParams::H264 { sps: Bytes::from_static(SPS), pps: Bytes::from_static(PPS) });

    let (first, second) = (&frames[0], &frames[1]);
    assert!(first.keyframe && !second.keyframe);
    assert_eq!((first.pts, second.pts), (0, 3000));
    let nals: Vec<&[u8]> = first.nal_units().collect();
    assert_eq!(nals, [SPS, PPS, IDR]);
    assert_eq!(second.nal_units().collect::<Vec<_>>(), [P_SLICE]);
    assert_eq!(validate_avcc(&first.data), Ok(3));
}
