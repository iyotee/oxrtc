//! A reader of a camera's H.265 that does not decode it is given the
//! stream's H.264 instead (W7 for WebRTC, M5 item D for MSE; D41, D44): one
//! subscription path for every egress that knows what its reader decodes.
//!
//! The reader subscribes as it asks; once the subscription has its tracks,
//! the camera's SPS is held against what the reader takes
//! ([`crate::h265_receiver::refused`]). A picture it does not take sends the
//! reader to the stream's next source without H.265 (openIVR's transcode):
//! the same wish without H.265 is subscribed to, the first subscription held
//! until that one has tracks so that the camera is not let go in between.
//! A stream with no such source sends the camera's H.265 anyway, as before.

use std::sync::Arc;

use crate::error::{SubEnd, SubscribeError};
use crate::h265_receiver::{HevcCamera, HevcReceiver, refused};
use crate::media::{Codec, MediaKind, TrackInfo};
use crate::stream::{ConsumerTag, Stream};
use crate::subscription::{Event, Subscription};
use crate::want::{MediaWant, Want};

/// A subscription with its first track list.
#[derive(Debug)]
pub struct Taken {
    /// The subscription, its track list already read.
    pub subscription: Subscription,
    /// Its tracks.
    pub tracks: Arc<[TrackInfo]>,
    /// The camera's picture the reader did not take, when it was sent the
    /// stream's H.264 instead; the reader's wish then had its H.265 taken out.
    pub passed_over: Option<HevcCamera>,
}

/// Why no subscription with tracks came.
#[derive(Debug, thiserror::Error)]
pub enum Untaken {
    /// The stream refused the wish.
    #[error(transparent)]
    Subscribe(#[from] SubscribeError),
    /// The subscription ended before its tracks.
    #[error("{0}")]
    Ended(#[from] SubEnd),
}

/// Subscribes to `stream` with `want` and waits for the track list.
pub async fn with_tracks(
    stream: &Arc<Stream>,
    want: &Want,
    tag: ConsumerTag,
) -> Result<(Subscription, Arc<[TrackInfo]>), Untaken> {
    let mut subscription = stream.subscribe(want, tag).await?;
    loop {
        match subscription.recv().await? {
            Event::Tracks(tracks) => return Ok((subscription, tracks)),
            _ => continue,
        }
    }
}

/// `want` without H.265 among its video codecs.
pub fn without_h265(want: &Want) -> Want {
    let video = match &want.video {
        MediaWant::OneOf(codecs) => {
            let rest: Vec<Codec> = codecs.iter().copied().filter(|c| *c != Codec::H265).collect();
            if rest.is_empty() { MediaWant::Skip } else { MediaWant::OneOf(rest) }
        }
        // `Any` cannot say "but H.265": such a reader is not checked.
        other => other.clone(),
    };
    Want { video, audio: want.audio.clone() }
}

/// Subscribes for a reader that decodes what `receivers` say (`None`: a
/// reader that says nothing, `stream.mp4`, which is sent what comes).
pub async fn subscribe(
    stream: &Arc<Stream>,
    want: &Want,
    receivers: Option<&[HevcReceiver]>,
    tag: ConsumerTag,
) -> Result<Taken, Untaken> {
    let (subscription, tracks) = with_tracks(stream, want, tag.clone()).await?;
    let first = Taken { subscription, tracks, passed_over: None };
    let Some(receivers) = receivers else { return Ok(first) };
    let Some(camera) = refused(receivers, &first.tracks) else { return Ok(first) };
    let fallback = without_h265(want);
    if !fallback.video.is_wanted() || matches!(want.video, MediaWant::Any) {
        return Ok(first);
    }
    match with_tracks(stream, &fallback, tag.clone()).await {
        Ok((subscription, tracks)) if tracks.iter().any(|t| t.kind == MediaKind::Video) => {
            tracing::info!(
                stream = stream.name(),
                reader = %tag.kind,
                profile = camera.profile,
                tier = u8::from(camera.tier),
                level = camera.level,
                takes = ?receivers,
                "the reader does not take this camera's H.265; sent the stream's H.264"
            );
            Ok(Taken { subscription, tracks, passed_over: Some(camera) })
        }
        _ => Ok(first),
    }
}
