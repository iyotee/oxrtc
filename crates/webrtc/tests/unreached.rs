//! A session no packet of its viewer ever reaches (a firewall dropping
//! every check, as Windows' did for openIVR's phone viewers on 2026-11-22)
//! ends named so, with a warning that says where to look; a session that
//! played keeps its own reason. Its own test binary: the log capture is the
//! process's one subscriber.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing, clippy::panic)]

mod common;

use std::fmt::Write as _;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use common::*;
use oxrtc_core::Codec;
use str0m::media::{Direction, MediaKind as RtcKind};
use tracing::field::{Field, Visit};
use tracing::span::{Attributes, Id, Record};
use tracing::{Event, Metadata, Subscriber};

#[derive(Clone, Default)]
struct Capture(Arc<Mutex<String>>);

struct Visitor<'a>(&'a mut String);

impl Visit for Visitor<'_> {
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        let _ = write!(self.0, "{}={value:?} ", field.name());
    }
}

impl Subscriber for Capture {
    fn enabled(&self, metadata: &Metadata<'_>) -> bool {
        metadata.target().starts_with("oxrtc_webrtc")
    }
    fn new_span(&self, _: &Attributes<'_>) -> Id {
        Id::from_u64(1)
    }
    fn record(&self, _: &Id, _: &Record<'_>) {}
    fn record_follows_from(&self, _: &Id, _: &Id) {}
    fn event(&self, event: &Event<'_>) {
        let mut log = self.0.lock().unwrap();
        let _ = write!(log, "{} ", event.metadata().level());
        event.record(&mut Visitor(&mut log));
        log.push('\n');
    }
    fn enter(&self, _: &Id) {}
    fn exit(&self, _: &Id) {}
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_session_its_viewer_never_reached_is_named_so() {
    let capture = Capture::default();
    tracing::subscriber::set_global_default(capture.clone()).unwrap();
    let (params, video) = synthetic_h264();
    let stream = hub(FakeFactory::default().with("cam", av_script(Codec::H264, params, video, true)))
        .upsert("cam", &["fake:cam"])
        .unwrap();
    let server = server(4, Duration::from_secs(1), WAIT).await;

    // A viewer whose every packet is dropped on the way: it offers, and its
    // checks never arrive.
    let mut rtc = browser(false).build(Instant::now());
    let mut api = rtc.sdp_api();
    api.add_media(RtcKind::Video, Direction::RecvOnly, None, None, None);
    let (offer, _pending) = api.apply().unwrap();
    server.answer(stream.clone(), &offer.to_sdp_string(), None).await.unwrap();
    assert!(eventually(WAIT, || server.session_count() == 0).await, "the session ends");
    let log = capture.0.lock().unwrap().clone();
    let ended: Vec<&str> = log.lines().filter(|l| l.contains("webrtc session ended")).collect();
    assert_eq!(ended.len(), 1, "{log}");
    assert!(ended[0].contains("reason=no packet from the viewer reached this server"), "{}", ended[0]);
    assert!(
        log.lines().any(|l| l.starts_with("WARN") && l.contains("no packet from the viewer ever reached this server")),
        "{log}"
    );

    // One that plays, then leaves: its reason is its own.
    let mut client = connect(&server, stream, browser(false)).await.unwrap();
    assert_video(&mut client, 3).await;
    client.stop(Control::Close);
    assert!(eventually(WAIT, || server.session_count() == 0).await);
    let log = capture.0.lock().unwrap().clone();
    let ended: Vec<&str> = log.lines().filter(|l| l.contains("webrtc session ended")).collect();
    assert_eq!(ended.len(), 2, "{log}");
    assert!(!ended[1].contains("no packet from the viewer"), "{}", ended[1]);
}
