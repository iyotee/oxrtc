//! The RTSP session openIVR opens by hand rather than through FFmpeg: its
//! sound de-duplicator (`app/services/streaming/audio_dedup.py`, which serves
//! a camera's G.711 sound to the Opus transcode with every packet once). It
//! asks in an order of its own, to work around go2rtc, and every step of it
//! is replayed here byte for byte: the picture described alone on a
//! connection kept open and never set up; the plain URL described on a
//! second one; the sound alone set up, on the channel pair twice its
//! section's index; `PLAY` on the URL as it was asked, without the
//! `Content-Base`'s trailing slash; `OPTIONS` sent in-band every 20 s, its
//! answer read between the interleaved frames.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing, clippy::panic)]

mod common;

use std::sync::Arc;
use std::time::Duration;

use common::{Client, Clip, Factory, config, hub, start};
use tokio::time::Instant;

/// `parse_sdp()`'s reading of a description: the index of the audio
/// section, its first payload type and its control URL.
fn audio_section(sdp: &str, base: &str) -> (usize, String, String) {
    let body = sdp.replace("\r\n", "\n");
    let sections: Vec<&str> = body.split("\nm=").skip(1).collect();
    let index = sections.iter().position(|s| s.starts_with("audio")).expect("an audio section");
    let lines: Vec<&str> = sections[index].lines().map(str::trim).collect();
    let payload = lines[0].split_whitespace().nth(3).unwrap().to_owned();
    let control = lines.iter().find_map(|l| l.strip_prefix("a=control:")).unwrap_or_default();
    let control = if control.starts_with("rtsp://") {
        control.to_owned()
    } else {
        format!("{}/{control}", base.trim_end_matches('/'))
    };
    (index, payload, control)
}

fn granted_channel(transport: &str) -> u8 {
    let (_, rest) = transport.split_once("interleaved=").expect("an interleaved transport");
    rest.chars().take_while(char::is_ascii_digit).collect::<String>().parse().unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_sound_deduplicators_session_carries_the_sound_alone() {
    let factory = Arc::new(Factory::default());
    factory.clip("camera_1_src", Clip::synthetic());
    let hub = hub(factory);
    hub.upsert("camera_1_src", &["clip:camera_1_src"]).unwrap();
    let server = start(hub.clone(), config()).await;
    let agent = "User-Agent: openIVR-audio";

    // The picture first, on a connection of its own that lives until the
    // sound plays: whoever reaches a closed camera first sets its order.
    let mut picture = Client::connect(server.addr).await;
    let url = server.url("camera_1_src");
    let described =
        picture.request("DESCRIBE", &format!("{url}?video"), &["Accept: application/sdp", agent], "").await.unwrap();
    assert_eq!(described.status, 200, "{described:?}");

    // Then the plain URL, both tracks described; the sound alone set up.
    let mut sound = Client::connect(server.addr).await;
    let described = sound.request("DESCRIBE", &url, &["Accept: application/sdp", agent], "").await.unwrap();
    assert_eq!(described.status, 200, "{described:?}");
    let base = described.header("Content-Base").unwrap_or_else(|| url.clone());
    let (index, payload, control) = audio_section(&described.body, &base);
    assert_eq!((index, payload.as_str()), (1, "8"), "video first, then PCMA: {}", described.body);
    let channel = 2 * index;
    let transport = format!("Transport: RTP/AVP/TCP;unicast;interleaved={channel}-{}", channel + 1);
    let setup = sound.request("SETUP", &control, &[transport.as_str(), agent], "").await.unwrap();
    assert_eq!(setup.status, 200, "{setup:?}");
    let session = setup.header("Session").unwrap().split(';').next().unwrap().to_owned();
    let session = format!("Session: {session}");
    let granted = granted_channel(&setup.header("Transport").unwrap());
    let play = sound.request("PLAY", &url, &["Range: npt=0.000-", session.as_str(), agent], "").await.unwrap();
    assert_eq!(play.status, 200, "PLAY on the URL as asked, without the trailing slash: {play:?}");
    drop(picture);

    // Only the sound crosses the connection, on the channel granted.
    let mut sound_packets = 0;
    let until = Instant::now() + Duration::from_millis(1500);
    while Instant::now() < until {
        let (on, data) = sound.interleaved().await.expect("the sound keeps coming");
        assert!(on == granted || on == granted + 1, "channel {on}: only the sound's pair ({granted}) is set up");
        if on == granted {
            assert_eq!(data[1] & 0x7f, 8, "PCMA payloads on the sound's channel");
            sound_packets += 1;
        }
    }
    assert!(sound_packets >= 20, "25 sound frames a second in this clip: {sound_packets}");

    // The keep-alive, sent between data frames and answered among them.
    let alive = sound.request("OPTIONS", &url, &[session.as_str(), agent], "").await.unwrap();
    assert_eq!(alive.status, 200, "{alive:?}");
    let (on, _) = sound.interleaved().await.expect("the sound goes on after the keep-alive");
    assert!(on == granted || on == granted + 1);
    let consumers = hub.get("camera_1_src").unwrap().snapshot().consumers;
    assert_eq!(consumers.len(), 1, "the picture's connection is gone, the sound's reads: {consumers:?}");
    assert_eq!(consumers[0].user_agent.as_deref(), Some("openIVR-audio"), "listed by its User-Agent, as go2rtc does");
}

/// The analysis decoders' URLs (`analysis_url()` and `capture_url()`, R36):
/// `?video&queue=live` is the picture alone AND the live class, together,
/// and every other spelling keeps the recorder's budget. Each half was tested
/// alone (`queries_select_the_described_media`, the paused reader's
/// `cam?queue=live`); the pair, as FFmpeg sends it, was not, and the class is
/// listed as `queue` so that it can be checked from outside.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_analysis_decoders_url_is_the_picture_alone_read_as_a_live_reader() {
    let factory = Arc::new(Factory::default());
    factory.clip("camera_1_sub", Clip::synthetic());
    let hub = hub(factory);
    hub.upsert("camera_1_sub", &["clip:camera_1_sub"]).unwrap();
    let server = start(hub.clone(), config()).await;
    let cases = [
        ("?video&queue=live", false, "live"),
        ("?queue=live&video", false, "live"),
        ("?video", false, "recorder"),
        ("?video&queue=recorder", false, "recorder"),
        ("?queue=live", true, "live"),
        ("", true, "recorder"),
    ];
    let mut clients = Vec::new();
    for (query, sound, class) in cases {
        let mut client = Client::connect(server.addr).await;
        let url = server.url(&format!("camera_1_sub{query}"));
        let described = client.request("DESCRIBE", &url, &["Accept: application/sdp"], "").await.unwrap();
        assert_eq!(described.status, 200, "{query}: {described:?}");
        assert!(described.body.contains("m=video"), "{query}: {}", described.body);
        assert_eq!(described.body.contains("m=audio"), sound, "{query}: {}", described.body);
        // FFmpeg sets a track up at the URL it was given, query and all, plus
        // `/trackID=0`: the query is not read again there.
        let base = described.header("Content-Base").unwrap();
        let setup = ["Transport: RTP/AVP/TCP;unicast;interleaved=0-1"];
        let set_up = client.request("SETUP", &format!("{base}trackID=0"), &setup, "").await.unwrap();
        assert_eq!(set_up.status, 200, "{query}: {set_up:?}");
        let listed = hub.get("camera_1_sub").unwrap().snapshot().consumers;
        let ours = listed.last().unwrap();
        assert_eq!((ours.kind.as_str(), ours.class.as_str()), ("rtsp", class), "{query}: {listed:?}");
        clients.push(client);
    }
}
