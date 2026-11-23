//! One socket of `/api/ws`: its messages read in order on one task, the offer
//! answered beside them (a source may take seconds to open, and the client
//! trickles its candidates meanwhile), an MSE's fragments or an MJPEG's
//! pictures written as they come ([`super::media`]), pings sent and the
//! socket's bounds kept.

use std::net::SocketAddr;
use std::time::Duration;

use axum::extract::ws::{CloseFrame, Message, WebSocket};
use oxrtc_core::{BoxFuture, CancellationToken, Start};
use tokio::time::Instant;

use super::WsLimits;
use super::media::{Media, Sent};
use super::message::{self, Dialect, Incoming, Later, Outgoing};
use crate::ApiState;
use crate::backend::{AnswerError, Answered, ResourceError};

/// RFC 6455's close codes this server sends.
const GOING_AWAY: u16 = 1001;
const UNSUPPORTED: u16 = 1003;
const INVALID: u16 = 1007;
const POLICY: u16 = 1008;
const NORMAL: u16 = 1000;

/// What a socket was opened with.
pub(crate) struct Context {
    pub(crate) state: ApiState,
    /// The upgrade's `src`.
    pub(crate) src: Option<String>,
    pub(crate) remote: Option<SocketAddr>,
    pub(crate) user_agent: Option<String>,
    /// Where an `mse` or `mjpeg` of this socket starts (the upgrade's `start`).
    pub(crate) start: Start,
    pub(crate) limits: WsLimits,
    pub(crate) shutdown: CancellationToken,
    /// The socket's place in the bounds, let go when it ends.
    pub(crate) _slot: crate::slots::Slot,
}

type Answering = BoxFuture<'static, Result<Answered, AnswerError>>;

/// An offer being answered.
struct InFlight {
    answer: Answering,
    dialect: Dialect,
    stream: String,
}

/// Why a socket ends.
enum End {
    /// The client closed it, or it broke: nothing to send.
    Gone,
    /// The server closes it with this code and reason.
    Close(u16, &'static str),
}

struct Socket {
    ws: WebSocket,
    context: Context,
    /// The session the last answer made, which later candidates go to.
    session: Option<String>,
    /// Candidates that came before their session's answer.
    early: Vec<String>,
    /// Sessions asked for so far.
    offers: usize,
    /// The current second's messages.
    window: Instant,
    in_window: u32,
    heard: Instant,
}

/// Serves one socket until it closes, the server stops, or a bound is passed.
pub(crate) async fn run(ws: WebSocket, context: Context) {
    let now = Instant::now();
    let mut socket =
        Socket { ws, context, session: None, early: Vec::new(), offers: 0, window: now, in_window: 0, heard: now };
    let mut in_flight: Option<InFlight> = None;
    // Beside the socket, as the offer in flight: a branch of the loop polls
    // it while others use the socket. Dropped when the socket ends, which
    // ends its subscription at once.
    let mut media: Option<Media> = None;
    let end = socket.serve(&mut in_flight, &mut media).await;
    drop(media);
    if let Some(left) = in_flight {
        // The answer can no longer be delivered: its session is ended as
        // soon as it exists, rather than waiting out its connection timeout.
        abandon(socket.context.state.clone(), left.answer);
    }
    if let End::Close(code, reason) = end {
        let frame = CloseFrame { code, reason: reason.into() };
        let _ = tokio::time::timeout(socket.context.limits.write_timeout, socket.ws.send(Message::Close(Some(frame))))
            .await;
    }
}

fn abandon(state: ApiState, answer: Answering) {
    let Some(answerer) = state.answerer.clone() else { return };
    let Ok(runtime) = tokio::runtime::Handle::try_current() else { return };
    runtime.spawn(async move {
        if let Ok(answered) = answer.await {
            tracing::debug!("api: websocket closed before its answer; its session is ended");
            let _ = answerer.close(answered.resource).await;
        }
    });
}

async fn answering(in_flight: &mut Option<InFlight>) -> Result<Answered, AnswerError> {
    match in_flight {
        Some(flight) => flight.answer.as_mut().await,
        None => std::future::pending().await,
    }
}

async fn streaming(media: &mut Option<Media>) -> Result<Vec<Sent>, String> {
    match media {
        Some(media) => media.next().await,
        None => std::future::pending().await,
    }
}

impl Socket {
    async fn serve(&mut self, in_flight: &mut Option<InFlight>, media: &mut Option<Media>) -> End {
        let limits = self.context.limits;
        let mut ping = tokio::time::interval_at(Instant::now() + limits.ping_every, limits.ping_every);
        loop {
            let idle_at = self.heard + limits.idle_timeout;
            tokio::select! {
                () = self.context.shutdown.cancelled() => return End::Close(GOING_AWAY, "server stopping"),
                received = self.ws.recv() => {
                    let received = match received {
                        Some(Ok(received)) => received,
                        // An oversized or malformed frame; a broken socket
                        // fails this close as well, which costs nothing.
                        Some(Err(_)) => return End::Close(POLICY, "bad frame"),
                        None => return End::Gone,
                    };
                    self.heard = Instant::now();
                    if let Err(end) = self.received(received, in_flight, media).await {
                        return end;
                    }
                }
                step = streaming(media) => {
                    if let Err(end) = self.streamed(step, media).await {
                        return end;
                    }
                }
                answered = answering(in_flight) => {
                    let Some(flight) = in_flight.take() else { continue };
                    if let Err(end) = self.answered(answered, flight).await {
                        return end;
                    }
                }
                _ = ping.tick() => {
                    if let Err(end) = self.send_raw(Message::Ping(Default::default())).await {
                        return end;
                    }
                }
                () = tokio::time::sleep_until(idle_at) => return End::Close(NORMAL, "idle"),
            }
        }
    }

    /// One frame from the client.
    async fn received(
        &mut self,
        received: Message,
        in_flight: &mut Option<InFlight>,
        media: &mut Option<Media>,
    ) -> Result<(), End> {
        let text = match received {
            Message::Text(text) => text,
            Message::Binary(_) => return Err(End::Close(UNSUPPORTED, "messages are JSON text")),
            Message::Close(_) => return Err(End::Gone),
            // tungstenite answers a ping itself; either one says the peer is
            // there, and each is a frame read and a pong written: counted.
            Message::Ping(_) | Message::Pong(_) => return self.count(),
        };
        self.count()?;
        let Ok(incoming) = message::parse(text.as_str()) else {
            return Err(End::Close(INVALID, "not a message"));
        };
        match incoming {
            Incoming::Offer { sdp, dialect } => self.offer(sdp, dialect, in_flight).await,
            Incoming::BadOffer { dialect, reason } => {
                self.send(Outgoing::Error { about: dialect.offer_type(), reason: format!("bad offer: {reason}") }).await
            }
            Incoming::Candidate(Some(line)) => self.candidate(line, in_flight.is_some()).await,
            Incoming::Candidate(None) | Incoming::Ignored => Ok(()),
            Incoming::Mse(codecs) => self.media("mse", |context| Media::mse(context, &codecs), media).await,
            Incoming::Mjpeg => self.media("mjpeg", Media::mjpeg, media).await,
            Incoming::Later(later) => self.send(later_error(later)).await,
        }
    }

    /// Counts a message against the rate bound.
    fn count(&mut self) -> Result<(), End> {
        let now = Instant::now();
        if now.duration_since(self.window) >= Duration::from_secs(1) {
            self.window = now;
            self.in_window = 0;
        }
        self.in_window = self.in_window.saturating_add(1);
        if self.in_window > self.context.limits.max_messages_per_second {
            tracing::debug!("api: websocket sent too many messages, closing");
            return Err(End::Close(POLICY, "too many messages"));
        }
        Ok(())
    }

    async fn offer(&mut self, sdp: String, dialect: Dialect, in_flight: &mut Option<InFlight>) -> Result<(), End> {
        let about = dialect.offer_type();
        let refuse = |reason: &str| Outgoing::Error { about, reason: reason.to_owned() };
        if in_flight.is_some() {
            return self.send(refuse("an offer is already being answered")).await;
        }
        if self.offers >= self.context.limits.max_offers {
            return self.send(refuse("too many offers on one socket")).await;
        }
        let stream = self.context.src.as_deref().and_then(|name| self.context.state.hub.get(name));
        let Some(stream) = stream else { return self.send(refuse("stream not found")).await };
        let Some(answerer) = self.context.state.answerer.clone() else {
            return self.send(refuse("webrtc is not enabled")).await;
        };
        self.offers += 1;
        let name = stream.name().to_owned();
        let answer = answerer.answer(stream, sdp, self.context.remote, self.context.user_agent.clone());
        *in_flight = Some(InFlight { answer, dialect, stream: name });
        Ok(())
    }

    /// `mse` or `mjpeg`: a subscription, answered once it is made. One at a
    /// time, and counted with the offers against the sessions a socket may
    /// ask for in its life.
    async fn media(
        &mut self,
        about: &'static str,
        start: impl FnOnce(&Context) -> Result<Media, &'static str>,
        media: &mut Option<Media>,
    ) -> Result<(), End> {
        let refuse = |reason: &str| Outgoing::Error { about, reason: reason.to_owned() };
        if media.is_some() {
            return self.send(refuse("already streaming on this socket")).await;
        }
        if self.offers >= self.context.limits.max_offers {
            return self.send(refuse("too many sessions on one socket")).await;
        }
        match start(&self.context) {
            Ok(started) => {
                self.offers += 1;
                *media = Some(started);
                Ok(())
            }
            Err(reason) => self.send(refuse(reason)).await,
        }
    }

    /// What the stream gave: its messages sent, or its failure said. A
    /// failure before the player was told what follows is an error it
    /// answers by trying its next mode; one after it (the stream ended or
    /// changed codecs) closes the socket, so that the player starts again.
    async fn streamed(&mut self, step: Result<Vec<Sent>, String>, media: &mut Option<Media>) -> Result<(), End> {
        match step {
            Ok(sent) => {
                for item in sent {
                    match item {
                        Sent::Text(outgoing) => self.send(outgoing).await?,
                        Sent::Binary(bytes) => self.send_raw(Message::Binary(bytes)).await?,
                    }
                }
                Ok(())
            }
            Err(reason) => {
                let (about, started) = media.as_ref().map_or(("mse", false), |m| (m.about(), m.started()));
                *media = None;
                tracing::debug!("api: {about} over websocket ended: {reason}");
                self.send(Outgoing::Error { about, reason }).await?;
                if started { Err(End::Close(NORMAL, "stream ended")) } else { Ok(()) }
            }
        }
    }

    /// The offer's outcome: the answer and the server's candidates, then the
    /// client's candidates that came before it.
    async fn answered(&mut self, answered: Result<Answered, AnswerError>, flight: InFlight) -> Result<(), End> {
        let answered = match answered {
            Ok(answered) => answered,
            Err(error) => {
                self.early.clear();
                let fail = crate::routes::refused_offer(&self.context.state, &flight.stream, &error);
                return self
                    .send(Outgoing::Error { about: flight.dialect.offer_type(), reason: fail.message().to_owned() })
                    .await;
            }
        };
        let candidates = message::answer_candidates(&answered.sdp);
        let resource = answered.resource.clone();
        if let Err(end) = self.send(Outgoing::Answer { sdp: answered.sdp, dialect: flight.dialect }).await {
            if let Some(answerer) = self.context.state.answerer.clone() {
                let _ = answerer.close(resource).await;
            }
            return Err(end);
        }
        tracing::debug!(stream = %flight.stream, "api: webrtc answered over websocket");
        self.session = Some(resource);
        // The answer carries every candidate; go2rtc sends its own as
        // messages, and a client may read only those.
        for line in candidates {
            self.send(Outgoing::Candidate(line)).await?;
        }
        for line in std::mem::take(&mut self.early) {
            self.trickle(line).await?;
        }
        Ok(())
    }

    async fn candidate(&mut self, line: String, answering: bool) -> Result<(), End> {
        if line.len() > self.context.limits.max_candidate_bytes {
            let reason = "the candidate is too long".to_owned();
            return self.send(Outgoing::Error { about: "webrtc/candidate", reason }).await;
        }
        if answering || self.session.is_none() {
            if self.early.len() >= self.context.limits.max_early_candidates {
                let reason = "too many candidates before the answer".to_owned();
                return self.send(Outgoing::Error { about: "webrtc/candidate", reason }).await;
            }
            self.early.push(line);
            return Ok(());
        }
        self.trickle(line).await
    }

    /// Hands a candidate to the session, as a `PATCH` would.
    async fn trickle(&mut self, line: String) -> Result<(), End> {
        let (Some(session), Some(answerer)) = (self.session.clone(), self.context.state.answerer.clone()) else {
            return Ok(());
        };
        match answerer.trickle(session, format!("a={line}\r\n")).await {
            // A session that ended takes nothing more; go2rtc says nothing.
            Ok(()) | Err(ResourceError::NotFound) => Ok(()),
            Err(ResourceError::Bad(reason) | ResourceError::Unsupported(reason)) => {
                self.send(Outgoing::Error { about: "webrtc/candidate", reason }).await
            }
        }
    }

    async fn send(&mut self, outgoing: Outgoing) -> Result<(), End> {
        self.send_raw(Message::Text(outgoing.to_json().into())).await
    }

    async fn send_raw(&mut self, message: Message) -> Result<(), End> {
        match tokio::time::timeout(self.context.limits.write_timeout, self.ws.send(message)).await {
            Ok(Ok(())) => Ok(()),
            Ok(Err(_)) => Err(End::Gone),
            Err(_) => {
                tracing::debug!("api: websocket client not reading, closing");
                Err(End::Gone)
            }
        }
    }
}

/// What a type of a later item is answered with until that item is done:
/// go2rtc's form of a failed message, which its player reads as "try the
/// next mode". F4 and F6 replace this one function.
fn later_error(later: Later) -> Outgoing {
    Outgoing::Error { about: later.name(), reason: "not supported by this server yet".to_owned() }
}
