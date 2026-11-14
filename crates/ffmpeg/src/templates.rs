//! Named argument templates: go2rtc 1.9.15's built-in defaults
//! (`internal/ffmpeg/ffmpeg.go`), overridden by the configuration file's
//! `ffmpeg:` section.
//!
//! Only the operator writes templates (D6): a source may *name* one
//! (`#video=h264`, `#input=dedup_input`, `#raw=my_flags`), never supply one.

use std::collections::BTreeMap;

/// go2rtc's defaults, verbatim, minus `bin` (a separate setting here) and the
/// hardware encoders (`#hardware=` is not supported).
const DEFAULTS: &[(&str, &str)] = &[
    ("global", "-hide_banner"),
    ("timeout", "5"),
    // inputs
    ("file", "-re -i {input}"),
    ("http", "-fflags nobuffer -flags low_delay -i {input}"),
    (
        "rtsp",
        "-fflags nobuffer -flags low_delay -timeout {timeout} -user_agent go2rtc/ffmpeg -rtsp_flags prefer_tcp -i {input}",
    ),
    ("rtsp/udp", "-fflags nobuffer -flags low_delay -timeout {timeout} -user_agent go2rtc/ffmpeg -i {input}"),
    // output
    ("output", "-user_agent ffmpeg/go2rtc -rtsp_transport tcp -f rtsp {output}"),
    ("output/mjpeg", "-f mjpeg -"),
    ("output/raw", "-f yuv4mpegpipe -"),
    ("output/aac", "-f adts -"),
    ("output/wav", "-f wav -"),
    // video
    (
        "h264",
        "-c:v libx264 -g 50 -profile:v high -level:v 4.1 -preset:v superfast -tune:v zerolatency -pix_fmt:v yuv420p",
    ),
    (
        "h265",
        "-c:v libx265 -g 50 -profile:v main -x265-params level=5.1:high-tier=0 -preset:v superfast -tune:v zerolatency -pix_fmt:v yuv420p",
    ),
    ("mjpeg", "-c:v mjpeg"),
    ("raw", "-c:v rawvideo"),
    ("raw/gray8", "-c:v rawvideo -pix_fmt:v gray8"),
    ("raw/yuv420p", "-c:v rawvideo -pix_fmt:v yuv420p"),
    ("raw/yuv422p", "-c:v rawvideo -pix_fmt:v yuv422p"),
    ("raw/yuv444p", "-c:v rawvideo -pix_fmt:v yuv444p"),
    // audio
    ("opus", "-c:a libopus -application:a lowdelay -min_comp 0"),
    ("opus/16000", "-c:a libopus -application:a lowdelay -min_comp 0 -ar:a 16000 -ac:a 1"),
    ("pcmu", "-c:a pcm_mulaw -ar:a 8000 -ac:a 1"),
    ("pcmu/8000", "-c:a pcm_mulaw -ar:a 8000 -ac:a 1"),
    ("pcmu/16000", "-c:a pcm_mulaw -ar:a 16000 -ac:a 1"),
    ("pcmu/48000", "-c:a pcm_mulaw -ar:a 48000 -ac:a 1"),
    ("pcma", "-c:a pcm_alaw -ar:a 8000 -ac:a 1"),
    ("pcma/8000", "-c:a pcm_alaw -ar:a 8000 -ac:a 1"),
    ("pcma/16000", "-c:a pcm_alaw -ar:a 16000 -ac:a 1"),
    ("pcma/48000", "-c:a pcm_alaw -ar:a 48000 -ac:a 1"),
    ("aac", "-c:a aac"),
    ("aac/16000", "-c:a aac -ar:a 16000 -ac:a 1"),
    ("mp3", "-c:a libmp3lame -q:a 8"),
    ("pcm", "-c:a pcm_s16be -ar:a 8000 -ac:a 1"),
    ("pcm/8000", "-c:a pcm_s16be -ar:a 8000 -ac:a 1"),
    ("pcm/16000", "-c:a pcm_s16be -ar:a 16000 -ac:a 1"),
    ("pcm/48000", "-c:a pcm_s16be -ar:a 48000 -ac:a 1"),
    ("pcml", "-c:a pcm_s16le -ar:a 8000 -ac:a 1"),
    ("pcml/8000", "-c:a pcm_s16le -ar:a 8000 -ac:a 1"),
    ("pcml/16000", "-c:a pcm_s16le -ar:a 16000 -ac:a 1"),
    ("pcml/44100", "-c:a pcm_s16le -ar:a 44100 -ac:a 1"),
];

/// FFmpeg's log level, as go2rtc sets it by default (`log.ffmpeg: error`):
/// appended to the global template, overridden or not.
const LOG_LEVEL: &str = " -v error";

/// Default value of `{timeout}`, in seconds, when the `timeout` template does
/// not hold a usable number.
const DEFAULT_TIMEOUT_SECS: u32 = 5;

/// The templates in effect: the defaults, then the configuration on top.
#[derive(Debug, Clone)]
pub(crate) struct Templates {
    map: BTreeMap<String, String>,
}

impl Templates {
    /// The defaults overridden by `config` (the `ffmpeg:` section minus `bin`).
    pub(crate) fn new(config: &BTreeMap<String, String>) -> Self {
        let mut map: BTreeMap<String, String> =
            DEFAULTS.iter().map(|(name, text)| ((*name).to_owned(), (*text).to_owned())).collect();
        for (name, text) in config {
            if name != "bin" {
                map.insert(name.clone(), text.clone());
            }
        }
        if let Some(global) = map.get_mut("global") {
            global.push_str(LOG_LEVEL);
        }
        Self { map }
    }

    /// A template by name; an empty one counts as absent (go2rtc's rule).
    pub(crate) fn get(&self, name: &str) -> Option<&str> {
        self.map.get(name).map(String::as_str).filter(|text| !text.is_empty())
    }

    /// The default socket timeout of the RTSP inputs, in seconds.
    pub(crate) fn timeout_secs(&self) -> u32 {
        self.get("timeout").and_then(|t| t.trim().parse().ok()).filter(|&t| t > 0).unwrap_or(DEFAULT_TIMEOUT_SECS)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn configuration_overrides_defaults_and_bin_is_not_a_template() {
        let config: BTreeMap<String, String> = [
            ("h264", "-codec:v libx264 -g:v 30"),
            ("dedup_input", "-probesize 32 -i {input}"),
            ("bin", "/usr/bin/ffmpeg"),
            ("opus", ""),
        ]
        .iter()
        .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
        .collect();
        let t = Templates::new(&config);
        assert_eq!(t.get("h264"), Some("-codec:v libx264 -g:v 30"));
        assert_eq!(t.get("dedup_input"), Some("-probesize 32 -i {input}"));
        assert_eq!(t.get("bin"), None);
        assert_eq!(t.get("opus"), None, "an empty template is no template");
        assert_eq!(t.get("pcma/16000"), Some("-c:a pcm_alaw -ar:a 16000 -ac:a 1"));
        assert_eq!(t.get("global"), Some("-hide_banner -v error"));
    }

    #[test]
    fn timeout_falls_back_to_five_seconds() {
        assert_eq!(Templates::new(&BTreeMap::new()).timeout_secs(), 5);
        let odd: BTreeMap<String, String> = [("timeout".to_owned(), "soon".to_owned())].into();
        assert_eq!(Templates::new(&odd).timeout_secs(), 5);
        let ten: BTreeMap<String, String> = [("timeout".to_owned(), "10".to_owned())].into();
        assert_eq!(Templates::new(&ten).timeout_secs(), 10);
    }
}
