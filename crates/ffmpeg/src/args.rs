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
use oxrtc_core::{Codec, Declared, MediaKind, SourceSpec, SpecError};

use crate::split::quote_split;
use crate::templates::Templates;

/// Placeholder of the input URL in input templates.
const INPUT: &str = "{input}";
/// Placeholder of the publication URL in the output template.
const OUTPUT: &str = "{output}";
/// Longest `#timeout=`, in seconds.
const MAX_TIMEOUT_SECS: u32 = 3600;
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
    /// The arguments (after the binary), publishing to `output`.
    pub(crate) fn args(&self, output: &str) -> Vec<String> {
        let mut args = self.args.clone();
        for &slot in &self.output_slots {
            if let Some(arg) = args.get_mut(slot) {
                *arg = arg.replace(OUTPUT, output);
            }
        }
        args
    }

    /// What the fragments say the process will produce.
    pub(crate) fn declared(&self) -> Option<Declared> {
        self.declared.clone()
    }

    /// `#timeout=N`, which also bounds how long FFmpeg itself waits on its input.
    pub(crate) fn stall_timeout(&self) -> Option<Duration> {
        self.stall_timeout
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
    let given_timeout = query.first("timeout").map(parse_timeout).transpose()?;
    let timeout = given_timeout.unwrap_or_else(|| templates.timeout_secs());

    let (mut input, input_url) = build_input(spec, &query, templates, relay, owner, timeout, &videos, &audios)?;
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

    let mut args = split(templates.get("global").unwrap_or_default())?;
    let input_start = args.len();
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
    let output_start = args.len();
    args.extend(split(templates.get("output").ok_or_else(|| invalid("ffmpeg: no output template"))?)?);
    let output_slots: Vec<usize> =
        (output_start..args.len()).filter(|&i| args.get(i).is_some_and(|a| a.contains(OUTPUT))).collect();
    if output_slots.is_empty() {
        return Err(invalid("ffmpeg: the output template has no {output}"));
    }
    Ok(Plan {
        args,
        output_slots,
        declared: declared(&videos, &audios),
        stall_timeout: given_timeout.map(|t| Duration::from_secs(u64::from(t))),
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
    for extra in query.all("query") {
        url.push('&');
        url.push_str(extra);
    }
    Ok((template("rtsp")?, url))
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
    // go2rtc sends these to a pipe in formats oxrtc does not read; RTP cannot
    // carry little-endian PCM or MP3 here, and `auto` needs a negotiation.
    if matches!(family, "mjpeg" | "raw" | "pcml" | "mp3" | "auto") {
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
    value
        .parse::<u32>()
        .ok()
        .filter(|t| (1..=MAX_TIMEOUT_SECS).contains(t))
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
#[path = "args_tests.rs"]
mod tests;
