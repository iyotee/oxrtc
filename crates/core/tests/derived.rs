//! A stream made from a stream by a source of its own (D46): one for every
//! caller while the stream lasts, its source run on demand and shared by its
//! readers, closed with the stream, shown in its listing, never in the hub's.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing, clippy::panic)]

mod common;

use std::sync::Arc;
use std::time::Duration;

use common::{FakeFactory, Script, hub_with, limits};
use oxrtc_core::derived::MAX_DERIVED;
use oxrtc_core::{Codec, ConsumerTag, Event, MediaWant, SubEnd, Want};

fn jpeg() -> Want {
    Want { video: MediaWant::OneOf(vec![Codec::Jpeg]), audio: MediaWant::Skip }
}

fn setup() -> (Arc<oxrtc_core::Hub>, Arc<oxrtc_core::Stream>, Arc<common::Fake>) {
    let factory = Arc::new(FakeFactory::default());
    factory.add("camera", Script::video(Codec::H264, Script::gop(25)));
    let transcode = factory.add("jpeg", Script::video(Codec::Jpeg, vec![(1, true); 200]));
    let hub = hub_with(factory, limits());
    let stream = hub.upsert("camera_1", &["fake:camera"]).unwrap();
    (hub, stream, transcode)
}

#[tokio::test(start_paused = true)]
async fn every_reader_shares_one_derived_source_run_on_demand() {
    let (hub, stream, transcode) = setup();
    let derived = hub.derived(&stream, "mjpeg", "fake:jpeg").unwrap();
    assert_eq!(derived.name(), "camera_1#mjpeg");
    assert!(Arc::ptr_eq(&derived, &hub.derived(&stream, "mjpeg", "fake:jpeg").unwrap()));
    assert_eq!(transcode.runs(), 0, "nothing runs before a reader");
    assert!(hub.get("camera_1#mjpeg").is_none() && hub.streams().len() == 1, "not in the hub's list");

    let mut first = derived.subscribe(&jpeg(), ConsumerTag::new("mjpeg")).await.unwrap();
    let mut second = derived.subscribe(&jpeg(), ConsumerTag::new("mjpeg")).await.unwrap();
    for sub in [&mut first, &mut second] {
        assert!(matches!(sub.recv().await.unwrap(), Event::Tracks(t) if t[0].codec == Codec::Jpeg));
    }
    assert_eq!((transcode.runs(), transcode.active()), (1, 1), "one source for both readers");

    let listing = stream.snapshot();
    assert!(listing.sources.iter().any(|s| s.url == "fake:jpeg"), "{listing:?}");
    assert_eq!(listing.consumers.iter().filter(|c| c.kind == "mjpeg").count(), 2);

    drop((first, second));
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert_eq!(transcode.active(), 0, "stopped after its grace");
}

#[tokio::test(start_paused = true)]
async fn a_replaced_stream_ends_its_derived_readers_and_a_kept_one_keeps_them() {
    let (hub, stream, _) = setup();
    let derived = hub.derived(&stream, "mjpeg", "fake:jpeg").unwrap();
    // The same sources again: the same stream, the same derived one.
    let kept = hub.upsert("camera_1", &["fake:camera"]).unwrap();
    assert!(Arc::ptr_eq(&derived, &hub.derived(&kept, "mjpeg", "fake:jpeg").unwrap()));

    let mut reader = derived.subscribe(&jpeg(), ConsumerTag::new("mjpeg")).await.unwrap();
    assert!(matches!(reader.recv().await.unwrap(), Event::Tracks(_)));
    hub.upsert("camera_1", &["fake:camera", "fake:jpeg"]).unwrap();
    assert!(derived.is_closed());
    let ended = loop {
        match reader.recv().await {
            Ok(_) => continue,
            Err(end) => break end,
        }
    };
    assert_eq!(ended, SubEnd::Closed);
    assert!(hub.derived(&stream, "mjpeg", "fake:jpeg").is_err(), "refused once its stream is closed");
}

#[tokio::test(start_paused = true)]
async fn an_alias_and_too_many_keys_are_refused() {
    let (hub, stream, _) = setup();
    assert!(hub.derived(&stream, "alias", "stream:camera_1").is_err());
    for key in 0..MAX_DERIVED {
        hub.derived(&stream, &format!("k{key}"), "fake:jpeg").unwrap();
    }
    assert!(hub.derived(&stream, "one-more", "fake:jpeg").is_err());
    assert!(hub.derived(&stream, "k0", "fake:jpeg").is_ok(), "a key it holds is still answered");
}
