//! The argument vectors, against go2rtc 1.9.15's own command lines.
//!
//! Each expected line is what go2rtc's `parseArgs(...).String()` produces with
//! its default configuration (whose `log.ffmpeg: error` appends `-v error` to
//! the global template), split by go2rtc's `QuoteSplit` - which is what it
//! hands to `exec`. The openIVR cases come from docs/COMPATIBILITY.md; the
//! others from go2rtc's `internal/ffmpeg/ffmpeg_test.go`.

use std::collections::BTreeMap;

use super::*;

const RELAY: &str = "127.0.0.1:8554";

/// openIVR's `ffmpeg:` section (docs/COMPATIBILITY.md).
const OPENIVR: &[(&str, &str)] = &[
    (
        "h264",
        "-codec:v libx264 -g:v 30 -preset:v superfast -tune:v zerolatency -profile:v baseline -level:v 5.1 -pix_fmt:v yuv420p -fps_mode:v passthrough",
    ),
    ("dedup_input", "-fflags nobuffer -flags low_delay -probesize 32 -i {input}"),
];

fn templates(config: &[(&str, &str)]) -> Templates {
    let map: BTreeMap<String, String> = config.iter().map(|(k, v)| ((*k).to_owned(), (*v).to_owned())).collect();
    Templates::new(&map)
}

fn plan_of(spec: &str, config: &[(&str, &str)]) -> Result<Plan, SpecError> {
    plan(&SourceSpec::parse(spec).unwrap(), &templates(config), RELAY, "camera_1")
}

fn ours(spec: &str, config: &[(&str, &str)]) -> Vec<String> {
    plan_of(spec, config).unwrap().args(OUTPUT)
}

fn go2rtc(line: &str) -> Vec<String> {
    quote_split(line.strip_prefix("ffmpeg ").unwrap()).unwrap()
}

fn refused(spec: &str) -> String {
    match plan_of(spec, OPENIVR) {
        Err(SpecError::Invalid(why)) => why,
        other => panic!("{spec} was not refused: {other:?}"),
    }
}

#[test]
fn openivr_transcode_of_a_stream_by_name() {
    let spec = "ffmpeg:camera_1_src#video=h264#width=1920#height=1080";
    assert_eq!(
        ours(spec, OPENIVR),
        go2rtc(
            "ffmpeg -hide_banner -v error -fflags nobuffer -flags low_delay -timeout 5000000 -user_agent go2rtc/ffmpeg \
             -rtsp_flags prefer_tcp -i rtsp://127.0.0.1:8554/camera_1_src?video&source=ffmpeg:camera_1_src%23video%3Dh264%23width%3D1920%23height%3D1080 \
             -codec:v libx264 -g:v 30 -preset:v superfast -tune:v zerolatency -profile:v baseline -level:v 5.1 -pix_fmt:v yuv420p -fps_mode:v passthrough \
             -an -vf \"scale=1920:1080\" -user_agent ffmpeg/go2rtc -rtsp_transport tcp -f rtsp {output}"
        )
    );
    // The same with go2rtc's own h264 template.
    assert_eq!(
        ours(spec, &[]),
        go2rtc(
            "ffmpeg -hide_banner -v error -fflags nobuffer -flags low_delay -timeout 5000000 -user_agent go2rtc/ffmpeg \
             -rtsp_flags prefer_tcp -i rtsp://127.0.0.1:8554/camera_1_src?video&source=ffmpeg:camera_1_src%23video%3Dh264%23width%3D1920%23height%3D1080 \
             -c:v libx264 -g 50 -profile:v high -level:v 4.1 -preset:v superfast -tune:v zerolatency -pix_fmt:v yuv420p \
             -an -vf \"scale=1920:1080\" -user_agent ffmpeg/go2rtc -rtsp_transport tcp -f rtsp {output}"
        )
    );
}

#[test]
fn openivr_audio_transcode_of_an_rtsp_url() {
    assert_eq!(
        ours("ffmpeg:rtsp://127.0.0.1:8554/camera_1_src#audio=opus#timeout=20", OPENIVR),
        go2rtc(
            "ffmpeg -hide_banner -v error -allowed_media_types audio -fflags nobuffer -flags low_delay -timeout 20000000 \
             -user_agent go2rtc/ffmpeg -rtsp_flags prefer_tcp -i rtsp://127.0.0.1:8554/camera_1_src \
             -c:a libopus -application:a lowdelay -min_comp 0 -vn -user_agent ffmpeg/go2rtc -rtsp_transport tcp -f rtsp {output}"
        )
    );
}

#[test]
fn openivr_audio_transcode_of_an_http_url_with_a_named_input() {
    assert_eq!(
        ours("ffmpeg:http://127.0.0.1:8080/camera_1_src#audio=opus#input=dedup_input", OPENIVR),
        go2rtc(
            "ffmpeg -hide_banner -v error -fflags nobuffer -flags low_delay -probesize 32 -i http://127.0.0.1:8080/camera_1_src \
             -c:a libopus -application:a lowdelay -min_comp 0 -vn -user_agent ffmpeg/go2rtc -rtsp_transport tcp -f rtsp {output}"
        )
    );
}

#[test]
fn go2rtc_ip_camera_cases() {
    let cases = [
        (
            "ffmpeg:http://example.com",
            "ffmpeg -hide_banner -v error -fflags nobuffer -flags low_delay -i http://example.com -c copy -user_agent ffmpeg/go2rtc -rtsp_transport tcp -f rtsp {output}",
        ),
        (
            "ffmpeg:http://example.com#video=h264",
            "ffmpeg -hide_banner -v error -fflags nobuffer -flags low_delay -i http://example.com -c:v libx264 -g 50 -profile:v high -level:v 4.1 -preset:v superfast -tune:v zerolatency -pix_fmt:v yuv420p -an -user_agent ffmpeg/go2rtc -rtsp_transport tcp -f rtsp {output}",
        ),
        (
            "ffmpeg:https://example.com#video=copy",
            "ffmpeg -hide_banner -v error -fflags nobuffer -flags low_delay -i https://example.com -c:v copy -an -user_agent ffmpeg/go2rtc -rtsp_transport tcp -f rtsp {output}",
        ),
        (
            "ffmpeg:rtsp://example.com",
            "ffmpeg -hide_banner -v error -allowed_media_types video+audio -fflags nobuffer -flags low_delay -timeout 5000000 -user_agent go2rtc/ffmpeg -rtsp_flags prefer_tcp -i rtsp://example.com -c copy -user_agent ffmpeg/go2rtc -rtsp_transport tcp -f rtsp {output}",
        ),
        // go2rtc's test still expects `-level:v 5.1` here; its 1.9.15 default
        // template (the one it runs) uses `-x265-params`.
        (
            "ffmpeg:rtsp://example.com#video=h265#width=1280#height=720",
            "ffmpeg -hide_banner -v error -allowed_media_types video -fflags nobuffer -flags low_delay -timeout 5000000 -user_agent go2rtc/ffmpeg -rtsp_flags prefer_tcp -i rtsp://example.com -c:v libx265 -g 50 -profile:v main -x265-params level=5.1:high-tier=0 -preset:v superfast -tune:v zerolatency -pix_fmt:v yuv420p -an -vf \"scale=1280:720\" -user_agent ffmpeg/go2rtc -rtsp_transport tcp -f rtsp {output}",
        ),
        (
            "ffmpeg:rtsp://example.com#input=rtsp/udp",
            "ffmpeg -hide_banner -v error -allowed_media_types video+audio -fflags nobuffer -flags low_delay -timeout 5000000 -user_agent go2rtc/ffmpeg -i rtsp://example.com -c copy -user_agent ffmpeg/go2rtc -rtsp_transport tcp -f rtsp {output}",
        ),
        (
            "ffmpeg:rtsp://example.com#timeout=10",
            "ffmpeg -hide_banner -v error -allowed_media_types video+audio -fflags nobuffer -flags low_delay -timeout 10000000 -user_agent go2rtc/ffmpeg -rtsp_flags prefer_tcp -i rtsp://example.com -c copy -user_agent ffmpeg/go2rtc -rtsp_transport tcp -f rtsp {output}",
        ),
        (
            "ffmpeg:rtsp://example.com#audio=opus",
            "ffmpeg -hide_banner -v error -allowed_media_types audio -fflags nobuffer -flags low_delay -timeout 5000000 -user_agent go2rtc/ffmpeg -rtsp_flags prefer_tcp -i rtsp://example.com -c:a libopus -application:a lowdelay -min_comp 0 -vn -user_agent ffmpeg/go2rtc -rtsp_transport tcp -f rtsp {output}",
        ),
        (
            "ffmpeg:rtsp://example.com#video=h264#rotate=-90",
            "ffmpeg -hide_banner -v error -allowed_media_types video -fflags nobuffer -flags low_delay -timeout 5000000 -user_agent go2rtc/ffmpeg -rtsp_flags prefer_tcp -i rtsp://example.com -c:v libx264 -g 50 -profile:v high -level:v 4.1 -preset:v superfast -tune:v zerolatency -pix_fmt:v yuv420p -an -vf \"transpose=2\" -user_agent ffmpeg/go2rtc -rtsp_transport tcp -f rtsp {output}",
        ),
        (
            "ffmpeg:rtsp://example.com#video=copy#audio=pcmu",
            "ffmpeg -hide_banner -v error -allowed_media_types video+audio -fflags nobuffer -flags low_delay -timeout 5000000 -user_agent go2rtc/ffmpeg -rtsp_flags prefer_tcp -i rtsp://example.com -c:v copy -c:a pcm_mulaw -ar:a 8000 -ac:a 1 -user_agent ffmpeg/go2rtc -rtsp_transport tcp -f rtsp {output}",
        ),
    ];
    for (spec, line) in cases {
        assert_eq!(ours(spec, &[]), go2rtc(line), "{spec}");
    }
}

#[test]
fn several_codecs_of_a_kind_map_each_to_its_own_output() {
    // Args.String's multimode, applied by hand to go2rtc's templates: its
    // ReplaceAll numbers every `:a ` of the codec, `-ar:a` included.
    assert_eq!(
        ours("ffmpeg:rtsp://example.com#video=copy#audio=pcma#audio=opus", &[]),
        go2rtc(
            "ffmpeg -hide_banner -v error -allowed_media_types video+audio -fflags nobuffer -flags low_delay -timeout 5000000 \
             -user_agent go2rtc/ffmpeg -rtsp_flags prefer_tcp -i rtsp://example.com -map 0:v:0? -c:v:0 copy \
             -map 0:a:0? -c:a:0 pcm_alaw -ar:a:0 8000 -ac:a:0 1 -map 0:a:0? -c:a:1 libopus -application:a:1 lowdelay -min_comp 0 \
             -user_agent ffmpeg/go2rtc -rtsp_transport tcp -f rtsp {output}"
        )
    );
}

#[test]
fn stream_names_honour_query_and_async() {
    let args = ours("ffmpeg:camera_1_src#audio=opus#query=x=1#async", &[]);
    let at = args.iter().position(|a| a == "-i").unwrap();
    assert_eq!(args[..4], ["-hide_banner", "-v", "error", "-use_wallclock_as_timestamps"]);
    assert_eq!(
        args[at + 1],
        "rtsp://127.0.0.1:8554/camera_1_src?audio&source=ffmpeg:camera_1_src%23audio%3Dopus%23query%3Dx%3D1%23async&x=1"
    );
}

#[test]
fn input_and_output_are_single_arguments() {
    let plan = plan_of("ffmpeg:rtsp://cam/a b/{output}#audio=opus", &[]).unwrap();
    let args = plan.args("rtsp://127.0.0.1:1/_publish/t");
    let at = args.iter().position(|a| a == "-i").unwrap();
    assert_eq!(args[at + 1], "rtsp://cam/a b/{output}", "the input is one argument, never rewritten");
    assert_eq!(args.last().unwrap(), "rtsp://127.0.0.1:1/_publish/t");
    // A quoted placeholder in a template is one argument too.
    let quoted = [("spaced", "-f lavfi -i \"{input}\"")];
    let args = ours("ffmpeg:http://cam/x y#video=h264#input=spaced", &quoted);
    let at = args.iter().position(|a| a == "-i").unwrap();
    assert_eq!(args[at + 1], "http://cam/x y");
}

#[test]
fn declared_media_follow_the_fragments() {
    let declared = |spec: &str| plan_of(spec, OPENIVR).unwrap().declared();
    assert_eq!(declared("ffmpeg:camera_1_src#video=h264#width=1920"), Some(vec![(MediaKind::Video, Codec::H264)]));
    assert_eq!(declared("ffmpeg:rtsp://cam/x#audio=opus#timeout=20"), Some(vec![(MediaKind::Audio, Codec::Opus)]));
    assert_eq!(
        declared("ffmpeg:rtsp://cam/x#video=h265#audio=pcma/16000"),
        Some(vec![(MediaKind::Video, Codec::H265), (MediaKind::Audio, Codec::Pcma)])
    );
    assert_eq!(declared("ffmpeg:rtsp://cam/x#audio=pcm/8000"), Some(vec![(MediaKind::Audio, Codec::L16)]));
    assert_eq!(declared("ffmpeg:rtsp://cam/x"), None);
    assert_eq!(declared("ffmpeg:rtsp://cam/x#video=copy"), None);
    assert_eq!(declared("ffmpeg:rtsp://cam/x#video=h264#audio=copy"), None, "copy makes the whole list unknown");
}

#[test]
fn timeout_is_the_stall_timeout() {
    let plan = plan_of("ffmpeg:rtsp://cam/x#audio=opus#timeout=20", OPENIVR).unwrap();
    assert_eq!(plan.stall_timeout(), Some(Duration::from_secs(20)));
    assert_eq!(plan_of("ffmpeg:camera_1_src#video=h264", OPENIVR).unwrap().stall_timeout(), None);
}

#[test]
fn text_from_the_source_never_becomes_arguments() {
    // go2rtc would run each of these values as FFmpeg arguments.
    assert!(refused("ffmpeg:camera_1_src#raw=-y C:/Windows/x").contains("no template"));
    assert!(refused("ffmpeg:camera_1_src#video=-c:v libx264 -f mp4 out.mp4").contains("no template"));
    assert!(refused("ffmpeg:camera_1_src#audio=-c:a aac").contains("no template"));
    assert!(refused("ffmpeg:rtsp://cam/x#input=-f lavfi -i {input}").contains("no template"));
    assert!(refused("ffmpeg:camera_1_src#video=h264#width=640,movie=secret").contains("width"));
    assert!(refused("ffmpeg:camera_1_src#video=h264#height=0").contains("height"));
    assert!(refused("ffmpeg:rtsp://cam/x#timeout=5 -y").contains("timeout"));
    assert!(refused("ffmpeg:rtsp://cam/x#timeout=0").contains("timeout"));
    assert!(refused("ffmpeg:rtsp://cam/x#video=h264#bitrate=1M -y").contains("bitrate"));
    assert!(refused("ffmpeg:rtsp://cam/x#video=h264#hardware=vaapi").contains("hardware"));
    assert!(refused("ffmpeg:rtsp://cam/x#video=h264#drawtext=textfile=/etc/passwd").contains("drawtext"));
    assert!(refused("ffmpeg:rtsp://cam/x#video").contains("needs a value"));
    // Named templates are fine, a custom one included.
    assert!(plan_of("ffmpeg:camera_1_src#raw=dedup_input", OPENIVR).is_ok());
    assert!(plan_of("ffmpeg:camera_1_src#video=h264#bitrate=1500k", OPENIVR).is_ok());
}

#[test]
fn inputs_are_streams_or_network_urls_only() {
    assert!(refused("ffmpeg:/media/bbb.mp4").contains("stream name"));
    assert!(
        refused("ffmpeg:C:/media/bbb.mp4").contains("scheme") || refused("ffmpeg:C:/media/bbb.mp4").contains("stream")
    );
    assert!(refused("ffmpeg:device?video=0").contains("stream name"));
    assert!(refused("ffmpeg:file:///etc/passwd#video=h264").contains("scheme"));
    assert!(refused("ffmpeg:rtmp://example.com").contains("scheme"));
    let self_read = plan_of("ffmpeg:camera_1#video=h264", OPENIVR);
    assert_eq!(self_read, Err(SpecError::SelfReference));
}

#[test]
fn outputs_oxrtc_cannot_carry_are_refused() {
    for spec in [
        "ffmpeg:rtsp://cam/x#video=mjpeg",
        "ffmpeg:rtsp://cam/x#video=raw/gray8",
        "ffmpeg:rtsp://cam/x#audio=pcml",
        "ffmpeg:rtsp://cam/x#audio=mp3",
        "ffmpeg:rtsp://cam/x#audio=auto",
    ] {
        assert!(refused(spec).contains("not supported"), "{spec}");
    }
    assert!(refused("ffmpeg:rtsp://cam/x#video=opus").contains("not a video codec"));
}

#[test]
fn an_output_template_without_its_placeholder_is_refused() {
    let broken = [("output", "-f rtsp rtsp://elsewhere/x")];
    let result = plan_of("ffmpeg:rtsp://cam/x#video=h264", &broken);
    assert!(matches!(result, Err(SpecError::Invalid(why)) if why.contains("{output}")));
}

#[test]
fn errors_do_not_quote_credentials() {
    let why = refused("ffmpeg:camera_1_src#input=rtsp://admin:hunter2@cam/x");
    assert!(!why.contains("hunter2"), "{why}");
}

#[test]
fn query_escape_matches_go() {
    assert_eq!(query_escape("camera_1_src#video=h264#width=1920"), "camera_1_src%23video%3Dh264%23width%3D1920");
    assert_eq!(query_escape("a b/c~d.e-f_g"), "a+b%2Fc~d.e-f_g");
    assert_eq!(query_escape("\u{e9}"), "%C3%A9");
}

#[test]
fn a_plan_debug_hides_the_input_credentials() {
    let plan = plan_of("ffmpeg:rtsp://admin:hunter2@192.168.1.10:554/live#video=h264", OPENIVR).unwrap();
    assert!(plan.args(OUTPUT).iter().any(|a| a.contains("hunter2")), "FFmpeg needs them");
    let shown = format!("{plan:?}");
    assert!(!shown.contains("hunter2") && shown.contains("rtsp://***@192.168.1.10"), "{shown}");
}
