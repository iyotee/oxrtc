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

/// The two differences from go2rtc's line for a transcode of a stream by
/// name: a video transcode reads its input with `&queue=live` (D18), a key
/// go2rtc ignores in a reader's query, and an input of the relay is read with
/// no frame-rate guess (`-fpsprobesize 0`, D36).
#[test]
fn openivr_transcode_of_a_stream_by_name() {
    let spec = "ffmpeg:camera_1_src#video=h264#width=1920#height=1080";
    assert_eq!(
        ours(spec, OPENIVR),
        go2rtc(
            "ffmpeg -hide_banner -v error -fpsprobesize 0 -flags low_delay -timeout 5000000 -user_agent go2rtc/ffmpeg \
             -rtsp_flags prefer_tcp -i rtsp://127.0.0.1:8554/camera_1_src?video&source=ffmpeg:camera_1_src%23video%3Dh264%23width%3D1920%23height%3D1080&queue=live \
             -codec:v libx264 -g:v 30 -preset:v superfast -tune:v zerolatency -profile:v baseline -level:v 5.1 -pix_fmt:v yuv420p -fps_mode:v passthrough \
             -an -vf \"scale=1920:1080\" -user_agent ffmpeg/go2rtc -rtsp_transport tcp -f rtsp {output}"
        )
    );
    // The same with go2rtc's own h264 template.
    assert_eq!(
        ours(spec, &[]),
        go2rtc(
            "ffmpeg -hide_banner -v error -fpsprobesize 0 -flags low_delay -timeout 5000000 -user_agent go2rtc/ffmpeg \
             -rtsp_flags prefer_tcp -i rtsp://127.0.0.1:8554/camera_1_src?video&source=ffmpeg:camera_1_src%23video%3Dh264%23width%3D1920%23height%3D1080&queue=live \
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
            "ffmpeg -hide_banner -v error -fpsprobesize 0 -allowed_media_types audio -flags low_delay -timeout 20000000 \
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
            "ffmpeg -hide_banner -v error -allowed_media_types video+audio -flags low_delay -timeout 5000000 -user_agent go2rtc/ffmpeg -rtsp_flags prefer_tcp -i rtsp://example.com -c copy -user_agent ffmpeg/go2rtc -rtsp_transport tcp -f rtsp {output}",
        ),
        // go2rtc's test still expects `-level:v 5.1` here; its 1.9.15 default
        // template (the one it runs) uses `-x265-params`.
        (
            "ffmpeg:rtsp://example.com#video=h265#width=1280#height=720",
            "ffmpeg -hide_banner -v error -allowed_media_types video -flags low_delay -timeout 5000000 -user_agent go2rtc/ffmpeg -rtsp_flags prefer_tcp -i rtsp://example.com -c:v libx265 -g 50 -profile:v main -x265-params level=5.1:high-tier=0 -preset:v superfast -tune:v zerolatency -pix_fmt:v yuv420p -an -vf \"scale=1280:720\" -user_agent ffmpeg/go2rtc -rtsp_transport tcp -f rtsp {output}",
        ),
        (
            "ffmpeg:rtsp://example.com#input=rtsp/udp",
            "ffmpeg -hide_banner -v error -allowed_media_types video+audio -fflags nobuffer -flags low_delay -timeout 5000000 -user_agent go2rtc/ffmpeg -i rtsp://example.com -c copy -user_agent ffmpeg/go2rtc -rtsp_transport tcp -f rtsp {output}",
        ),
        (
            "ffmpeg:rtsp://example.com#timeout=10",
            "ffmpeg -hide_banner -v error -allowed_media_types video+audio -flags low_delay -timeout 10000000 -user_agent go2rtc/ffmpeg -rtsp_flags prefer_tcp -i rtsp://example.com -c copy -user_agent ffmpeg/go2rtc -rtsp_transport tcp -f rtsp {output}",
        ),
        (
            "ffmpeg:rtsp://example.com#audio=opus",
            "ffmpeg -hide_banner -v error -allowed_media_types audio -flags low_delay -timeout 5000000 -user_agent go2rtc/ffmpeg -rtsp_flags prefer_tcp -i rtsp://example.com -c:a libopus -application:a lowdelay -min_comp 0 -vn -user_agent ffmpeg/go2rtc -rtsp_transport tcp -f rtsp {output}",
        ),
        (
            "ffmpeg:rtsp://example.com#video=h264#rotate=-90",
            "ffmpeg -hide_banner -v error -allowed_media_types video -flags low_delay -timeout 5000000 -user_agent go2rtc/ffmpeg -rtsp_flags prefer_tcp -i rtsp://example.com -c:v libx264 -g 50 -profile:v high -level:v 4.1 -preset:v superfast -tune:v zerolatency -pix_fmt:v yuv420p -an -vf \"transpose=2\" -user_agent ffmpeg/go2rtc -rtsp_transport tcp -f rtsp {output}",
        ),
        (
            "ffmpeg:rtsp://example.com#video=copy#audio=pcmu",
            "ffmpeg -hide_banner -v error -allowed_media_types video+audio -flags low_delay -timeout 5000000 -user_agent go2rtc/ffmpeg -rtsp_flags prefer_tcp -i rtsp://example.com -c:v copy -c:a pcm_mulaw -ar:a 8000 -ac:a 1 -user_agent ffmpeg/go2rtc -rtsp_transport tcp -f rtsp {output}",
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
            "ffmpeg -hide_banner -v error -allowed_media_types video+audio -flags low_delay -timeout 5000000 \
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
    assert_eq!(args[..6], ["-hide_banner", "-v", "error", "-fpsprobesize", "0", "-use_wallclock_as_timestamps"]);
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

/// The input URL of a plan: the argument after `-i`.
fn input_of(spec: &str) -> String {
    let args = ours(spec, &[]);
    let at = args.iter().position(|a| a == "-i").unwrap();
    args[at + 1].clone()
}

/// A transcode that only has a picture to make is read as what it is: a
/// reader whose output WebRTC viewers watch, which a lagging CPU must cut to
/// a keyframe after 2 s rather than let trail by 10 (D18, R23b).
#[test]
fn a_video_transcode_of_a_stream_reads_its_input_as_a_live_reader() {
    for spec in [
        "ffmpeg:camera_1_src#video=h264#width=1920#height=1080",
        "ffmpeg:camera_1_src#video=copy",
        "ffmpeg:camera_1_src#video=h264#video=h265",
        "ffmpeg:camera_1_src#video=h264#query=x=1",
    ] {
        let url = input_of(spec);
        assert!(url.contains("?video&source=ffmpeg:") && url.contains("&queue=live"), "{spec}: {url}");
        assert_eq!(url.matches("queue=").count(), 1, "{spec}: {url}");
    }
    // Whatever follows the class is the operator's own `#query=`, in order.
    assert!(input_of("ffmpeg:camera_1_src#video=h264#query=x=1").ends_with("&queue=live&x=1"));
}

/// Whatever carries sound is recorded by openIVR (`camera_N_rec`), and a
/// sound transcode cut to a keyframe would drop sound frames: it keeps the
/// recorder class, so does a transcode that is not one (a copy of everything).
#[test]
fn a_transcode_with_sound_keeps_the_recorder_class() {
    for spec in [
        "ffmpeg:camera_1_src#audio=opus",
        "ffmpeg:camera_1_src#audio=opus#audio=pcmu",
        "ffmpeg:camera_1_src#video=h264#audio=opus",
        "ffmpeg:camera_1_src#video=copy#audio=copy",
        "ffmpeg:camera_1_src",
    ] {
        let url = input_of(spec);
        assert!(!url.contains("queue="), "{spec}: {url}");
    }
}

/// The operator's own class wins: a `#query=queue=recorder` is the old
/// behaviour back, and a second `queue=` is not added beside it (the server
/// answers `400` to two that differ).
#[test]
fn a_class_the_operator_gave_is_not_overridden() {
    let url = input_of("ffmpeg:camera_1_src#video=h264#query=queue=recorder");
    assert_eq!(url.matches("queue=").count(), 1, "{url}");
    assert!(url.ends_with("&queue=recorder"), "{url}");
    let url = input_of("ffmpeg:camera_1_src#video=h264#query=x=1&queue=live");
    assert_eq!(url.matches("queue=").count(), 1, "{url}");
    // A value the server will refuse is still the operator's to fix: the
    // driver adds no key beside it. The class is read by the server's own
    // parser, so `?queue=` and a key after others count as it counts them.
    for extra in ["queue=bogus", "?queue=recorder", "a=1&queue=live&b=2"] {
        let url = input_of(&format!("ffmpeg:camera_1_src#video=h264#query={extra}"));
        assert_eq!(url.matches("queue=").count(), 1, "{extra}: {url}");
    }
    // `queue` without a value, or a longer key that starts the same, names no class.
    let url = input_of("ffmpeg:camera_1_src#video=h264#query=queued=1");
    assert!(url.contains("&queue=live&queued=1"), "{url}");
}

/// The key the driver writes is read back by the server's parser as the live
/// class: one definition of the key and of its value, not two spellings.
#[test]
fn the_key_the_driver_writes_is_the_class_the_server_reads() {
    let url = input_of("ffmpeg:camera_1_src#video=h264");
    let query = url.split_once('?').map(|(_, q)| q).unwrap_or_default();
    assert_eq!(oxrtc_core::ConsumerClass::from_query(query).unwrap(), Some(oxrtc_core::ConsumerClass::Live), "{url}");
}

/// A URL is the operator's own: only a stream of this server is read through
/// a query oxrtc builds.
#[test]
fn a_url_input_is_left_as_it_is() {
    for spec in [
        "ffmpeg:rtsp://127.0.0.1:8554/camera_1_src#video=h264",
        "ffmpeg:rtsp://admin:pw@192.168.1.10:554/live#video=h264",
        "ffmpeg:http://example.com/cam#video=h264",
    ] {
        assert!(!input_of(spec).contains("queue="), "{spec}");
    }
}

#[test]
fn the_rate_guess_is_skipped_only_for_the_relay_and_an_ffmpeg_that_reads_the_rate() {
    let relay = plan_of("ffmpeg:camera_1_src#video=h264", OPENIVR).unwrap();
    let skipped = relay.args_reading_as("{output}", None, true);
    assert_eq!(skipped.get(3..5), Some(&["-fpsprobesize".to_owned(), "0".to_owned()][..]), "{skipped:?}");
    // FFmpeg 6.1 and 7.1 do not read `a=framerate`: their guess is kept.
    let kept = relay.args_reading_as("{output}", None, false);
    assert!(!kept.iter().any(|a| a == "-fpsprobesize"), "{kept:?}");
    assert_eq!(kept.len() + 2, skipped.len());
    // A camera's own URL is never told: its rate is the camera's.
    let camera = plan_of("ffmpeg:rtsp://192.0.2.10/live#video=h264", OPENIVR).unwrap();
    assert!(!camera.args_reading_as("{output}", None, true).iter().any(|a| a == "-fpsprobesize"));
}
