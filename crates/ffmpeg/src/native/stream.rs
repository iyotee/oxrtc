//! The native transcode of a stream of this hub: its sound read through a
//! subscription, as FFmpeg read it through the RTSP relay (a recorder's
//! queue: every frame lost is a hole in a recording).

use oxrtc_audio::transcode::{Input, InputCodec};
use oxrtc_core::{
    CancellationToken, Codec, ConsumerClass, ConsumerTag, Event, MediaKind, MediaWant, ProducerSink, SourceError,
    SubEnd, SubscribeError, TrackInfo, Want,
};

use super::publish::Publisher;
use super::{Attempt, NativeDriver};

/// The consumer a native transcode is listed as in `/api/streams`.
pub(crate) const CONSUMER_KIND: &str = "native_audio";

/// What the transcode reads from `track`, or why it does not.
pub(crate) fn input_of(track: &TrackInfo) -> Result<Input, String> {
    let codec = match track.codec {
        Codec::Pcma => InputCodec::Alaw,
        Codec::Pcmu => InputCodec::Ulaw,
        Codec::L16 => InputCodec::L16,
        other => return Err(format!("the sound is {}", other.as_str())),
    };
    Ok(Input { codec, rate: track.clock_rate, channels: track.channels.max(1) })
}

fn subscribe_error(error: SubscribeError) -> SourceError {
    match error {
        SubscribeError::Closed => SourceError::Ended,
        // No source of the stream has a sound: what the relay answers 455 to.
        SubscribeError::NoMatchingSource => SourceError::Declined(format!("native audio: {error}")),
        other => SourceError::Unreachable(format!("native audio: {other}")),
    }
}

pub(crate) async fn run(
    driver: &NativeDriver,
    name: &str,
    audio: &MediaWant,
    sink: &ProducerSink,
    cancel: &CancellationToken,
) -> Attempt {
    let Some(hub) = driver.hub.upgrade() else { return Attempt::Ran(Err(SourceError::Ended)) };
    // What the RTSP relay answers for a stream it does not have: 404.
    let Some(stream) = hub.get(name) else {
        return Attempt::Ran(Err(SourceError::Declined(format!("native audio: no stream {name}"))));
    };
    drop(hub);
    let want = Want { video: MediaWant::Skip, audio: audio.clone() };
    let tag = ConsumerTag::new(CONSUMER_KIND).with_class(ConsumerClass::Recorder);
    let subscribed = tokio::select! {
        biased;
        () = cancel.cancelled() => return Attempt::Ran(Ok(())),
        subscribed = stream.subscribe(&want, tag) => subscribed,
    };
    let mut subscription = match subscribed {
        Ok(subscription) => subscription,
        Err(error) => return Attempt::Ran(Err(subscribe_error(error))),
    };
    let mut publisher: Option<Publisher> = None;
    let mut sound: Option<usize> = None;
    loop {
        let event = tokio::select! {
            biased;
            () = cancel.cancelled() => return Attempt::Ran(Ok(())),
            event = subscription.recv() => event,
        };
        match event {
            Ok(Event::Tracks(tracks)) => {
                let found = tracks.iter().position(|t| t.kind == MediaKind::Audio);
                let Some((index, track)) = found.and_then(|i| tracks.get(i).map(|t| (i, t))) else {
                    // The relay answers a sound it has not with 455 (FFmpeg: declined).
                    return Attempt::Ran(Err(SourceError::Declined(format!("native audio: {name} has no sound"))));
                };
                let input = match input_of(track) {
                    Ok(input) => input,
                    Err(why) if publisher.is_none() => return Attempt::NotNative(why),
                    Err(why) => return Attempt::Ran(Err(SourceError::Unreachable(format!("native audio: {why}")))),
                };
                sound = Some(index);
                if publisher.as_ref().is_some_and(|p| p.reads(input)) {
                    continue;
                }
                match Publisher::new(input, driver.plan.output, sink) {
                    Ok(made) => publisher = Some(made),
                    Err(why) if publisher.is_none() => return Attempt::NotNative(why),
                    Err(why) => return Attempt::Ran(Err(SourceError::Unreachable(format!("native audio: {why}")))),
                }
            }
            Ok(Event::Frame { track, frame }) => {
                if Some(track) != sound {
                    continue;
                }
                if let Some(publisher) = publisher.as_mut()
                    && let Err(error) = publisher.push(&frame.data, frame.pts, frame.received, sink)
                {
                    return Attempt::Ran(Err(error));
                }
            }
            Ok(Event::Discontinuity) => {
                if let Some(publisher) = publisher.as_mut() {
                    publisher.discontinuity();
                }
            }
            Err(SubEnd::Closed) => return Attempt::Ran(Err(SourceError::Ended)),
            Err(SubEnd::NoTracks) => {
                return Attempt::Ran(Err(SourceError::Declined(format!("native audio: {name} has no sound"))));
            }
        }
    }
}
