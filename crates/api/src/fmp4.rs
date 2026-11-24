//! A stream as fragmented MP4, for every egress that writes it: MSE over
//! `/api/ws` (F3), its `mp4` keyframes (F2) and `GET /api/stream.mp4` /
//! `GET /api/frame.mp4` (F4).
//!
//! [`Feed`] is one consumer's subscription and its [`muxer::Muxer`]: the
//! subscription's events become what a reader takes, the MIME type once, an
//! init segment, then one fragment per frame from a keyframe on, written by
//! `oxrtc-mp4`. The egress only carries what it returns (WebSocket messages,
//! an HTTP body).
//!
//! The subscription's own queue is the reader's: a reader slower than the
//! stream falls behind its budget, loses frames, resumes at a keyframe, and
//! the consumer's `dropped` counter and the log say so (`oxrtc_core::queue`);
//! the egress buffers no more than the fragment it is writing.

use std::sync::Arc;
use std::time::Duration;

use oxrtc_core::h265_fallback::{self, Taken, Untaken};
use oxrtc_core::h265_receiver::HevcReceiver;
use oxrtc_core::{BoxFuture, ConsumerTag, Event, Refusals, Stream, SubscribeError, Subscription, Want};
use oxrtc_mp4::HevcEntry;

pub(crate) mod container;
pub(crate) mod muxer;

use container::{Container, Mp4};
use muxer::{MuxerOf, Out};

/// Shortest time between two log lines of one reader's losses: a reader
/// that keeps falling behind loses about once per group of pictures.
pub const LOSS_REPORT_EVERY: Duration = Duration::from_secs(10);

/// What a reader asks for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Wish {
    /// The subscription's wish.
    pub want: Want,
    /// The H.265 sample entry: `hvc1` (Safari takes no other) unless the
    /// reader takes only `hev1`.
    pub hevc: HevcEntry,
    /// What the reader's H.265 decoder takes, when it says (the `hvc1` and
    /// `hev1` entries of an MSE codec list, D44): a camera none of them takes
    /// is passed over for the stream's H.264, by the rule WebRTC follows
    /// (`oxrtc_core::h265_fallback`). `None` for a reader that says nothing
    /// (`stream.mp4`, an empty MSE list), which is sent what comes.
    pub receivers: Option<Vec<HevcReceiver>>,
}

/// What a feed gives its egress next.
#[derive(Debug)]
pub(crate) enum Step {
    /// To send, in order.
    Send(Vec<Out>),
    /// It cannot go on, for this reason (go2rtc's words where it has them).
    Failed(String),
}

enum State {
    Subscribing(BoxFuture<'static, Result<Taken, Untaken>>),
    Streaming(Box<Subscription>),
}

/// One reader's subscription and muxer, in fragmented MP4 ([`Feed`]) or
/// MPEG-TS. Dropping it ends the subscription at once.
pub(crate) struct FeedOf<C> {
    state: State,
    muxer: MuxerOf<C>,
    stream: String,
    remote: String,
    /// The egress, as the log names it (`mse`, `mp4`).
    what: &'static str,
    /// The losses, said at most once per [`LOSS_REPORT_EVERY`].
    losses: Refusals,
}

/// The fragmented MP4 feed of MSE and `stream.mp4`.
pub(crate) type Feed = FeedOf<Mp4>;

impl<C: Container> FeedOf<C> {
    /// Subscribes to `stream` with `wish`, listed as `tag` says; `what` names
    /// the egress in the log.
    pub(crate) fn start(stream: Arc<Stream>, wish: Wish, tag: ConsumerTag, what: &'static str) -> Self {
        let remote = tag.remote.clone().unwrap_or_else(|| "-".to_owned());
        let want = wish.want.clone();
        let name = stream.name().to_owned();
        let receivers = wish.receivers.clone();
        let subscribing =
            Box::pin(async move { h265_fallback::subscribe(&stream, &want, receivers.as_deref(), tag).await });
        Self {
            state: State::Subscribing(subscribing),
            muxer: MuxerOf::new(wish.want, wish.hevc),
            stream: name,
            remote,
            what,
            losses: Refusals::every(LOSS_REPORT_EVERY),
        }
    }

    /// Whether the reader was told the MIME type, and so reads fragments.
    pub(crate) fn started(&self) -> bool {
        self.muxer.started()
    }

    /// The next thing to send. Cancel-safe: the subscription is kept across
    /// a cancellation, and an event is handled whole between two awaits.
    pub(crate) async fn next(&mut self) -> Step {
        loop {
            let event = match &mut self.state {
                State::Subscribing(subscribing) => match subscribing.as_mut().await {
                    // The track list the subscription began with, read
                    // while the reader's H.265 was checked.
                    Ok(taken) => {
                        self.state = State::Streaming(Box::new(taken.subscription));
                        Event::Tracks(taken.tracks)
                    }
                    Err(Untaken::Subscribe(error)) => return Step::Failed(subscribe_error(&error)),
                    Err(Untaken::Ended(end)) => return Step::Failed(format!("streams: {end}")),
                },
                State::Streaming(subscription) => match subscription.recv().await {
                    Ok(event) => event,
                    Err(end) => return Step::Failed(format!("streams: {end}")),
                },
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
                            "api: {} reader fell behind the stream: frames dropped, it resumes at the next keyframe",
                            self.what
                        );
                    }
                }
            }
        }
    }

    /// The muxer, for what an egress reads of it beside the bytes (HLS: the
    /// latest frame's keyframe flag, the zero, the TS tables).
    pub(crate) fn muxer(&mut self) -> &mut MuxerOf<C> {
        &mut self.muxer
    }

    /// The MIME type it gave was not sent ([`MuxerOf::untell`]).
    pub(crate) fn untell(&mut self) {
        self.muxer.untell();
    }

    /// The time of the latest frame written, from the reader's zero.
    pub(crate) fn latest(&self) -> Duration {
        self.muxer.latest
    }
}

/// go2rtc's words where it has them.
fn subscribe_error(error: &SubscribeError) -> String {
    match error {
        SubscribeError::NoMatchingSource | SubscribeError::NothingWanted => "codecs not matched".to_owned(),
        other => format!("streams: {other}"),
    }
}
