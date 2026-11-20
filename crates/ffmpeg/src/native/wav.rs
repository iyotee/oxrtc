//! The native transcode of a WAV stream over HTTP (openIVR's sound
//! de-duplicator): the samples dated by their count, as FFmpeg dates a WAV,
//! each piece received when it arrived.

use oxrtc_audio::transcode::{Input, InputCodec};
use oxrtc_audio::wav::{Encoding, Reader, WavError};
use oxrtc_core::{CancellationToken, ProducerSink, SourceError};
use tokio::time::Instant;

use super::publish::Publisher;
use super::{Attempt, NativeDriver};

/// The most bytes of samples handed to the transcoder at once: well under
/// what it takes in one call (`MAX_INPUT_BYTES`).
const PIECE_BYTES: usize = 16 * 1024;

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
    loop {
        let chunk = tokio::select! {
            biased;
            () = cancel.cancelled() => return Attempt::Ran(Ok(())),
            chunk = body.chunk() => chunk,
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
}
