//! The driver against a scripted RTSP server on 127.0.0.1:0: failure mapping,
//! SETUP order, the bounds, and TEARDOWN on drop. Needs no external binary.
// Test code: a failed expectation is a panic by design.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]

mod common;

use std::time::Duration;

use bytes::Bytes;
use common::camera::{FakeCamera, IDR, P_SLICE, PPS, SPS, Script, demand, header};
use oxrtc_core::frame::validate_avcc;
use oxrtc_core::{Codec, CodecParams, ConsumerTag, Event, HubError, MediaKind, SourceState, SpecError, Want};
use oxrtc_rtsp_client::{MAX_STALE_WAIT, SILENT_STALE_WAIT};

#[tokio::test]
async fn refused_credentials_are_auth_and_travel_decoded_outside_the_url() {
    let camera = FakeCamera::start(Script::Unauthorized).await;
    let hub = common::hub(vec![]);
    let stream = hub.upsert("cam", &[camera.url("admin:p%40ss@", "")]).unwrap();
    let viewer = demand(&stream);
    assert_eq!(common::retry_error(&stream, Duration::from_secs(10)).await, "authentication refused");
    viewer.abort();
    let requests = camera.requests();
    assert!(requests.iter().all(|r| !r.contains("p@ss") && !r.contains("admin:")), "{requests:?}");
    // Basic base64("admin:p@ss"): the password reached the camera decoded, in the header only.
    assert!(requests.iter().any(|r| header(r, "Authorization") == Some("Basic YWRtaW46cEBzcw==")), "{requests:?}");
    assert!(requests.iter().all(|r| header(r, "User-Agent").is_some_and(|ua| ua.starts_with("oxrtc/"))));
}

#[tokio::test]
async fn a_closed_port_is_unreachable() {
    let port = common::free_port();
    let hub = common::hub(vec![]);
    let stream = hub.upsert("cam", &[format!("rtsp://127.0.0.1:{port}/cam")]).unwrap();
    let viewer = demand(&stream);
    let error = common::retry_error(&stream, Duration::from_secs(10)).await;
    viewer.abort();
    assert!(error.starts_with("unreachable: Unable to connect"), "{error}");
}

#[tokio::test]
async fn a_silent_camera_times_out_the_handshake_after_its_own_timeout() {
    let camera = FakeCamera::start(Script::Silent).await;
    let hub = common::hub(vec![]);
    let stream = hub.upsert("cam", &[camera.url("", "#timeout=1")]).unwrap();
    let viewer = demand(&stream);
    let error = common::retry_error(&stream, Duration::from_secs(5)).await;
    viewer.abort();
    assert_eq!(error, "unreachable: no answer to DESCRIBE within 1s");
}

#[tokio::test]
async fn an_oversized_answer_is_refused_before_it_is_read() {
    let camera = FakeCamera::start(Script::Oversized).await;
    let hub = common::hub(vec![]);
    let stream = hub.upsert("cam", &[camera.url("", "#timeout=5")]).unwrap();
    let viewer = demand(&stream);
    let error = common::retry_error(&stream, Duration::from_secs(10)).await;
    viewer.abort();
    // Without the bound the driver would wait for the body and time out instead.
    assert!(error.starts_with("protocol error: RTSP framing error"), "{error}");
}

#[tokio::test]
async fn video_is_set_up_first_metadata_never_and_silence_after_play_is_a_stall() {
    let camera = FakeCamera::start(Script::PlayThenSilence).await;
    let hub = common::hub(vec![]);
    let stream = hub.upsert("cam", &[camera.url("", "#timeout=1")]).unwrap();
    let viewer = demand(&stream);
    common::wait_state(&stream, Duration::from_secs(5), |_| !stream.snapshot().sources[0].tracks.is_empty()).await;
    let tracks = stream.snapshot().sources[0].tracks.clone();
    assert_eq!(tracks.len(), 2, "{tracks:?}");
    assert_eq!((tracks[0].kind, tracks[0].codec, tracks[0].clock_rate), (MediaKind::Video, Codec::H264, 90_000));
    assert_eq!((tracks[1].kind, tracks[1].codec, tracks[1].clock_rate), (MediaKind::Audio, Codec::Pcma, 8_000));

    let error = common::retry_error(&stream, Duration::from_secs(5)).await;
    assert_eq!(error, "no media within the stall timeout");
    viewer.abort();

    let setups: Vec<String> = camera.requests().into_iter().filter(|r| r.starts_with("SETUP ")).collect();
    assert!(setups.len() >= 2, "{setups:?}");
    assert!(setups[0].contains("/cam/trackID=1 "), "video must be set up first: {setups:?}");
    assert!(setups[1].contains("/cam/trackID=0 "), "{setups:?}");
    assert!(setups.iter().all(|s| !s.contains("trackID=2")), "metadata was set up: {setups:?}");
    assert!(setups.iter().all(|s| header(s, "Transport").is_some_and(|t| t.contains("RTP/AVP/TCP"))));
}

#[tokio::test]
async fn audio_and_rtcp_without_pictures_is_a_stall() {
    let camera = FakeCamera::start(Script::PlayThenAudioOnly).await;
    let hub = common::hub(vec![]);
    let stream = hub.upsert("cam", &[camera.url("", "#timeout=1")]).unwrap();
    let viewer = demand(&stream);
    // The microphone is heard (the hub's own stall timeout is 20 s, so only
    // the driver's 1 s can end this run)...
    common::wait_state(&stream, Duration::from_secs(5), |_| stream.snapshot().sources[0].frames >= 3).await;
    // ...but the picture never comes.
    let error = common::retry_error(&stream, Duration::from_secs(5)).await;
    viewer.abort();
    assert_eq!(error, "no media within the stall timeout");
}

#[tokio::test]
async fn an_unanswered_audio_setup_fails_the_session_instead_of_playing_on() {
    let camera = FakeCamera::start(Script::AudioSetupUnanswered).await;
    let hub = common::hub(vec![]);
    let stream = hub.upsert("cam", &[camera.url("", "#timeout=1")]).unwrap();
    let viewer = demand(&stream);
    let error = common::retry_error(&stream, Duration::from_secs(5)).await;
    viewer.abort();
    // Going on to PLAY would read the late SETUP answer as PLAY's.
    assert_eq!(error, "unreachable: no answer to SETUP within 1s");
    assert!(!camera.requests().iter().any(|r| r.starts_with("PLAY ")), "{:?}", camera.requests());
}

#[tokio::test]
async fn the_last_viewer_leaving_tears_the_session_down() {
    let camera = FakeCamera::start(Script::PlayThenSilence).await;
    let hub = common::hub(vec![]);
    let stream = hub.upsert("cam", &[camera.url("", "#timeout=30")]).unwrap();
    let viewer = demand(&stream);
    common::wait_state(&stream, Duration::from_secs(5), |_| !stream.snapshot().sources[0].tracks.is_empty()).await;
    viewer.abort();
    common::wait_state(&stream, Duration::from_secs(5), |s| *s == SourceState::Idle).await;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while !camera.requests().iter().any(|r| r.starts_with("TEARDOWN ")) {
        assert!(tokio::time::Instant::now() < deadline, "no TEARDOWN: {:?}", camera.requests());
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// One viewer, as L03's: it takes the source up, sees pictures, leaves, and
/// the call returns once the camera has seen the TEARDOWN of its session.
/// Gives how long the viewer waited for the camera's DESCRIBE.
async fn viewer_cycle(camera: &FakeCamera, stream: &std::sync::Arc<oxrtc_core::Stream>) -> Duration {
    let count = |method: &str| camera.requests().iter().filter(|r| r.starts_with(method)).count();
    let (describes, teardowns) = (count("DESCRIBE "), count("TEARDOWN "));
    let started = tokio::time::Instant::now();
    let viewer = demand(stream);
    while count("DESCRIBE ") == describes {
        assert!(started.elapsed() < Duration::from_secs(6), "no DESCRIBE: {:?}", camera.requests());
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    let waited = started.elapsed();
    common::wait_state(stream, Duration::from_secs(8), |_| !stream.snapshot().sources[0].tracks.is_empty()).await;
    viewer.abort();
    while count("TEARDOWN ") == teardowns {
        assert!(started.elapsed() < Duration::from_secs(12), "no TEARDOWN: {:?}", camera.requests());
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    waited
}

/// A server that never answers TEARDOWN (go2rtc 1.9.15 while it plays) kept
/// retina's attempt, and the connection, open for 5 s, and every next viewer
/// waited all of `MAX_STALE_WAIT` for it (L03: 2.97 s against 2.49 s). The first
/// one still does, once, to learn the camera is silent; the others wait the
/// short courtesy only.
#[tokio::test]
async fn a_camera_that_never_answers_teardown_costs_the_stale_wait_once() {
    let camera = FakeCamera::start(Script::IgnoresTeardown).await;
    let hub = common::hub(vec![]);
    let stream = hub.upsert("cam", &[camera.url("", "#timeout=30")]).unwrap();
    let nobody_before = viewer_cycle(&camera, &stream).await;
    let first = viewer_cycle(&camera, &stream).await;
    let second = viewer_cycle(&camera, &stream).await;
    let third = viewer_cycle(&camera, &stream).await;
    assert!(nobody_before < MAX_STALE_WAIT / 2, "a source with nothing before it waited {nobody_before:?}");
    // Absolute, not relative to the constants: retina's own attempt lasts 5 s.
    assert!(first >= MAX_STALE_WAIT - Duration::from_millis(100), "the first waited only {first:?}");
    assert!(first < Duration::from_secs(2), "the first waited {first:?} (stale wait {MAX_STALE_WAIT:?})");
    for later in [second, third] {
        assert!(later < MAX_STALE_WAIT / 2, "a later viewer waited {later:?} (short wait {SILENT_STALE_WAIT:?})");
    }
}

/// A camera that answers TEARDOWN ends the wait at once, whatever has been
/// learned: nobody waits for what has already happened.
#[tokio::test]
async fn a_camera_that_answers_teardown_is_never_waited_for() {
    let camera = FakeCamera::start(Script::PlayThenSilence).await;
    let hub = common::hub(vec![]);
    let stream = hub.upsert("cam", &[camera.url("", "#timeout=30")]).unwrap();
    for _ in 0..4 {
        let waited = viewer_cycle(&camera, &stream).await;
        assert!(waited < MAX_STALE_WAIT / 2, "waited {waited:?} for a camera that answers");
    }
}

/// What a silent camera taught is doubted again when it refuses a session
/// after the short wait: it may have said no for want of the old session's end,
/// so the retry waits in full. The gap between the refused DESCRIBE and the
/// next one is the backoff (150 to 250 ms here) plus that wait: about 1.2 s
/// with the full wait, under 0.4 s with the short one.
#[tokio::test]
async fn a_refusal_after_the_short_wait_makes_the_retry_wait_in_full() {
    let camera = FakeCamera::start(Script::IgnoresTeardown).await;
    let hub = common::hub(vec![]);
    let stream = hub.upsert("cam", &[camera.url("", "#timeout=30")]).unwrap();
    let _nobody_before = viewer_cycle(&camera, &stream).await;
    let _learns_silence = viewer_cycle(&camera, &stream).await;
    let short = viewer_cycle(&camera, &stream).await;
    assert!(short < MAX_STALE_WAIT / 2, "the camera was not taken for silent: {short:?}");

    let count = || camera.requests().iter().filter(|r| r.starts_with("DESCRIBE ")).count();
    let before = count();
    camera.refuse_next_describes(1);
    let viewer = demand(&stream);
    let started = tokio::time::Instant::now();
    while count() == before {
        assert!(started.elapsed() < Duration::from_secs(6), "no DESCRIBE: {:?}", camera.requests());
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
    let refused_at = tokio::time::Instant::now();
    while count() == before + 1 {
        assert!(refused_at.elapsed() < Duration::from_secs(6), "no retry: {:?}", camera.requests());
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
    let gap = refused_at.elapsed();
    viewer.abort();
    assert!(gap >= MAX_STALE_WAIT, "the retry after a refusal waited only {gap:?} (stale wait {MAX_STALE_WAIT:?})");
}

#[tokio::test]
async fn sources_are_checked_when_the_stream_is_created() {
    let port = common::free_port();
    let hub = common::hub(vec![port]);
    let refused_as = |name: &str, source: String| match hub.upsert(name, std::slice::from_ref(&source)) {
        Err(HubError::Spec { error, .. }) => error,
        other => panic!("{source} gave {other:?}"),
    };
    let refused = |source: String| refused_as("cam", source);
    assert_eq!(refused(format!("rtsp://127.0.0.1:{port}/cam")), SpecError::SelfReference);
    for bad in [
        format!("rtsp://127.0.0.1:{port}/not a name"),
        format!("rtsp://127.0.0.1:{port}/other?video=nope"),
        "rtsp:///live".to_owned(),
        "rtsp://cam/x#timeout=0".to_owned(),
    ] {
        assert!(matches!(refused(bad.clone()), SpecError::Invalid(_)), "{bad}");
    }
    assert!(hub.upsert("tls", &["rtsps://cam.example/live"]).is_ok());
    // Loopback URLs are aliases: one that would lead back is a cycle.
    assert!(hub.upsert("x", &[format!("rtsp://127.0.0.1:{port}/y?video#timeout=20")]).is_ok());
    assert_eq!(refused_as("y", format!("rtsp://localhost:{port}/x")), SpecError::Cycle("x -> y".into()));
}

#[tokio::test]
async fn a_url_on_our_own_port_is_read_in_process() {
    let camera = FakeCamera::start(Script::PlayThenSilence).await;
    // Nothing listens on this port: only the in-process path can work.
    let port = common::free_port();
    let hub = common::hub(vec![port]);
    hub.upsert("cam", &[camera.url("", "#timeout=30")]).unwrap();
    let relay = hub.upsert("relay", &[format!("rtsp://localhost:{port}/cam?audio")]).unwrap();
    let mut sub = tokio::time::timeout(Duration::from_secs(10), relay.subscribe(&Want::all(), ConsumerTag::new("t")))
        .await
        .unwrap()
        .unwrap();
    let Ok(Event::Tracks(tracks)) = sub.recv().await else { panic!("first event is not Tracks") };
    assert_eq!(tracks.iter().map(|t| t.codec).collect::<Vec<_>>(), [Codec::Pcma]);
    assert!(matches!(relay.snapshot().sources[0].state, SourceState::Connecting { .. } | SourceState::Running));
    assert_eq!(camera.requests().iter().filter(|r| r.starts_with("DESCRIBE ")).count(), 1);
}

#[tokio::test]
async fn parameters_found_in_band_publish_a_new_track_list_and_frames_are_avcc() {
    let camera = FakeCamera::start(Script::PlayThenFrames).await;
    let hub = common::hub(vec![]);
    let stream = hub.upsert("cam", &[camera.url("", "#timeout=30")]).unwrap();
    let want = Want::from_query("video").unwrap();
    let mut sub = tokio::time::timeout(Duration::from_secs(10), stream.subscribe(&want, ConsumerTag::new("t")))
        .await
        .unwrap()
        .unwrap();
    let mut tracks = None;
    let mut frames = Vec::new();
    while frames.len() < 2 {
        match tokio::time::timeout(Duration::from_secs(10), sub.recv()).await.unwrap().unwrap() {
            Event::Tracks(t) => tracks = Some(t),
            Event::Frame { frame, .. } => frames.push(frame),
            Event::Discontinuity => panic!("discontinuity"),
        }
    }
    // The SDP announced no parameter sets: these came from the first frame.
    let tracks = tracks.unwrap();
    assert_eq!(tracks[0].params, CodecParams::H264 { sps: Bytes::from_static(SPS), pps: Bytes::from_static(PPS) });

    let (first, second) = (&frames[0], &frames[1]);
    assert!(first.keyframe && !second.keyframe);
    // On the hub's timeline, the camera's own step kept: the track list
    // published again for the parameter sets moved nothing.
    assert_eq!(second.pts - first.pts, 3000);
    let nals: Vec<&[u8]> = first.nal_units().collect();
    assert_eq!(nals, [SPS, PPS, IDR]);
    assert_eq!(second.nal_units().collect::<Vec<_>>(), [P_SLICE]);
    assert_eq!(validate_avcc(&first.data), Ok(3));
}
