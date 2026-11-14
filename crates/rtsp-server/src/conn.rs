//! One RTSP connection: the read loop, request dispatch, and the session it
//! holds (a reader or a publisher, at most one per connection).

use std::fmt::Write as _;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use bytes::BytesMut;
use oxrtc_core::CancellationToken;
use rtsp_types::headers::{PUBLIC, RtpLowerTransport, SESSION, Transport, TransportMode, Transports};
use rtsp_types::{Method, Request, Response, StatusCode};
use tokio::io::AsyncReadExt;
use tokio::net::TcpStream;
use tokio::time::Instant;

use crate::Shared;
use crate::outbox::{self, Outbox, QUEUE_BYTES, QUEUE_ITEMS};
use crate::reader::Reader;
use crate::recorder::Recorder;
use crate::wire::{self, FramingError, Incoming, bare, buffer_limit, next_message, reply};

/// A response.
pub(crate) type Reply = Response<Vec<u8>>;
/// A request.
pub(crate) type Req = Request<Vec<u8>>;

/// How long a response may wait for room in the write queue.
const RESPONSE_WAIT: Duration = Duration::from_secs(5);
/// How long queued bytes may take to drain once the connection ends.
const DRAIN_WAIT: Duration = Duration::from_secs(1);
/// Bytes read from the socket at a time.
const READ_CHUNK: usize = 16 * 1024;

const PUBLIC_METHODS: &str = "OPTIONS, DESCRIBE, SETUP, PLAY, TEARDOWN, GET_PARAMETER, SET_PARAMETER, ANNOUNCE, RECORD";

/// What a connection holds.
pub(crate) enum State {
    /// No session.
    Idle,
    /// A reader, from `DESCRIBE` on.
    Reader(Box<Reader>),
    /// A publisher, from `ANNOUNCE` on.
    Recorder(Box<Recorder>),
}

/// The transport a `SETUP` asked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Interleaved {
    /// RTP and RTCP channels, if the client chose them.
    pub(crate) channels: Option<(u8, u8)>,
    /// `mode=record`.
    pub(crate) record: bool,
}

/// Reads the `Transport` header: the first RTP-over-TCP offer, or `None`
/// (UDP only, or nothing usable), which is answered `461`.
pub(crate) fn interleaved(request: &Req) -> Option<Interleaved> {
    let transports = request.typed_header::<Transports>().ok()??;
    transports.iter().find_map(|t| match t {
        Transport::Rtp(rtp) if rtp.lower_transport == Some(RtpLowerTransport::Tcp) => Some(Interleaved {
            channels: rtp.params.interleaved.map(|(a, b)| (a, b.unwrap_or(a.wrapping_add(1)))),
            record: rtp.params.mode.contains(&TransportMode::Record),
        }),
        _ => None,
    })
}

/// The path of the request URI without its slashes, and its query.
pub(crate) fn target(request: &Req) -> Option<(String, String)> {
    let uri = request.request_uri()?;
    let path = uri.path().trim_matches('/').to_owned();
    Some((path, uri.query().unwrap_or_default().to_owned()))
}

pub(crate) async fn serve(socket: TcpStream, peer: SocketAddr, shared: Arc<Shared>, cancel: CancellationToken) {
    let (mut read, write) = socket.into_split();
    let (outbox, inbox) = outbox::outbox(QUEUE_ITEMS, QUEUE_BYTES);
    let writer_done = CancellationToken::new();
    let writer = tokio::spawn(outbox::write_loop(inbox, write, shared.config.session_timeout, writer_done.clone()));
    let mut conn = Conn { session_id: session_id(), shared, peer, outbox, state: State::Idle, buf: BytesMut::new() };
    let reason = conn.run(&mut read, &cancel, &writer_done).await;
    tracing::debug!(%peer, reason, "rtsp: connection closed");
    drop(conn);
    // Let the queued responses go out, but not forever.
    let abort = writer.abort_handle();
    if tokio::time::timeout(DRAIN_WAIT, writer).await.is_err() {
        abort.abort();
    }
}

fn session_id() -> String {
    let mut id = String::with_capacity(16);
    let _ = write!(id, "{:016x}", fastrand::u64(..));
    id
}

struct Conn {
    shared: Arc<Shared>,
    peer: SocketAddr,
    outbox: Outbox,
    session_id: String,
    state: State,
    buf: BytesMut,
}

async fn sleep_until(deadline: Option<Instant>) {
    match deadline {
        Some(at) => tokio::time::sleep_until(at).await,
        None => std::future::pending().await,
    }
}

async fn ended(token: Option<CancellationToken>) {
    match token {
        Some(token) => token.cancelled_owned().await,
        None => std::future::pending().await,
    }
}

impl Conn {
    async fn run(
        &mut self,
        read: &mut tokio::net::tcp::OwnedReadHalf,
        cancel: &CancellationToken,
        writer_done: &CancellationToken,
    ) -> &'static str {
        let max_request = self.shared.config.max_request_bytes;
        let limit = buffer_limit(max_request);
        let mut chunk = vec![0u8; READ_CHUNK];
        let mut last_activity = Instant::now();
        loop {
            loop {
                match next_message(&mut self.buf, max_request) {
                    Ok(Some(message)) => {
                        if let Err(reason) = self.handle(message, cancel).await {
                            return reason;
                        }
                    }
                    Ok(None) => break,
                    Err(error) => {
                        let (status, reason) = match error {
                            FramingError::TooLarge => (StatusCode::RequestMessageBodyTooLarge, "request too large"),
                            FramingError::Malformed => (StatusCode::BadRequest, "malformed request"),
                        };
                        let _ = self.outbox.send(bare(status), RESPONSE_WAIT).await;
                        return reason;
                    }
                }
            }
            // checked: a configured `Duration::MAX` means "never", not a panic.
            let idle = last_activity.checked_add(self.shared.config.session_timeout);
            let (deadline, session_end) = match &self.state {
                State::Idle => (None, None),
                State::Reader(reader) => (reader.deadline(), None),
                State::Recorder(recorder) => (recorder.deadline(), Some(recorder.session_token())),
            };
            let room = limit.saturating_sub(self.buf.len()).min(READ_CHUNK);
            let window = chunk.get_mut(..room.max(1)).unwrap_or_default();
            tokio::select! {
                biased;
                () = cancel.cancelled() => return "server shutdown",
                () = writer_done.cancelled() => return "write failed",
                () = ended(session_end) => return "publication ended",
                () = sleep_until(idle) => return "session timeout",
                () = sleep_until(deadline) => return "no PLAY or RECORD in time",
                read = read.read(window), if room > 0 => match read {
                    Ok(0) | Err(_) => return "peer closed",
                    Ok(n) => {
                        self.buf.extend_from_slice(window.get(..n).unwrap_or_default());
                        last_activity = Instant::now();
                    }
                },
                event = next_event(&mut self.state) => {
                    if let State::Reader(reader) = &mut self.state
                        && let Err(reason) = reader.on_event(event, &self.outbox)
                    {
                        return reason;
                    }
                }
            }
        }
    }

    async fn handle(&mut self, message: Incoming, cancel: &CancellationToken) -> Result<(), &'static str> {
        match message {
            Incoming::Request(request) => {
                // A DESCRIBE may wait for a source's tracks: never past shutdown.
                let response = tokio::select! {
                    biased;
                    () = cancel.cancelled() => return Err("server shutdown"),
                    response = self.dispatch(&request) => response,
                };
                let close = response.status() == StatusCode::Forbidden;
                // Waiting for room behind a stuck reader must not hold the
                // connection (and its subscription) past shutdown either.
                let sent = tokio::select! {
                    biased;
                    () = cancel.cancelled() => return Err("server shutdown"),
                    sent = self.outbox.send(wire::encode(&response), RESPONSE_WAIT) => sent,
                };
                if !sent {
                    return Err("write queue stuck");
                }
                if close { Err("forbidden") } else { Ok(()) }
            }
            Incoming::Data { channel, payload } => {
                if let State::Recorder(recorder) = &mut self.state {
                    recorder.on_data(channel, &payload);
                }
                Ok(())
            }
            Incoming::Ignored => Ok(()),
        }
    }

    fn session_header(&self) -> String {
        format!("{};timeout={}", self.session_id, self.shared.config.session_timeout.as_secs())
    }

    /// Whether the request names another session than ours.
    fn foreign_session(&self, request: &Req) -> bool {
        let Some(value) = request.header(&SESSION) else { return false };
        let id = value.as_str().split(';').next().unwrap_or_default().trim();
        !matches!(self.state, State::Idle) && id != self.session_id
    }

    async fn dispatch(&mut self, request: &Req) -> Reply {
        if self.foreign_session(request) {
            return reply(request, StatusCode::SessionNotFound).build(Vec::new());
        }
        let session = self.session_header();
        match request.method() {
            Method::Options => reply(request, StatusCode::Ok).header(PUBLIC, PUBLIC_METHODS).build(Vec::new()),
            Method::GetParameter | Method::SetParameter => {
                reply(request, StatusCode::Ok).header(SESSION, session).build(Vec::new())
            }
            Method::Describe => {
                // One upstream per connection: a new DESCRIBE replaces the old.
                self.state = State::Idle;
                match Reader::describe(&self.shared, self.peer, request).await {
                    Ok((reader, response)) => {
                        self.state = State::Reader(Box::new(reader));
                        response
                    }
                    Err(response) => response,
                }
            }
            Method::Announce => {
                self.state = State::Idle;
                match Recorder::announce(&self.shared, self.peer, request) {
                    Ok((recorder, response)) => {
                        self.state = State::Recorder(Box::new(recorder));
                        response
                    }
                    Err(response) => response,
                }
            }
            Method::Setup => match &mut self.state {
                State::Reader(reader) => reader.setup(request, &session),
                State::Recorder(recorder) => recorder.setup(request, &session),
                State::Idle => reply(request, StatusCode::MethodNotValidInThisState).build(Vec::new()),
            },
            Method::Play => match &mut self.state {
                State::Reader(reader) => reader.play(request, &session),
                _ => reply(request, StatusCode::MethodNotValidInThisState).build(Vec::new()),
            },
            Method::Record => match &mut self.state {
                State::Recorder(recorder) => recorder.record(request, &session),
                _ => reply(request, StatusCode::MethodNotValidInThisState).build(Vec::new()),
            },
            Method::Teardown => {
                self.state = State::Idle;
                reply(request, StatusCode::Ok).build(Vec::new())
            }
            _ => reply(request, StatusCode::NotImplemented).build(Vec::new()),
        }
    }
}

/// The next event of a playing reader; pending otherwise.
async fn next_event(state: &mut State) -> Result<oxrtc_core::Event, oxrtc_core::SubEnd> {
    match state {
        State::Reader(reader) if reader.playing() => reader.recv().await,
        _ => std::future::pending().await,
    }
}
