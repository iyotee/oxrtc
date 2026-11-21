//! The native transcode of a WAV stream over HTTP (openIVR's sound
//! de-duplicator): the samples dated by their count, as FFmpeg dates a WAV,
//! each piece received when it arrived.

use std::time::Duration;

use oxrtc_audio::transcode::{Input, InputCodec};
use oxrtc_audio::wav::{Encoding, Reader, WavError};
use oxrtc_core::{CancellationToken, ProducerSink, SourceError};
use tokio::time::Instant;

use super::publish::Publisher;
use super::{Attempt, NativeDriver};

/// The most bytes of samples handed to the transcoder at once: well under
/// what it takes in one call (`MAX_INPUT_BYTES`).
const PIECE_BYTES: usize = 16 * 1024;

/// How far the samples read may run ahead of the wall clock since the first
/// of them: the lead the core's timeline allows a source
/// (`MAX_LEAD_SECS`). A server that sends faster than time (a file sent at
/// the network's speed) is read no faster than that, TCP holding the rest at
/// the server; read as it came, a transcode took such a server at whatever
/// rate it could encode, 9000 s of sound in 5 s, every packet published.
/// openIVR's de-duplicator replays about 4 s of a camera's past at once,
/// which this leaves as it is.
const MAX_LEAD: Duration = Duration::from_secs(oxrtc_core::timeline::MAX_LEAD_SECS.unsigned_abs());

/// When the next piece may be read, `written` samples at `rate` having been
/// read since `start`: `None` while they are no more than [`MAX_LEAD`] ahead.
fn paused_until(start: Instant, written: i64, rate: u32, now: Instant) -> Option<Instant> {
    let read = Duration::from_micros(written.unsigned_abs().saturating_mul(1_000_000) / u64::from(rate.max(1)));
    // More than an Instant can say has been read: wait the lead again.
    let resume = start.checked_add(read.checked_sub(MAX_LEAD)?).unwrap_or(now + MAX_LEAD);
    (resume > now).then_some(resume)
}

/// The media types of a WAV stream.
fn is_wav(content_type: Option<&str>) -> bool {
    let media = content_type.unwrap_or_default().split(';').next().unwrap_or_default().trim().to_ascii_lowercase();
    matches!(media.as_str(), "audio/wav" | "audio/x-wav" | "audio/wave" | "audio/vnd.wave")
}

/// FFmpeg's reading of an HTTP status it was refused with: a 4XX is the
/// server declining (openIVR's 415 for a sound that is not G.711), anything
/// else worth asking again.
fn refusal(status: u16) -> SourceError {
    let text = format!("native audio: Server returned {status}");
    if (400..500).contains(&status) { SourceError::Declined(text) } else { SourceError::Unreachable(text) }
}

pub(crate) async fn run(driver: &NativeDriver, url: &str, sink: &ProducerSink, cancel: &CancellationToken) -> Attempt {
    let Some(http) = driver.http.clone() else { return Attempt::NotNative("no HTTP client".into()) };
    let opened = tokio::select! {
        biased;
        () = cancel.cancelled() => return Attempt::Ran(Ok(())),
        opened = http.get(url.to_owned(), driver.answer_timeout()) => opened,
    };
    let mut body = match opened {
        Ok(body) => body,
        Err(error) => return Attempt::Ran(Err(error)),
    };
    if !(200..300).contains(&body.status()) {
        return Attempt::Ran(Err(refusal(body.status())));
    }
    if !is_wav(body.content_type()) {
        return Attempt::NotNative(format!("the answer is {}", body.content_type().unwrap_or("untyped")));
    }
    let mut reader = Reader::new();
    let mut publisher: Option<Publisher> = None;
    let mut samples = Vec::new();
    let mut written: i64 = 0;
    let mut start: Option<(Instant, u32)> = None;
    loop {
        let due = publisher.as_ref().and_then(Publisher::next_due);
        let paused = start.and_then(|(at, rate)| paused_until(at, written, rate, Instant::now()));
        // A body's next piece is cancel-safe: nothing is read until it is whole.
        let chunk = tokio::select! {
            biased;
            () = cancel.cancelled() => return Attempt::Ran(Ok(())),
            () = super::publish::wait_until(due) => {
                if let Some(publisher) = publisher.as_mut()
                    && let Err(error) = publisher.publish_due(sink)
                {
                    return Attempt::Ran(Err(error));
                }
                continue;
            }
            () = super::publish::wait_until(paused), if paused.is_some() => continue,
            chunk = body.chunk(), if paused.is_none() => chunk,
        };
        let received = Instant::now();
        let data = match chunk {
            None => return Attempt::Ran(Err(SourceError::Ended)),
            Some(Err(error)) => return Attempt::Ran(Err(error)),
            Some(Ok(data)) => data,
        };
        samples.clear();
        if let Err(error) = reader.push(&data, &mut samples) {
            return match (error, &publisher) {
                // FFmpeg may read what this reader does not (another coding, 24 bits).
                (WavError::Unsupported(what), None) => Attempt::NotNative(format!("a WAV of {what}")),
                (WavError::NotWav, None) => Attempt::NotNative("not a WAV stream".into()),
                (error, _) => Attempt::Ran(Err(SourceError::Protocol(format!("native audio: {error}")))),
            };
        }
        let Some(format) = reader.format() else { continue };
        let publisher = match publisher.as_mut() {
            Some(publisher) => publisher,
            None => {
                let codec = match format.encoding {
                    Encoding::Pcm16 => InputCodec::PcmLe,
                    Encoding::Alaw => InputCodec::Alaw,
                    Encoding::Ulaw => InputCodec::Ulaw,
                };
                let input = Input { codec, rate: format.rate, channels: format.channels };
                match Publisher::new(input, driver.plan.output, sink) {
                    Ok(made) => publisher.insert(made),
                    Err(why) => return Attempt::NotNative(why),
                }
            }
        };
        if samples.is_empty() {
            continue;
        }
        start.get_or_insert((received, format.rate));
        // In pieces the transcoder takes whole (a fast server's read may be large).
        let align = format.block_align().max(1);
        for piece in samples.chunks(PIECE_BYTES - PIECE_BYTES % align) {
            if let Err(error) = publisher.push(piece, written, received, sink) {
                return Attempt::Ran(Err(error));
            }
            written = written.saturating_add(i64::try_from(piece.len() / align).unwrap_or(i64::MAX));
        }
        if reader.finished() {
            return Attempt::Ran(Err(SourceError::Ended));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wav_media_types_and_refusals() {
        assert!(is_wav(Some("audio/wav")));
        assert!(is_wav(Some("Audio/X-WAV; charset=binary")));
        assert!(!is_wav(Some("audio/aac")));
        assert!(!is_wav(None));
        assert!(matches!(refusal(415), SourceError::Declined(_)));
        assert!(matches!(refusal(404), SourceError::Declined(_)));
        assert!(matches!(refusal(503), SourceError::Unreachable(_)));
    }

    #[test]
    fn the_samples_read_are_held_to_the_lead() {
        let start = Instant::now();
        // 9 s of 8 kHz read at once: under the lead, read on.
        assert_eq!(paused_until(start, 72_000, 8000, start), None);
        // 30 s read at once: wait until 20 s have passed.
        assert_eq!(paused_until(start, 240_000, 8000, start), Some(start + Duration::from_secs(20)));
        assert_eq!(paused_until(start, 240_000, 8000, start + Duration::from_secs(20)), None);
        // In real time it never waits.
        assert_eq!(paused_until(start, 8000 * 3600, 8000, start + Duration::from_secs(3600)), None);
        assert_eq!(paused_until(start, 0, 0, start), None);
    }
}
