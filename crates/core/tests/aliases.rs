//! What bounds aliases: a stream never reads itself through others, and
//! resolving an alias is bounded in depth and in breadth.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing, clippy::panic)]

mod common;

use std::sync::Arc;

use common::{FakeFactory, Script, hub_with, limits};
use oxrtc_core::{
    Codec, ConsumerTag, HubError, Limits, MAX_ALIAS_DEPTH, MAX_CANDIDATES, MediaKind, SpecError, SubscribeError, Want,
};

fn tag() -> ConsumerTag {
    ConsumerTag::new("test")
}

fn cycle(error: HubError) -> (usize, String) {
    match error {
        HubError::Spec { index, error: SpecError::Cycle(path) } => (index, path),
        other => panic!("{other:?}"),
    }
}

#[tokio::test(start_paused = true)]
async fn a_stream_that_would_read_itself_through_others_is_refused() {
    let factory = Arc::new(FakeFactory::default());
    factory.add("cam", Script::video(Codec::H264, Script::gop(5)));
    let hub = hub_with(factory, limits());
    hub.upsert("a", &["stream:b"]).unwrap();
    hub.upsert("b", &["stream:c"]).unwrap();
    // c -> a -> b -> c.
    let (index, path) = cycle(hub.upsert("c", &["fake:cam", "stream:a?video"]).unwrap_err());
    assert_eq!((index, path.as_str()), (1, "a -> b -> c"));
    assert!(hub.get("c").is_none(), "not created");
    // The shortest one too, and through a stream that is replaced.
    hub.upsert("c", &["fake:cam"]).unwrap();
    let (_, path) = cycle(hub.upsert("b", &["stream:a"]).unwrap_err());
    assert_eq!(path, "a -> b");
    assert_eq!(hub.get("b").unwrap().specs().next().unwrap().to_string(), "stream:c", "the old b is kept");
    // A stream naming itself is still refused when its alias is built.
    assert_eq!(
        hub.upsert("d", &["stream:d"]).unwrap_err(),
        HubError::Spec { index: 0, error: SpecError::SelfReference }
    );
}

#[tokio::test(start_paused = true)]
async fn two_aliases_onto_one_stream_are_not_a_cycle() {
    let factory = Arc::new(FakeFactory::default());
    factory.add("cam", Script::video(Codec::H264, Script::gop(50)));
    let hub = hub_with(factory, limits());
    hub.upsert("d", &["fake:cam"]).unwrap();
    hub.upsert("b", &["stream:d"]).unwrap();
    hub.upsert("c", &["stream:d"]).unwrap();
    let top = hub.upsert("a", &["stream:b", "stream:c"]).unwrap();
    assert!(top.subscribe(&Want::all(), tag()).await.is_ok());
}

fn chain(hub: &oxrtc_core::Hub, aliases: usize) -> Arc<oxrtc_core::Stream> {
    hub.upsert("s0", &["fake:cam"]).unwrap();
    let mut last = None;
    for n in 1..=aliases {
        last = Some(hub.upsert(&format!("s{n}"), &[format!("stream:s{}", n - 1)]).unwrap());
    }
    last.unwrap()
}

#[tokio::test(start_paused = true)]
async fn aliases_are_followed_up_to_a_depth() {
    let factory = Arc::new(FakeFactory::default());
    factory.add("cam", Script::video(Codec::H264, Script::gop(50)));
    let hub = hub_with(factory.clone(), limits());
    let deepest = chain(&hub, MAX_ALIAS_DEPTH);
    assert!(deepest.subscribe(&Want::all(), tag()).await.is_ok(), "{MAX_ALIAS_DEPTH} aliases in a row");

    let hub = hub_with(factory, limits());
    let deeper = chain(&hub, MAX_ALIAS_DEPTH + 1);
    assert_eq!(deeper.subscribe(&Want::all(), tag()).await.unwrap_err(), SubscribeError::NoMatchingSource);
}

#[tokio::test(start_paused = true)]
async fn the_candidates_of_one_media_are_bounded() {
    // Sixteen aliases onto streams of eight sources: 128 candidates. The
    // first 64 offer only a picture; the sound behind them is passed over.
    let factory = Arc::new(FakeFactory::default());
    let mut picture = Script::video(Codec::H264, Script::gop(50));
    picture.declared = Some(vec![(MediaKind::Video, Codec::H264)]);
    factory.add("v", picture);
    let mut sound = Script::video(Codec::Opus, vec![]);
    sound.declared = Some(vec![(MediaKind::Audio, Codec::Opus)]);
    factory.add("a", sound);
    let hub = hub_with(factory, Limits { max_sources_per_stream: 16, ..limits() });
    let per_stream = 8;
    let full = MAX_CANDIDATES / per_stream;
    for n in 0..16 {
        let source = if n < full { "fake:v" } else { "fake:a" };
        hub.upsert(&format!("m{n}"), &vec![source; per_stream]).unwrap();
    }
    let aliases: Vec<String> = (0..16).map(|n| format!("stream:m{n}")).collect();
    let top = hub.upsert("top", &aliases).unwrap();
    let sound = Want::from_query("audio=opus").unwrap();
    assert_eq!(top.subscribe(&sound, tag()).await.unwrap_err(), SubscribeError::NoMatchingSource);
    // One alias fewer in front, and the sound is reached.
    let shorter = hub.upsert("shorter", &aliases[1..]).unwrap();
    assert!(shorter.subscribe(&sound, tag()).await.is_ok());
}
