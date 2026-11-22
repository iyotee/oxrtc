//! `{"type":"mse","value":"<codecs>"}` on `/api/ws` (M5 F3): the stream as
//! fragmented MP4 over the socket, for a browser's Media Source Extensions
//! (go2rtc's player in `mse` mode, the WebRTC Camera card, a browser without
//! WebRTC).
//!
//! The value lists the codecs the browser's `MediaSource.isTypeSupported` said
//! yes to. It makes the subscription's [`Want`], so that the sources are
//! chosen by the same rule as for a WebRTC offer: the first source of the
//! stream that offers an accepted codec (a camera's own H.265 for a browser
//! that lists `hvc1` or `hev1`, the H.264 transcode after it otherwise). The
//! answer is `{"type":"mse","value":"video/mp4; codecs=\"...\""}` with the
//! codecs the stream really has, then binary frames: an init segment, then
//! one fragment per frame from a keyframe on ([`super::muxer`]).
//!
//! The subscription's own queue is the socket's: a reader slower than the
//! stream falls behind its budget, loses frames, resumes at a keyframe, and
//! the consumer's `dropped` counter and the log say so (`oxrtc_core::queue`);
//! the socket buffers no more than the fragment it is writing.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use oxrtc_core::{
    BoxFuture, Codec, ConsumerTag, Event, MediaWant, Refusals, Stream, SubscribeError, Subscription, Want,
};
use oxrtc_mp4::HevcEntry;

use super::muxer::{Muxer, Out};

/// Most names read from one codec list; the rest is not looked at.
pub const MAX_CODECS: usize = 32;

/// go2rtc's name for an MSE consumer, which `/api/streams` lists.
pub const CONSUMER_KIND: &str = "mse/fmp4";

/// Shortest time between two log lines of one socket's losses: a reader
/// that keeps falling behind loses about once per group of pictures.
pub const LOSS_REPORT_EVERY: Duration = Duration::from_secs(10);

/// What a codec list asks for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Wish {
    /// The subscription's wish.
    pub want: Want,
    /// The H.265 sample entry: `hvc1` when listed (Safari takes no other),
    /// `hev1` when only it is.
    pub hevc: HevcEntry,
}

/// Reads a codec list (RFC 6381 names, comma-separated, as go2rtc's player
/// writes them). A family is taken whatever its profile and level, since the
/// player lists fixed strings (`avc1.640029`) and not the stream's own:
/// `avc1`/`avc3` (H.264), `hvc1`/`hev1` (H.265), `mp4a.40` (AAC), `opus`,
/// `flac` (G.711 A-law and mu-law, and L16, written as FLAC). An empty list
/// asks what go2rtc gives a client that sends none: H.264 or H.265, and
/// AAC. A list with nothing known asks for nothing.
pub fn wish(list: &str) -> Wish {
    let mut listed = Vec::new();
    let (mut hvc1, mut hev1, mut named) = (false, false, false);
    for name in list.split(',').map(str::trim).filter(|n| !n.is_empty()).take(MAX_CODECS) {
        named = true;
        let (family, rest) = name.split_once('.').unwrap_or((name, ""));
        let family = family.to_ascii_lowercase();
        match family.as_str() {
            "avc1" | "avc3" => listed.push(Codec::H264),
            "hvc1" => {
                hvc1 = true;
                listed.push(Codec::H265);
            }
            "hev1" => {
                hev1 = true;
                listed.push(Codec::H265);
            }
            "mp4a" if rest == "40" || rest.starts_with("40.") => listed.push(Codec::Aac),
            "opus" => listed.push(Codec::Opus),
            "flac" => listed.extend([Codec::Pcma, Codec::Pcmu, Codec::L16]),
            _ => {}
        }
    }
    if !named {
        return Wish {
            want: Want {
                video: MediaWant::OneOf(vec![Codec::H264, Codec::H265]),
                audio: MediaWant::OneOf(vec![Codec::Aac]),
            },
            hevc: HevcEntry::Hvc1,
        };
    }
    // The sources are then chosen by the order every egress shares, as for
    // a WebRTC offer.
    Wish { want: Want::preferred(&listed), hevc: if hev1 && !hvc1 { HevcEntry::Hev1 } else { HevcEntry::Hvc1 } }
}

/// What an MSE gives the socket next.
#[derive(Debug)]
pub(crate) enum Step {
    /// To send, in order.
    Send(Vec<Out>),
    /// It cannot go on, for this reason (answered `mse: <reason>`).
    Failed(String),
}

enum State {
    Subscribing(BoxFuture<'static, Result<Subscription, SubscribeError>>),
    Streaming(Box<Subscription>),
}

/// One socket's MSE: its subscription and its muxer. Dropping it ends the
/// subscription at once.
pub(crate) struct Mse {
    state: State,
    muxer: Muxer,
    stream: String,
    remote: String,
    /// The losses, said at most once per [`LOSS_REPORT_EVERY`].
    losses: Refusals,
}

impl Mse {
    /// Subscribes to `stream` with `wish`, for a socket from `remote`.
    pub(crate) fn start(stream: Arc<Stream>, wish: Wish, remote: Option<SocketAddr>, agent: Option<&str>) -> Self {
        let mut tag = ConsumerTag::new(CONSUMER_KIND);
        if let Some(remote) = remote {
            tag = tag.with_remote(remote.to_string());
        }
        if let Some(agent) = agent {
            tag = tag.with_user_agent(agent);
        }
        let want = wish.want.clone();
        let name = stream.name().to_owned();
        let subscribing = Box::pin(async move { stream.subscribe(&want, tag).await });
        Self {
            state: State::Subscribing(subscribing),
            muxer: Muxer::new(wish.want, wish.hevc),
            stream: name,
            remote: remote.map_or_else(|| "-".to_owned(), |r| r.to_string()),
            losses: Refusals::every(LOSS_REPORT_EVERY),
        }
    }

    /// Whether the player was told the MIME type, and so reads fragments.
    pub(crate) fn started(&self) -> bool {
        self.muxer.started()
    }

    /// The next thing to send. Cancel-safe: the subscription is kept across
    /// a cancellation, and an event is handled whole between two awaits.
    pub(crate) async fn next(&mut self) -> Step {
        loop {
            let subscription = match &mut self.state {
                State::Subscribing(subscribing) => match subscribing.as_mut().await {
                    Ok(subscription) => {
                        self.state = State::Streaming(Box::new(subscription));
                        continue;
                    }
                    Err(error) => return Step::Failed(subscribe_error(&error)),
                },
                State::Streaming(subscription) => subscription,
            };
            let event = match subscription.recv().await {
                Ok(event) => event,
                Err(end) => return Step::Failed(format!("streams: {end}")),
            };
            match event {
                Event::Tracks(tracks) => match self.muxer.tracks(&tracks) {
                    Ok(out) if out.is_empty() => {}
                    Ok(out) => return Step::Send(out),
                    Err(error) => return Step::Failed(error.to_string()),
                },
                Event::Frame { track, frame } => {
                    if let Some(bytes) = self.muxer.frame(track, &frame) {
                        return Step::Send(vec![Out::Binary(bytes)]);
                    }
                }
                Event::Discontinuity => {
                    self.muxer.lost();
                    // The subscription counts the frames (`dropped` in the
                    // stream's listing); this says it where an operator
                    // reads, at the default level.
                    if let Some(losses) = self.losses.refused() {
                        tracing::info!(
                            stream = %self.stream,
                            remote = %self.remote,
                            losses,
                            "api: mse reader fell behind the stream: frames dropped, it resumes at the next keyframe"
                        );
                    }
                }
            }
        }
    }
}

/// go2rtc's words where it has them.
fn subscribe_error(error: &SubscribeError) -> String {
    match error {
        SubscribeError::NoMatchingSource | SubscribeError::NothingWanted => "codecs not matched".to_owned(),
        other => format!("streams: {other}"),
    }
}

#[cfg(test)]
#[path = "mse_tests.rs"]
mod tests;
