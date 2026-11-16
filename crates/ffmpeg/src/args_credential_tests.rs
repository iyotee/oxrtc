//! A run's credential for reading the relay goes into the input URL of a
//! source that reads the relay, and nowhere else (D24).

use std::collections::BTreeMap;

use super::*;

const RELAY: &str = "127.0.0.1:8554";
const OUTPUT: &str = "rtsp://127.0.0.1:8554/_publish/t";

fn plan_of(spec: &str) -> Plan {
    let templates = Templates::new(&BTreeMap::new());
    plan(&SourceSpec::parse(spec).unwrap(), &templates, RELAY, "camera_1").unwrap()
}

fn input_of(args: &[String]) -> &str {
    let at = args.iter().position(|a| a == "-i").unwrap();
    args.get(at + 1).unwrap()
}

#[test]
fn a_stream_name_is_read_with_the_runs_credential() {
    let plan = plan_of("ffmpeg:camera_1_src#audio=opus");
    let args = plan.args_reading_as(OUTPUT, Some(("run-1", "pw")));
    assert!(input_of(&args).starts_with("rtsp://run-1:pw@127.0.0.1:8554/camera_1_src?"), "{args:?}");
    assert_eq!(args.last().unwrap(), OUTPUT, "the output is the token's, with no credential");
    // The query, `&source=` and `&queue=` included, is what it was.
    let bare = plan.args(OUTPUT);
    assert_eq!(input_of(&args).replacen("run-1:pw@", "", 1), input_of(&bare));
}

#[test]
fn the_relays_own_url_is_read_with_it_too() {
    let plan = plan_of("ffmpeg:rtsp://127.0.0.1:8554/camera_1_src#audio=opus#timeout=20");
    let args = plan.args_reading_as(OUTPUT, Some(("run-1", "pw")));
    assert_eq!(input_of(&args), "rtsp://run-1:pw@127.0.0.1:8554/camera_1_src");
}

#[test]
fn without_a_credential_the_arguments_are_what_they_were() {
    let plan = plan_of("ffmpeg:camera_1_src#video=h264");
    assert_eq!(plan.args_reading_as(OUTPUT, None), plan.args(OUTPUT));
    assert!(!input_of(&plan.args(OUTPUT)).contains('@'));
}

#[test]
fn a_camera_never_gets_it() {
    for spec in ["ffmpeg:rtsp://192.168.1.9/stream#audio=opus", "ffmpeg:http://192.168.1.9/x.mjpg#video=h264"] {
        let plan = plan_of(spec);
        let args = plan.args_reading_as(OUTPUT, Some(("run-1", "pw")));
        assert!(!args.iter().any(|a| a.contains("run-1") || a.contains("pw@")), "{spec}: {args:?}");
        assert_eq!(args, plan.args(OUTPUT));
    }
}

#[test]
fn an_input_that_names_its_own_user_keeps_it() {
    // Not the relay by the plan's own test (`rtsp://<relay>/`): the operator wrote a user.
    let plan = plan_of("ffmpeg:rtsp://me:mine@127.0.0.1:8554/camera_1_src#audio=opus");
    let args = plan.args_reading_as(OUTPUT, Some(("run-1", "pw")));
    assert_eq!(input_of(&args), "rtsp://me:mine@127.0.0.1:8554/camera_1_src");
}

#[test]
fn the_credential_stays_out_of_the_plans_debug_form() {
    let plan = plan_of("ffmpeg:camera_1_src#audio=opus");
    let _ = plan.args_reading_as(OUTPUT, Some(("run-1", "pw")));
    assert!(!format!("{plan:?}").contains("pw"));
}

#[test]
fn the_relay_by_the_name_localhost_is_the_relay_too() {
    // The RTSP client reads `rtsp://localhost:<port>/x` as this server's stream (a loopback host
    // on the server's port), and so does FFmpeg's connection: it needs the credential as well.
    for spec in ["ffmpeg:rtsp://localhost:8554/camera_1_src#audio=opus", "ffmpeg:rtsp://LocalHost:8554/camera_1_src"] {
        let plan = plan_of(spec);
        assert!(plan.local(), "{spec}");
        let args = plan.args_reading_as(OUTPUT, Some(("run-1", "pw")));
        assert!(input_of(&args).starts_with("rtsp://run-1:pw@"), "{spec}: {args:?}");
        assert!(input_of(&args).to_ascii_lowercase().contains("@localhost:8554/camera_1_src"), "{spec}: {args:?}");
    }
}

#[test]
fn only_the_relays_own_port_on_a_loopback_name_is_the_relay() {
    for spec in [
        "ffmpeg:rtsp://localhost:8555/camera_1_src",
        "ffmpeg:rtsp://localhost/camera_1_src",
        "ffmpeg:rtsp://localhost.evil.example:8554/camera_1_src",
        "ffmpeg:rtsp://localhost:8554@evil.example/camera_1_src",
        "ffmpeg:rtsp://[::1]:8554/camera_1_src",
    ] {
        let plan = plan_of(spec);
        assert!(!plan.local(), "{spec}");
        let args = plan.args_reading_as(OUTPUT, Some(("run-1", "pw")));
        assert!(!args.iter().any(|a| a.contains("run-1")), "{spec}: {args:?}");
    }
}

#[test]
fn a_relay_on_the_ipv6_loopback_is_reached_by_localhost_too() {
    let templates = Templates::new(&BTreeMap::new());
    let spec = SourceSpec::parse("ffmpeg:rtsp://localhost:8554/camera_1_src#audio=opus").unwrap();
    let plan = plan(&spec, &templates, "[::1]:8554", "camera_1").unwrap();
    assert!(plan.local());
    // A relay that is not on loopback is not reached by `localhost`.
    let plan = super::plan(&spec, &templates, "192.168.1.5:8554", "camera_1").unwrap();
    assert!(!plan.local());
}
