//! A scripted RTSP camera on 127.0.0.1:0, in plain TCP or behind TLS, for the
//! integration tests of the driver.
#![allow(dead_code, missing_docs, clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use oxrtc_core::{ConsumerTag, Stream, Want};
use oxrtc_rtsp_client::MAX_RTSP_MESSAGE_BYTES;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::task::JoinHandle;
use tokio_rustls::TlsAcceptor;

/// How the fake camera behaves.
#[derive(Clone, Copy, PartialEq)]
pub enum Script {
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
    /// The same handshake, then an access unit that never ends (sixty FU-A
    /// fragments of 1,000 bytes at one timestamp, no end bit, no marker),
    /// then, at the next timestamp, SPS + PPS + IDR and a P slice.
    PlayThenEndlessAccessUnit,
    /// `PlayThenSilence`, but a TEARDOWN is never answered, as go2rtc 1.9.15's
    /// RTSP server does while it plays (measured on the bench's camera server:
    /// no answer within 8 s, three attempts out of three).
    IgnoresTeardown,
    /// `PlayThenSilence`, but each TEARDOWN is answered `LATE_TEARDOWN`
    /// after it arrives: a camera that answers, slowly.
    AnswersTeardownLate,
}

/// How long `Script::AnswersTeardownLate` takes to answer a TEARDOWN.
pub const LATE_TEARDOWN: Duration = Duration::from_millis(60);

pub const SPS: &[u8] = &[
    0x67, 0x64, 0x00, 0x1e, 0xac, 0x2c, 0x6a, 0x82, 0xc0, 0xf6, 0x9b, 0x82, 0x80, 0x82, 0xa0, 0x00, 0x00, 0x03, 0x00,
    0x20, 0x00, 0x00, 0x03, 0x03, 0xd0, 0x80,
];
// Retina's own sample parameter sets, without their trailing zero byte (which it strips).
pub const PPS: &[u8] = &[0x68, 0xee, 0x31, 0xb2, 0x1b];
pub const IDR: &[u8] = &[0x65, 0x88, 0x84, 0x00, 0x33, 0xff];
pub const P_SLICE: &[u8] = &[0x41, 0x9a, 0x02, 0x04, 0x60];

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
async fn talk_without_pictures<S: AsyncWrite + Unpin>(socket: &mut S) {
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

pub struct FakeCamera {
    pub port: u16,
    requests: Arc<Mutex<Vec<String>>>,
    /// DESCRIBEs still to refuse with a 503, whatever the script.
    refusals: Arc<AtomicU32>,
    task: JoinHandle<()>,
}

impl Drop for FakeCamera {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl FakeCamera {
    pub async fn start(script: Script) -> Self {
        Self::start_with(script, None).await
    }

    /// The camera behind TLS: every connection first runs the handshake with `acceptor`.
    pub async fn start_tls(script: Script, acceptor: TlsAcceptor) -> Self {
        Self::start_with(script, Some(acceptor)).await
    }

    async fn start_with(script: Script, acceptor: Option<TlsAcceptor>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let log = requests.clone();
        let refusals = Arc::new(AtomicU32::new(0));
        let refuse = refusals.clone();
        let scheme = if acceptor.is_some() { "rtsps" } else { "rtsp" };
        let task = tokio::spawn(async move {
            while let Ok((socket, _)) = listener.accept().await {
                let (log, refuse) = (log.clone(), refuse.clone());
                match acceptor.clone() {
                    None => {
                        tokio::spawn(serve(socket, script, scheme, port, log, refuse));
                    }
                    Some(acceptor) => {
                        tokio::spawn(async move {
                            // A client that refuses the certificate ends the handshake: no request follows.
                            if let Ok(secured) = acceptor.accept(socket).await {
                                serve(secured, script, scheme, port, log, refuse).await;
                            }
                        });
                    }
                }
            }
        });
        Self { port, requests, refusals, task }
    }

    /// The next `count` DESCRIBEs are answered 503 (a camera that refuses a
    /// session while it is busy), then the script goes on.
    pub fn refuse_next_describes(&self, count: u32) {
        self.refusals.store(count, Ordering::SeqCst);
    }

    pub fn requests(&self) -> Vec<String> {
        self.requests.lock().unwrap().clone()
    }

    pub fn url(&self, userinfo: &str, fragment: &str) -> String {
        format!("rtsp://{userinfo}127.0.0.1:{}/cam{fragment}", self.port)
    }

    /// The same camera as an `rtsps://` URL.
    pub fn tls_url(&self, userinfo: &str, fragment: &str) -> String {
        format!("rtsps://{userinfo}127.0.0.1:{}/cam{fragment}", self.port)
    }
}

pub fn header<'a>(request: &'a str, name: &str) -> Option<&'a str> {
    request.lines().find_map(|l| {
        let (k, v) = l.split_once(':')?;
        k.trim().eq_ignore_ascii_case(name).then(|| v.trim())
    })
}

async fn serve<S: AsyncRead + AsyncWrite + Unpin>(
    mut socket: S,
    script: Script,
    scheme: &str,
    port: u16,
    log: Arc<Mutex<Vec<String>>>,
    refusals: Arc<AtomicU32>,
) {
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
        let refused = method == "DESCRIBE"
            && refusals.try_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1)).is_ok();
        let reply = match (script, method.as_str()) {
            _ if refused => format!("RTSP/1.0 503 Service Unavailable\r\nCSeq: {cseq}\r\n\r\n"),
            (Script::Silent, _) | (Script::IgnoresTeardown, "TEARDOWN") => continue,
            (Script::AnswersTeardownLate, "TEARDOWN") => {
                tokio::time::sleep(LATE_TEARDOWN).await;
                format!("RTSP/1.0 200 OK\r\nCSeq: {cseq}\r\nSession: 12345678\r\n\r\n")
            }
            (Script::Unauthorized, _) => {
                format!("RTSP/1.0 401 Unauthorized\r\nCSeq: {cseq}\r\nWWW-Authenticate: Basic realm=\"cam\"\r\n\r\n")
            }
            (Script::Oversized, _) => format!(
                "RTSP/1.0 200 OK\r\nCSeq: {cseq}\r\nContent-Type: application/sdp\r\nContent-Length: {}\r\n\r\n",
                MAX_RTSP_MESSAGE_BYTES + 1
            ),
            // Every other script plays.
            (_, "DESCRIBE") => format!(
                "RTSP/1.0 200 OK\r\nCSeq: {cseq}\r\nContent-Base: {scheme}://127.0.0.1:{port}/cam/\r\n\
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
        if script == Script::PlayThenEndlessAccessUnit && method == "PLAY" {
            let mut media = Vec::new();
            for seq in 0..60u16 {
                // FU-A of an IDR slice: the start bit on the first fragment only.
                let mut fragment = vec![0x7c, if seq == 0 { 0x85 } else { 0x05 }];
                fragment.resize(1002, 0x11);
                media.extend(interleaved(100 + seq, 1000, false, &fragment));
            }
            for (seq, ts, marker, nal) in
                [(160, 4000, false, SPS), (161, 4000, false, PPS), (162, 4000, true, IDR), (163, 7000, true, P_SLICE)]
            {
                media.extend(interleaved(seq, ts, marker, nal));
            }
            if socket.write_all(&media).await.is_err() {
                return;
            }
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
pub fn demand(stream: &Arc<Stream>) -> JoinHandle<()> {
    let stream = stream.clone();
    tokio::spawn(async move {
        let _subscription = stream.subscribe(&Want::all(), ConsumerTag::new("test")).await;
        std::future::pending::<()>().await;
    })
}
