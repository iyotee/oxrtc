//! Which `ffmpeg:` sources are a sound transcode oxrtc makes itself, with no
//! process (W5, D37): `#audio=` of one codec it encodes (Opus, G.711, L16)
//! from an input it reads itself - a stream of this server (by name, or by
//! its loopback RTSP URL) or an `http(s)` URL, which must then answer
//! `audio/wav` - and nothing that changes what FFmpeg would do beyond that:
//! no `#video=`, `#raw=`, filter, `#query=`, and an `#input=` only when its
//! template is made of probing flags (openIVR's `dedup_input`). Anything
//! else stays FFmpeg, and so does a source whose input turns out to carry
//! another sound (AAC, Opus) once it is read ([`super::Attempt::NotNative`]).

use oxrtc_audio::transcode::{Output, OutputCodec};
use oxrtc_core::hub::validate_name;
use oxrtc_core::{MediaWant, SourceSpec, Want};

use crate::split::quote_split;
use crate::templates::Templates;

/// What the native transcode reads.
#[derive(Clone, PartialEq, Eq)]
pub(crate) enum Input {
    /// A stream of this hub, the sound its consumer's wish allows.
    Stream {
        /// The stream's name.
        name: String,
        /// The sound a relay URL's query allows (`Any` when it says nothing).
        audio: MediaWant,
    },
    /// An `http(s)` URL that answers a WAV stream. Holds the source's URL,
    /// credentials included: never logged.
    Wav(String),
}

impl std::fmt::Debug for Input {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Stream { name, audio } => f.debug_struct("Stream").field("name", name).field("audio", audio).finish(),
            Self::Wav(url) => f.debug_tuple("Wav").field(&oxrtc_core::spec::redact(url)).finish(),
        }
    }
}

impl Input {
    /// Whether it reads a stream of this hub (no HTTP client needed).
    pub(crate) fn is_stream(&self) -> bool {
        matches!(self, Self::Stream { .. })
    }
}

/// A native sound transcode: what it reads, what it makes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct NativePlan {
    pub(crate) input: Input,
    pub(crate) output: Output,
}

/// The output of an `#audio=` value whose template is the built-in one:
/// go2rtc's templates of the codecs oxrtc encodes. `opus/16000` is left to
/// FFmpeg (it says a rate and a channel count of its own).
fn output_of(value: &str) -> Option<Output> {
    let (codec, rate) = match value {
        "opus" => (OutputCodec::Opus, 48_000),
        "pcma" | "pcma/8000" => (OutputCodec::Alaw, 8000),
        "pcma/16000" => (OutputCodec::Alaw, 16_000),
        "pcma/48000" => (OutputCodec::Alaw, 48_000),
        "pcmu" | "pcmu/8000" => (OutputCodec::Ulaw, 8000),
        "pcmu/16000" => (OutputCodec::Ulaw, 16_000),
        "pcmu/48000" => (OutputCodec::Ulaw, 48_000),
        "pcm" | "pcm/8000" => (OutputCodec::L16, 8000),
        "pcm/16000" => (OutputCodec::L16, 16_000),
        "pcm/48000" => (OutputCodec::L16, 48_000),
        _ => return None,
    };
    Some(Output { codec, rate })
}

/// Whether an input template only says how FFmpeg probes and buffers its
/// input - what the native reader has no use for: `-fflags nobuffer`,
/// `-flags low_delay`, `-probesize N`, `-analyzeduration N`, and one
/// `-i {input}`. openIVR's `dedup_input` is
/// `-fflags nobuffer -flags low_delay -probesize 32 -i {input}`, go2rtc's
/// `http` the same without `-probesize`.
fn probing_only(template: &str) -> bool {
    let Ok(words) = quote_split(template) else { return false };
    let mut inputs = 0;
    let mut words = words.iter().map(String::as_str);
    while let Some(flag) = words.next() {
        let value = words.next().unwrap_or_default();
        let number = !value.is_empty() && value.bytes().all(|b| b.is_ascii_digit());
        let known = match flag {
            "-fflags" => matches!(value, "nobuffer" | "+nobuffer"),
            "-flags" => matches!(value, "low_delay" | "+low_delay"),
            "-probesize" | "-analyzeduration" => number,
            "-i" => {
                inputs += 1;
                value == "{input}"
            }
            _ => false,
        };
        if !known {
            return false;
        }
    }
    inputs == 1
}

/// The native plan of `spec` for the stream `owner`, the server's RTSP
/// relay at `relay`; `None` leaves the source to FFmpeg.
pub(crate) fn plan(spec: &SourceSpec, templates: &Templates, relay: &str, owner: &str) -> Option<NativePlan> {
    let mut audio = None;
    let mut input_template = None;
    for (key, value) in spec.fragments() {
        match key {
            "audio" if audio.is_none() => audio = Some(value),
            "input" if input_template.is_none() => input_template = Some(value),
            "timeout" => {}
            _ => return None,
        }
    }
    let value = audio?;
    let output = output_of(value)?;
    if !templates.is_default(value) {
        return None;
    }
    let target = spec.target();
    let lower = target.to_ascii_lowercase();
    let input = if lower.starts_with("http://") || lower.starts_with("https://") {
        let template = match input_template {
            Some(name) => templates.get(name)?,
            None => templates.get("http")?,
        };
        if !probing_only(template) {
            return None;
        }
        Input::Wav(target.to_owned())
    } else if lower.starts_with("rtsp://") {
        if input_template.is_some() || !crate::relay::reads_relay(target, relay) {
            return None;
        }
        let path = target.strip_prefix("rtsp://")?.split_once('/')?.1;
        let (name, query) = path.split_once('?').unwrap_or((path, ""));
        let wish = if query.is_empty() { Want::all() } else { Want::from_query(query).ok()? };
        if validate_name(name).is_err() || name == owner || !wish.audio.is_wanted() {
            return None;
        }
        Input::Stream { name: name.to_owned(), audio: wish.audio }
    } else {
        if input_template.is_some() || validate_name(target).is_err() || target == owner {
            return None;
        }
        Input::Stream { name: target.to_owned(), audio: MediaWant::Any }
    };
    Some(NativePlan { input, output })
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;

    const RELAY: &str = "127.0.0.1:8554";
    /// openIVR's `dedup_input` template (media_server_manager.py).
    const DEDUP: &str = "-fflags nobuffer -flags low_delay -probesize 32 -i {input}";

    fn templates(extra: &[(&str, &str)]) -> Templates {
        let config: BTreeMap<String, String> = extra.iter().map(|(k, v)| ((*k).to_owned(), (*v).to_owned())).collect();
        Templates::new(&config)
    }

    fn planned(spec: &str, extra: &[(&str, &str)]) -> Option<NativePlan> {
        plan(&SourceSpec::parse(spec).unwrap(), &templates(extra), RELAY, "camera_1_rec")
    }

    /// openIVR's two sound sources of a camera (`recording_sources()`) are
    /// both native.
    #[test]
    fn openivrs_two_sound_sources_are_native() {
        let opus = Output { codec: OutputCodec::Opus, rate: 48_000 };
        let dedup = planned(
            "ffmpeg:http://127.0.0.1:50311/camera_1_src#audio=opus#input=dedup_input",
            &[("dedup_input", DEDUP)],
        )
        .unwrap();
        assert_eq!(dedup.input, Input::Wav("http://127.0.0.1:50311/camera_1_src".into()));
        assert_eq!(dedup.output, opus);
        let relay = planned("ffmpeg:rtsp://127.0.0.1:8554/camera_1_src#audio=opus#timeout=20", &[]).unwrap();
        assert_eq!(relay.input, Input::Stream { name: "camera_1_src".into(), audio: MediaWant::Any });
        assert_eq!(relay.output, opus);
        // localhost on the relay's port is the relay too.
        assert!(planned("ffmpeg:rtsp://localhost:8554/camera_1_src#audio=pcma", &[]).is_some());
        // By name, and a relay URL whose query keeps the sound.
        assert_eq!(
            planned("ffmpeg:camera_1_src#audio=pcmu/16000", &[]).map(|p| p.output),
            Some(Output { codec: OutputCodec::Ulaw, rate: 16_000 })
        );
        assert!(matches!(
            planned("ffmpeg:rtsp://127.0.0.1:8554/camera_1_src?audio=pcma#audio=opus", &[]).map(|p| p.input),
            Some(Input::Stream { audio: MediaWant::OneOf(_), .. })
        ));
        // go2rtc's own http template only probes: native too.
        assert!(planned("ffmpeg:https://u:p@cam.local/audio.wav#audio=opus", &[]).is_some());
    }

    /// Everything that would make FFmpeg do more than read and encode one
    /// sound stays FFmpeg.
    #[test]
    fn what_ffmpeg_does_beyond_that_stays_ffmpeg() {
        for spec in [
            "ffmpeg:camera_1_src#video=h264",
            "ffmpeg:camera_1_src#video=h264#audio=opus",
            "ffmpeg:camera_1_src#audio=opus#audio=pcma",
            "ffmpeg:camera_1_src#audio=aac",
            "ffmpeg:camera_1_src#audio=opus/16000",
            "ffmpeg:camera_1_src#audio=copy",
            "ffmpeg:camera_1_src#audio=opus#raw=-af volume=2",
            "ffmpeg:camera_1_src#audio=opus#async",
            "ffmpeg:camera_1_src#audio=opus#query=queue=live",
            "ffmpeg:camera_1_src#audio=opus#input=dedup_input",
            "ffmpeg:rtsp://192.168.1.10:554/stream#audio=opus",
            "ffmpeg:rtsp://user:pw@127.0.0.1:8554/camera_1_src#audio=opus",
            "ffmpeg:rtsp://127.0.0.1:8555/camera_1_src#audio=opus",
            "ffmpeg:rtsp://127.0.0.1:8554/camera_1_src?video#audio=opus",
            "ffmpeg:rtsp://127.0.0.1:8554/camera_1_rec#audio=opus",
            "ffmpeg:rtsps://127.0.0.1:8554/camera_1_src#audio=opus",
            "ffmpeg:http://127.0.0.1:50311/camera_1_src#audio=opus#input=other",
            "ffmpeg:http://127.0.0.1:50311/camera_1_src#audio=opus#input=reads_a_file",
        ] {
            assert_eq!(
                planned(spec, &[("dedup_input", DEDUP), ("other", "-re -i {input}"), ("reads_a_file", "-i /etc/x")]),
                None,
                "{spec}"
            );
        }
        // An operator's own `opus` template, or `http` one that does more than probe.
        assert_eq!(planned("ffmpeg:camera_1_src#audio=opus", &[("opus", "-c:a libopus -b:a 24k")]), None);
        assert_eq!(planned("ffmpeg:http://cam/a.wav#audio=opus", &[("http", "-re -i {input}")]), None);
    }

    #[test]
    fn a_probing_template_is_its_flags_and_one_input() {
        assert!(probing_only(DEDUP));
        assert!(probing_only("-fflags nobuffer -flags low_delay -i {input}"));
        assert!(probing_only("-analyzeduration 0 -probesize 32 -i {input}"));
        assert!(!probing_only("-i {input} -i {input}"));
        assert!(!probing_only("-probesize 32"));
        assert!(!probing_only("-probesize x -i {input}"));
        assert!(!probing_only("-f wav -i {input}"));
        assert!(!probing_only("-i \"{input}"));
    }
}
