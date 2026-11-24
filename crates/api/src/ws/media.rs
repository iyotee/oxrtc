//! What one socket of `/api/ws` streams, beside its offers: an `mse`'s
//! fragments (F3), an `mjpeg`'s pictures (F5) or an `mp4`'s keyframes (F2),
//! one at a time per socket, counted with the offers against the sessions a
//! socket may ask for.
//!
//! `mp4` is go2rtc's poster mode: its player, given neither MSE nor native
//! HLS, sets each binary message as a `<video>`'s `data:` URL and draws its
//! first picture as the poster. go2rtc answers `{"type":"mse","value":
//! "<MIME type>"}` and then sends, at each keyframe of the video, the init
//! segment and that keyframe's fragment in one message, a whole MP4 file.
//! Here the same bytes come from MSE's one fMP4 path (the `Feed` and its
//! muxer, as `frame.mp4`'s): its fragments that are not a keyframe's are
//! not sent.

use bytes::Bytes;
use oxrtc_core::{BoxFuture, ConsumerTag};

use super::conn::Context;
use super::message::Outgoing;
use super::mse;
use crate::fmp4::muxer::Out;
use crate::fmp4::{Feed, Step};
use crate::mjpeg::{self, Pictures};

/// A message to send.
pub(crate) enum Sent {
    Text(Outgoing),
    Binary(Bytes),
}

/// A socket's stream.
pub(crate) enum Media {
    /// Fragmented MP4 for the browser's Media Source Extensions.
    Mse(Box<Feed>),
    /// JPEG pictures, while the stream's own or its transcode is opened.
    MjpegStarting(BoxFuture<'static, Result<Pictures, String>>),
    /// JPEG pictures, the player told so.
    Mjpeg(Box<Pictures>),
    /// Each keyframe as a whole MP4 file, with the init segment it is read
    /// with (the latest).
    Mp4 { feed: Box<Feed>, init: Option<Bytes> },
}

impl Media {
    /// The consumer tag of a socket's stream of `kind`.
    fn tag(context: &Context, kind: &str) -> ConsumerTag {
        let mut tag = ConsumerTag::new(kind).with_start(context.start).pooled();
        if let Some(remote) = context.remote {
            tag = tag.with_remote(remote.to_string());
        }
        if let Some(agent) = context.user_agent.as_deref() {
            tag = tag.with_user_agent(agent);
        }
        tag
    }

    /// `mse` in the codecs of `codecs`, or why not (go2rtc's words).
    pub(crate) fn mse(context: &Context, codecs: &str) -> Result<Self, &'static str> {
        let stream = context.stream().ok_or("stream not found")?;
        let wish = mse::wish(codecs);
        if wish.want.is_empty() {
            return Err("codecs not matched");
        }
        tracing::debug!(stream = %stream.name(), "api: mse over websocket");
        let tag = Self::tag(context, mse::CONSUMER_KIND);
        Ok(Self::Mse(Box::new(Feed::start(stream, wish, tag, "mse"))))
    }

    /// `mp4` in the video codecs of `codecs`, or why not (go2rtc's words).
    pub(crate) fn mp4(context: &Context, codecs: &str) -> Result<Self, &'static str> {
        let stream = context.stream().ok_or("stream not found")?;
        let wish = mse::mp4_wish(codecs);
        if wish.want.is_empty() {
            return Err("codecs not matched");
        }
        tracing::debug!(stream = %stream.name(), "api: mp4 over websocket");
        let tag = Self::tag(context, crate::mp4::CONSUMER_KIND);
        Ok(Self::Mp4 { feed: Box::new(Feed::start(stream, wish, tag, "mp4")), init: None })
    }

    /// `mjpeg`, or why not.
    pub(crate) fn mjpeg(context: &Context) -> Result<Self, &'static str> {
        let stream = context.stream().ok_or("stream not found")?;
        tracing::debug!(stream = %stream.name(), "api: mjpeg over websocket");
        let tag = Self::tag(context, mjpeg::CONSUMER_KIND);
        let hub = context.state.hub.clone();
        Ok(Self::MjpegStarting(Box::pin(async move { Pictures::start(&hub, stream, tag).await })))
    }

    /// The type an error about it names.
    pub(crate) fn about(&self) -> &'static str {
        match self {
            Self::Mse(_) => "mse",
            Self::MjpegStarting(_) | Self::Mjpeg(_) => "mjpeg",
            Self::Mp4 { .. } => "mp4",
        }
    }

    /// Whether the player was told what follows: a failure then closes the
    /// socket (the player starts again), one before is an error it answers
    /// by trying its next mode.
    pub(crate) fn started(&self) -> bool {
        match self {
            Self::Mse(feed) | Self::Mp4 { feed, .. } => feed.started(),
            Self::MjpegStarting(_) => false,
            Self::Mjpeg(_) => true,
        }
    }

    /// What to send next, or why it cannot go on. Cancel-safe: a stream
    /// being opened is kept across a cancellation.
    pub(crate) async fn next(&mut self) -> Result<Vec<Sent>, String> {
        match self {
            Self::Mse(feed) => match feed.next().await {
                Step::Failed(reason) => Err(reason),
                Step::Send(out) => Ok(out
                    .into_iter()
                    .map(|item| match item {
                        Out::Mime(mime) => Sent::Text(Outgoing::Mse(mime)),
                        Out::Init(bytes) | Out::Binary(bytes) => Sent::Binary(bytes),
                    })
                    .collect()),
            },
            Self::MjpegStarting(starting) => {
                let pictures = starting.as_mut().await?;
                // As go2rtc: the player is told once the reader is added,
                // then the pictures come.
                *self = Self::Mjpeg(Box::new(pictures));
                Ok(vec![Sent::Text(Outgoing::Mjpeg)])
            }
            Self::Mjpeg(pictures) => Ok(vec![Sent::Binary(pictures.next().await?)]),
            Self::Mp4 { feed, init } => loop {
                let out = match feed.next().await {
                    Step::Failed(reason) => return Err(reason),
                    Step::Send(out) => out,
                };
                // The latest frame's: a feed gives one fragment per frame.
                let key = feed.muxer().last_key;
                let mut sent = Vec::new();
                for item in out {
                    match item {
                        Out::Mime(mime) => sent.push(Sent::Text(Outgoing::Mse(mime))),
                        Out::Init(segment) => *init = Some(segment),
                        Out::Binary(fragment) if key => {
                            if let Some(init) = init.as_ref() {
                                sent.push(Sent::Binary(file(init, &fragment)));
                            }
                        }
                        Out::Binary(_) => {}
                    }
                }
                if !sent.is_empty() {
                    return Ok(sent);
                }
            },
        }
    }
}

/// One MP4 file: the init segment, then a keyframe's fragment.
fn file(init: &Bytes, fragment: &Bytes) -> Bytes {
    let mut file = bytes::BytesMut::with_capacity(init.len() + fragment.len());
    file.extend_from_slice(init);
    file.extend_from_slice(fragment);
    file.freeze()
}
