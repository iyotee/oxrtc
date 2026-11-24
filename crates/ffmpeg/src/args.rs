//! From an `ffmpeg:` source to an FFmpeg argument vector, the way go2rtc
//! 1.9.15 builds it (`parseArgs` in `internal/ffmpeg/ffmpeg.go`, `Args.String`
//! in `pkg/ffmpeg/ffmpeg.go`), with go2rtc's command line split by its own
//! `QuoteSplit` rules.
//!
//! Where go2rtc would take text from the source as FFmpeg arguments (an
//! unknown `#video=`/`#audio=`/`#raw=`/`#input=` value is used verbatim), the
//! source is refused instead (D6), and every value that ends up inside an
//! argument (`#width=`, `#timeout=`, `#bitrate=`) must be a number.

use std::time::Duration;

use oxrtc_core::hub::validate_name;
use oxrtc_core::spec::{MAX_TIMEOUT_SECS, timeout_secs};
use oxrtc_core::{Codec, ConsumerClass, Declared, MediaKind, QUEUE_QUERY_KEY, SourceSpec, SpecError};

use crate::split::quote_split;
use crate::templates::Templates;

/// Placeholder of the input URL in input templates.
const INPUT: &str = "{input}";
/// Placeholder of the publication URL in the output template.
const OUTPUT: &str = "{output}";
/// What an input that reads a stream of this server by name starts with: no
/// frame-rate guess. FFmpeg reads 20 pictures to guess a stream's rate before
/// it opens it, unless the stream says it, and the relay says it in its
/// `a=framerate` (`crates/rtsp-server` `sdp.rs` and `pace.rs`, D36), which
/// FFmpeg reads since 8.0 ([`crate::version`]): the pace of a coded track
/// (few cameras write it in their parameter sets; the owner's H.265 cameras
/// do not, and their transcode was encoded as 90000 pictures a second, at
/// half the bits, while nothing said it: W4 review), and the rate of a Motion
/// JPEG track, however fast (the owner, 2026-11-20, D36). The guess cost a
/// webcam's transcode 0.6 s at 30 pictures a second and 20 s at one, and
/// 28 MiB of resident memory.
const RELAY_INPUT: &str = "-fpsprobesize 0";
/// Largest `#width=` / `#height=`.
const MAX_DIMENSION: i32 = 16_384;

/// Everything a run needs, computed once from the source.
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct Plan {
    /// Holds the input URL, credentials included: never logged as is.
    args: Vec<String>,
    output_slots: Vec<usize>,
    declared: Option<Declared>,
    stall_timeout: Option<Duration>,
    /// The input is this server's RTSP relay (a stream name, or its URL).
    local: bool,
    /// That input's URL, as it stands in `args`, when it names the relay and
    /// carries no user information of its own: where a run's credential for
    /// reading the relay goes (D24).
    relay_input: Option<String>,
    /// Where [`RELAY_INPUT`] goes when the input is a stream of the relay
    /// read by name and FFmpeg reads the rate the relay says
    /// ([`crate::version`]).
    rate_guess_at: Option<usize>,
    /// FFmpeg decodes and encodes ([`Plan::transcodes`]).
    transcodes: bool,
    /// The output is Motion JPEG on FFmpeg's standard output ([`Plan::pipes`]).
    pipe: bool,
}

impl std::fmt::Debug for Plan {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let args: Vec<String> = self.args.iter().map(|arg| oxrtc_core::spec::redact(arg)).collect();
        f.debug_struct("Plan")
            .field("args", &args)
            .field("declared", &self.declared)
            .field("stall_timeout", &self.stall_timeout)
            .finish_non_exhaustive()
    }
}

impl Plan {
    /// The arguments (after the binary), publishing to `output`, reading with no
    /// credential of the run's: what the tests compare to go2rtc's.
    #[cfg(test)]
    pub(crate) fn args(&self, output: &str) -> Vec<String> {
        self.args_reading_as(output, None, true)
    }

    /// The same, reading the relay as `credential` (user name, password) when
    /// the input is the relay's and says no user of its own: the run's own
    /// credential, never the operator's. A camera's URL is never touched.
    ///
    /// `skip_rate_guess`: FFmpeg reads the rate the relay says, so a stream
    /// of the relay read by name starts with [`RELAY_INPUT`].
    pub(crate) fn args_reading_as(
        &self,
        output: &str,
        credential: Option<(&str, &str)>,
        skip_rate_guess: bool,
    ) -> Vec<String> {
        let mut args = self.args.clone();
        for &slot in &self.output_slots {
            if let Some(arg) = args.get_mut(slot) {
                *arg = arg.replace(OUTPUT, output);
            }
        }
        if let (Some(url), Some((user, password))) = (&self.relay_input, credential)
            && let Some(rest) = url.strip_prefix("rtsp://")
        {
            let with = format!("rtsp://{user}:{password}@{rest}");
            for arg in &mut args {
                *arg = arg.replace(url.as_str(), &with);
            }
        }
        if let Some(at) = self.rate_guess_at.filter(|_| skip_rate_guess)
            && at <= args.len()
        {
            let flags = RELAY_INPUT.split_whitespace().map(str::to_owned);
            args.splice(at..at, flags);
        }
        args
    }

    /// What the fragments say the process will produce.
    pub(crate) fn local(&self) -> bool {
        self.local
    }

    pub(crate) fn declared(&self) -> Option<Declared> {
        self.declared.clone()
    }

    /// `#timeout=N`, which also bounds how long FFmpeg itself waits on its input.
    pub(crate) fn stall_timeout(&self) -> Option<Duration> {
        self.stall_timeout
    }

    /// Whether FFmpeg decodes and encodes: a `#video=` or `#audio=` other than
    /// `copy`, a `#raw=` or a filter (`#width=`, `#rotate=`). One that only
    /// copies (`#video=copy`, or no codec at all: `-c copy`) is a camera read
    /// through FFmpeg, which keeps a camera's idle grace (D36).
    pub(crate) fn transcodes(&self) -> bool {
        self.transcodes
    }

    /// Whether the pictures come on FFmpeg's standard output, one JPEG after
    /// the other (`#video=mjpeg` alone: go2rtc's `output/mjpeg`, `-f mjpeg -`),
    /// rather than published to the RTSP server: RTP/JPEG (RFC 2435) carries
    /// no side over 2040 pixels and only the standard Huffman tables, which
    /// FFmpeg's encoder does not write by default (D46).
    pub(crate) fn pipes(&self) -> bool {
        self.pipe
    }
}

fn invalid(message: impl Into<String>) -> SpecError {
    SpecError::Invalid(message.into())
}

/// A value from the source, safe to quote in an error.
fn shown(value: &str) -> String {
    oxrtc_core::spec::redact(&value.chars().take(48).collect::<String>())
}

/// The `#key=value` fragments, in order.
struct Query<'a>(Vec<(&'a str, &'a str)>);

impl<'a> Query<'a> {
    fn all(&self, key: &str) -> Vec<&'a str> {
        self.0.iter().filter(|(k, _)| *k == key).map(|(_, v)| *v).collect()
    }

    fn first(&self, key: &str) -> Option<&'a str> {
        self.0.iter().find(|(k, _)| *k == key).map(|(_, v)| *v)
    }

    fn has(&self, key: &str) -> bool {
        self.0.iter().any(|(k, _)| *k == key)
    }
}

/// Builds the plan of `spec` for the stream `owner`, reading stream names
/// through the RTSP server at `relay`.
pub(crate) fn plan(spec: &SourceSpec, templates: &Templates, relay: &str, owner: &str) -> Result<Plan, SpecError> {
    let query = Query(spec.fragments().collect());
    for key in ["hardware", "drawtext"] {
        if query.has(key) {
            return Err(invalid(format!("ffmpeg: #{key} is not supported")));
        }
    }
    for key in ["video", "audio", "raw", "input", "width", "height", "timeout", "bitrate", "rotate", "query"] {
        if query.all(key).iter().any(|v| v.is_empty()) {
            return Err(invalid(format!("ffmpeg: #{key} needs a value")));
        }
    }
    let videos = query.all("video");
    let audios = query.all("audio");
    let family = |value: &str| value.split('/').next().unwrap_or_default().to_owned();
    let pipe = videos.len() == 1 && audios.is_empty() && videos.iter().all(|v| family(v) == "mjpeg");
    if !pipe && videos.iter().any(|v| family(v) == "mjpeg") {
        // go2rtc publishes it over RTSP then, which FFmpeg's RTP/JPEG refuses
        // for the tables its encoder writes: refused here, said plainly.
        return Err(invalid("ffmpeg: #video=mjpeg is supported alone, with no other #video and no #audio"));
    }
    let given_timeout = query.first("timeout").map(parse_timeout).transpose()?;
    let timeout = given_timeout.unwrap_or_else(|| templates.timeout_secs());

    let (mut input, input_url) = build_input(spec, &query, templates, relay, owner, timeout, &videos, &audios)?;
    let local = crate::relay::reads_relay(&input_url, relay);
    if query.has("async") {
        input.insert_str(0, "-use_wallclock_as_timestamps 1 -async 1 ");
    }

    let mut codecs: Vec<String> = Vec::new();
    for raw in query.all("raw") {
        codecs.push(named(templates, raw)?.to_owned());
    }
    let mut filters: Vec<String> = Vec::new();
    let (width, height) = (query.first("width"), query.first("height"));
    if width.is_some() || height.is_some() {
        let side = |v: Option<&str>| v.map_or(Ok("-1".to_owned()), parse_dimension);
        filters.push(format!("scale={}:{}", side(width)?, side(height)?));
    }
    if let Some(rotate) = query.first("rotate") {
        match rotate {
            "90" => filters.push("transpose=1".into()),
            "180" => filters.push("transpose=1,transpose=1".into()),
            "-90" | "270" => filters.push("transpose=2".into()),
            _ => {}
        }
    }
    for video in &videos {
        codecs.push(codec_args(templates, MediaKind::Video, video)?);
    }
    if let Some(bitrate) = query.first("bitrate") {
        let b = parse_bitrate(bitrate)?;
        codecs.push(format!("-b:v {b} -maxrate {b} -bufsize {b}"));
    }
    for audio in &audios {
        codecs.push(codec_args(templates, MediaKind::Audio, audio)?);
    }
    match (videos.is_empty(), audios.is_empty()) {
        (true, true) => codecs.push("-c copy".into()),
        (true, false) => codecs.push("-vn".into()),
        (false, true) => codecs.push("-an".into()),
        (false, false) => {}
    }
    if videos.len() > 1 || audios.len() > 1 {
        map_each_codec(&mut codecs);
    }
    let transcodes =
        query.has("raw") || !filters.is_empty() || videos.iter().chain(&audios).any(|value| *value != "copy");

    // `rtsp://127.0.0.1:8554/x` names no user: the authority ends at the `/`.
    let relay_input = local.then(|| input_url.clone());
    let mut args = split(templates.get("global").unwrap_or_default())?;
    let input_start = args.len();
    // Only a stream read by name: its URL says `source=ffmpeg:`, and the
    // relay holds the answer of the reader that says so until it knows the
    // pace to announce. A relay URL the operator wrote says nothing of the
    // kind, is answered at once, and keeps FFmpeg's guess.
    let rate_guess_at = (local && validate_name(spec.target()).is_ok()).then_some(input_start);
    args.extend(split(&input)?);
    for arg in args.iter_mut().skip(input_start) {
        *arg = arg.replace(INPUT, &input_url);
    }
    for codec in &codecs {
        args.extend(split(codec)?);
    }
    if !filters.is_empty() {
        args.push("-vf".into());
        args.push(filters.join(","));
    }
    let output = if pipe { "output/mjpeg" } else { "output" };
    let output_args = split(templates.get(output).ok_or_else(|| invalid(format!("ffmpeg: no {output} template")))?)?;
    let threads = crate::x264::threads_unless_said(&args, &output_args);
    args.extend(threads);
    let output_start = args.len();
    args.extend(output_args);
    let output_slots: Vec<usize> =
        (output_start..args.len()).filter(|&i| args.get(i).is_some_and(|a| a.contains(OUTPUT))).collect();
    if output_slots.is_empty() && !pipe {
        return Err(invalid("ffmpeg: the output template has no {output}"));
    }
    Ok(Plan {
        args,
        output_slots,
        declared: declared(&videos, &audios),
        stall_timeout: given_timeout.map(|t| Duration::from_secs(u64::from(t))),
        local,
        relay_input,
        rate_guess_at,
        transcodes,
        pipe,
    })
}

/// The input part and the URL that replaces `{input}` in it.
#[allow(clippy::too_many_arguments)]
fn build_input(
    spec: &SourceSpec,
    query: &Query<'_>,
    templates: &Templates,
    relay: &str,
    owner: &str,
    timeout: u32,
    videos: &[&str],
    audios: &[&str],
) -> Result<(String, String), SpecError> {
    let base = spec.target();
    let template = |default: &str| -> Result<String, SpecError> {
        let text = match query.first("input") {
            Some(name) => named(templates, name)?,
            None => templates.get(default).ok_or_else(|| invalid(format!("ffmpeg: no {default} input template")))?,
        };
        Ok(text.replacen("{timeout}", &format!("{timeout}000000"), 1))
    };
    if let Some((scheme, _)) = base.split_once("://").filter(|(s, _)| !s.is_empty()) {
        return match scheme.to_ascii_lowercase().as_str() {
            "http" | "https" => Ok((template("http")?, base.to_owned())),
            "rtsp" | "rtsps" => {
                let allowed = match (videos.is_empty(), audios.is_empty()) {
                    (false, true) => "video",
                    (true, false) => "audio",
                    _ => "video+audio",
                };
                Ok((format!("-allowed_media_types {allowed} {}", template("rtsp")?), base.to_owned()))
            }
            other => Err(invalid(format!("ffmpeg: input scheme {:?} is not supported", shown(other)))),
        };
    }
    if validate_name(base).is_err() {
        return Err(invalid("ffmpeg: the input must be a stream name or an rtsp(s)/http(s) URL"));
    }
    if base == owner {
        return Err(SpecError::SelfReference);
    }
    let want = match (videos.is_empty(), audios.is_empty()) {
        (false, true) => "video",
        (true, false) => "audio",
        _ => "video&audio",
    };
    let source = spec.expose().split_once(':').map_or("", |(_, rest)| rest);
    let mut url = format!("rtsp://{relay}/{base}?{want}&source=ffmpeg:{}", query_escape(source));
    let extras = query.all("query");
    if shows_its_output(videos, audios) && !extras.iter().any(|extra| names_a_class(extra)) {
        url.push_str(&format!("&{QUEUE_QUERY_KEY}={}", ConsumerClass::Live.as_str()));
    }
    for extra in extras {
        url.push('&');
        url.push_str(extra);
    }
    Ok((template("rtsp")?, url))
}

/// Whether a transcode makes a picture and nothing else: its output is what
/// WebRTC viewers watch, so when it falls behind (a loaded CPU) it must be cut
/// to a keyframe after the live budget, not left up to the recorder's 10 s
/// behind (D18). One that makes sound is recorded by the NVR (openIVR's
/// `camera_N_rec`), and a sound cut short is a hole in a file: it keeps the
/// recorder class, as does a source that only repackages everything.
fn shows_its_output(videos: &[&str], audios: &[&str]) -> bool {
    !videos.is_empty() && audios.is_empty()
}

/// Whether an operator's `#query=` already says which class the input is
/// read as (`queue=live` or `queue=recorder`, even a value the server will
/// refuse): it wins, and a second key beside it would be refused by the
/// server when the two differ. The server's own parser answers, so the key's
/// name and what counts as one are written once.
fn names_a_class(extra: &str) -> bool {
    !matches!(ConsumerClass::from_query(extra), Ok(None))
}

/// The template a source names, which must exist (D6).
fn named<'t>(templates: &'t Templates, name: &str) -> Result<&'t str, SpecError> {
    templates.get(name).ok_or_else(|| invalid(format!("ffmpeg: no template named {:?}", shown(name))))
}

/// The encoder arguments of `#video=value` / `#audio=value`.
fn codec_args(templates: &Templates, kind: MediaKind, value: &str) -> Result<String, SpecError> {
    if value == "copy" {
        return Ok(format!("-c:{} copy", if kind == MediaKind::Video { 'v' } else { 'a' }));
    }
    let family = value.split('/').next().unwrap_or_default();
    // go2rtc sends these to a pipe in formats oxrtc does not read (Motion
    // JPEG it reads: [`Plan::pipes`]); RTP cannot
    // carry little-endian PCM or MP3 here, and `auto` needs a negotiation.
    if matches!(family, "raw" | "pcml" | "mp3" | "auto") || (family == "mjpeg" && kind == MediaKind::Audio) {
        return Err(invalid(format!("ffmpeg: #{}={} is not supported", kind.as_str(), shown(value))));
    }
    if let Some(codec) = Codec::parse(family)
        && codec.kind() != kind
    {
        return Err(invalid(format!("ffmpeg: {} is not a {} codec", shown(value), kind.as_str())));
    }
    Ok(named(templates, value)?.to_owned())
}

/// go2rtc's multi-codec mode: each `-c:v`/`-c:a` maps the first input track
/// of its kind to a numbered output stream.
fn map_each_codec(codecs: &mut [String]) {
    let (mut video, mut audio) = (0, 0);
    for codec in codecs.iter_mut() {
        if codec.starts_with("-c:v ") {
            *codec = format!("-map 0:v:0? {}", codec.replace(":v ", &format!(":v:{video} ")));
            video += 1;
        } else if codec.starts_with("-c:a ") {
            *codec = format!("-map 0:a:0? {}", codec.replace(":a ", &format!(":a:{audio} ")));
            audio += 1;
        }
    }
}

/// What the fragments declare: every `#video=`/`#audio=` must name a known
/// codec, or nothing is declared at all. A partial list would make the core
/// answer "no" for the kind left out (a `copy`), where the truth is "unknown".
fn declared(videos: &[&str], audios: &[&str]) -> Option<Declared> {
    if videos.is_empty() && audios.is_empty() {
        return None;
    }
    let mut out = Declared::new();
    for (kind, values) in [(MediaKind::Video, videos), (MediaKind::Audio, audios)] {
        for value in values {
            let codec = Codec::parse(value.split('/').next().unwrap_or_default()).filter(|c| c.kind() == kind)?;
            if !out.contains(&(kind, codec)) {
                out.push((kind, codec));
            }
        }
    }
    Some(out)
}

fn split(text: &str) -> Result<Vec<String>, SpecError> {
    quote_split(text).map_err(|e| invalid(format!("ffmpeg: template: {e}")))
}

fn parse_timeout(value: &str) -> Result<u32, SpecError> {
    timeout_secs(value)
        .and_then(|secs| u32::try_from(secs).ok())
        .ok_or_else(|| invalid(format!("ffmpeg: #timeout must be 1 to {MAX_TIMEOUT_SECS} seconds")))
}

fn parse_dimension(value: &str) -> Result<String, SpecError> {
    value
        .parse::<i32>()
        .ok()
        .filter(|d| (-2..=MAX_DIMENSION).contains(d) && *d != 0)
        .map(|d| d.to_string())
        .ok_or_else(|| invalid(format!("ffmpeg: a width or height must be 1 to {MAX_DIMENSION}, or -1")))
}

fn parse_bitrate(value: &str) -> Result<&str, SpecError> {
    let digits = value.strip_suffix(['k', 'K', 'm', 'M']).unwrap_or(value);
    let valid = !digits.is_empty() && digits.len() <= 10 && digits.bytes().all(|b| b.is_ascii_digit());
    if valid { Ok(value) } else { Err(invalid("ffmpeg: #bitrate must be a number, with an optional k or M")) }
}

/// Go's `url.QueryEscape`: unreserved bytes kept, space as `+`, the rest `%XX`.
pub(crate) fn query_escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len() * 3);
    for byte in text.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(char::from(byte)),
            b' ' => out.push('+'),
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

#[cfg(test)]
#[path = "args_credential_tests.rs"]
mod credential_tests;
#[cfg(test)]
#[path = "args_tests.rs"]
mod tests;
