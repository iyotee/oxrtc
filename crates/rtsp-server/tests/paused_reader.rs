//! A reader that stops reading (a segmenter rotating its file, a process
//! descheduled on a loaded machine): its frames wait in its subscription's
//! queue, a recorder's (10 s, 32 MiB), not in a write queue of 256 items.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing, clippy::panic)]

mod common;

use std::io::Write;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bytes::{BufMut, Bytes, BytesMut};
use common::{Client, Clip, Factory, config, hub, start};
use oxrtc_core::{Codec, CodecParams, Drivers, Hub, Limits, QueueBudget, TrackInfo};
use tokio::time::Instant;

/// A camera of 100 KB pictures at 25 a second (2.5 MB/s, a keyframe each
/// second) and G.711 sound every 20 ms: 5 s of it is 375 items and 12.5 MB,
/// beyond the 256 items and 4 MiB the write queue held before, and beyond
/// what the sockets' own buffers take.
fn heavy() -> Clip {
    let picture = |key: bool| {
        let mut nal = BytesMut::with_capacity(100_004);
        nal.put_u32(100_000);
        nal.put_u8(if key { 0x65 } else { 0x41 });
        nal.put_bytes(0x5a, 99_999);
        nal.freeze()
    };
    let (key, delta) = (picture(true), picture(false));
    let sound = Bytes::from_static(&[0xd5; 160]);
    let mut frames = Vec::new();
    for i in 0..25 {
        frames.push((0, i == 0, if i == 0 { key.clone() } else { delta.clone() }));
        frames.push((1, true, sound.clone()));
        frames.push((1, true, sound.clone()));
    }
    let params = CodecParams::H264 {
        sps: Bytes::from_static(&[0x67, 0x64, 0x00, 0x1f, 0xac]),
        pps: Bytes::from_static(&[0x68, 0xee, 0x3c, 0xb0]),
    };
    Clip {
        tracks: vec![TrackInfo::video(Codec::H264).with_params(params), TrackInfo::audio(Codec::Pcma, 8000, 1)],
        frames,
        fps: 25,
    }
}

/// Describes, sets up both tracks (channels 0-1 and 2-3) and plays.
async fn play(c: &mut Client, url: &str) {
    let d = c.request("DESCRIBE", url, &["Accept: application/sdp"], "").await.unwrap();
    assert_eq!(d.status, 200, "{d:?}");
    let base = d.header("Content-Base").unwrap();
    let video = ["Transport: RTP/AVP/TCP;unicast;interleaved=0-1"];
    let s = c.request("SETUP", &format!("{base}trackID=0"), &video, "").await.unwrap();
    assert_eq!(s.status, 200);
    let session = format!("Session: {}", s.header("Session").unwrap().split(';').next().unwrap());
    let audio = ["Transport: RTP/AVP/TCP;unicast;interleaved=2-3", session.as_str()];
    assert_eq!(c.request("SETUP", &format!("{base}trackID=1"), &audio, "").await.unwrap().status, 200);
    assert_eq!(c.request("PLAY", &base, &[session.as_str()], "").await.unwrap().status, 200);
}

/// What a reader saw of the RTP packets on channels 0 (picture) and 2
/// (sound). A frame dropped before the reader is never packetized, so it
/// leaves no hole in the sequence numbers: a loss shows in the RTP times.
#[derive(Default)]
struct Seen {
    packets: [u64; 2],
    last_time: [Option<u32>; 2],
    /// Steps in RTP time longer than one frame (3600 ticks of picture, 160
    /// of sound): frames missing.
    gaps: [u64; 2],
    /// Whether the first picture packet after each gap starts a keyframe
    /// (or the parameter sets before one).
    resumed_on_keyframe: Vec<bool>,
}

impl Seen {
    fn packet(&mut self, channel: u8, rtp: &[u8]) {
        let (track, step) = match channel {
            0 => (0, 3600),
            2 => (1, 160),
            _ => return,
        };
        let time = u32::from_be_bytes([rtp[4], rtp[5], rtp[6], rtp[7]]);
        self.packets[track] += 1;
        if let Some(last) = self.last_time[track]
            && time != last
            && time.wrapping_sub(last) > step + step / 2
        {
            self.gaps[track] += 1;
            if track == 0 {
                self.resumed_on_keyframe.push(starts_keyframe(&rtp[12..]));
            }
        }
        self.last_time[track] = Some(time);
    }
}

/// An H.264 RTP payload that begins a keyframe: an IDR, the parameter sets
/// sent ahead of one (alone or aggregated), or the first fragment of an IDR.
fn starts_keyframe(payload: &[u8]) -> bool {
    match payload[0] & 0x1f {
        5 | 7 | 8 | 24 => true,
        28 => payload[1] & 0x80 != 0 && payload[1] & 0x1f == 5,
        _ => false,
    }
}

/// Reads for `period`.
async fn read_for(c: &mut Client, seen: &mut Seen, period: Duration) {
    let until = Instant::now() + period;
    while Instant::now() < until {
        let Some((channel, rtp)) = c.interleaved().await else { break };
        seen.packet(channel, &rtp);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_reader_paused_5_s_loses_nothing() {
    let factory = Arc::new(Factory::default());
    factory.clip("cam", heavy());
    let hub = hub(factory);
    hub.upsert("cam", &["clip:cam"]).unwrap();
    let server = start(hub.clone(), config()).await;
    let mut c = Client::connect(server.addr).await;
    play(&mut c, &server.url("cam")).await;
    let mut seen = Seen::default();
    read_for(&mut c, &mut seen, Duration::from_secs(1)).await;
    // 5 s without reading a byte: 12.5 MB published meanwhile.
    tokio::time::sleep(Duration::from_secs(5)).await;
    read_for(&mut c, &mut seen, Duration::from_secs(4)).await;
    let dropped = hub.get("cam").unwrap().snapshot().consumers[0].dropped;
    assert_eq!((seen.gaps, dropped), ([0, 0], 0), "a 5 s pause is within a recorder's budget");
    // About 10 s of picture (84 packets of 1200 bytes a frame) and sound.
    assert!(seen.packets[0] > 8 * 25 * 84 && seen.packets[1] > 8 * 50, "{:?}", seen.packets);
}

/// A reader that shows what it reads (openIVR's detector, `analysis_url()`)
/// asks for the live budget with `queue=live`: paused 5 s, it resumes at a
/// keyframe instead of 5 s behind (with the default it lost nothing, the
/// test above, and its detections trailed the picture by as much). An
/// unknown class is refused rather than read as the default.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_reader_that_asks_for_the_live_queue_is_cut_to_a_keyframe() {
    let factory = Arc::new(Factory::default());
    factory.clip("cam", heavy());
    let hub = hub(factory);
    hub.upsert("cam", &["clip:cam"]).unwrap();
    let server = start(hub.clone(), config()).await;
    let mut c = Client::connect(server.addr).await;
    play(&mut c, &server.url("cam?queue=live")).await;
    let mut seen = Seen::default();
    read_for(&mut c, &mut seen, Duration::from_secs(1)).await;
    tokio::time::sleep(Duration::from_secs(5)).await;
    read_for(&mut c, &mut seen, Duration::from_secs(3)).await;
    let dropped = hub.get("cam").unwrap().snapshot().consumers[0].dropped;
    assert!(seen.gaps[0] > 0 && dropped > 0, "a 5 s pause is beyond the live 2 s: {:?}, {dropped}", seen.gaps);
    assert!(
        !seen.resumed_on_keyframe.is_empty() && seen.resumed_on_keyframe.iter().all(|k| *k),
        "the picture resumes at a keyframe after each gap: {:?}",
        seen.resumed_on_keyframe
    );

    let mut other = Client::connect(server.addr).await;
    for (query, status) in [("cam?video&queue=recorder", 200), ("cam?video&queue=realtime", 400)] {
        let d = other.request("DESCRIBE", &server.url(query), &["Accept: application/sdp"], "").await.unwrap();
        assert_eq!(d.status, status, "{query}: {d:?}");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_reader_paused_beyond_its_budget_resumes_at_a_keyframe() {
    let factory = Arc::new(Factory::default());
    factory.clip("cam", heavy());
    let mut drivers = Drivers::new();
    drivers.register(factory);
    let limits = Limits {
        recorder_queue: QueueBudget { max_bytes: 32 * 1024 * 1024, max_age: Duration::from_secs(1) },
        ..Limits::default()
    };
    let hub = Hub::new(drivers, limits);
    let logs = Logs::default();
    let writer = logs.clone();
    let subscriber = tracing_subscriber::fmt().with_ansi(false).with_writer(move || writer.clone()).finish();
    tracing::subscriber::set_global_default(subscriber).unwrap();
    hub.upsert("cam", &["clip:cam"]).unwrap();
    let server = start(hub.clone(), config()).await;
    let mut c = Client::connect(server.addr).await;
    play(&mut c, &server.url("cam")).await;
    let mut seen = Seen::default();
    read_for(&mut c, &mut seen, Duration::from_secs(1)).await;
    tokio::time::sleep(Duration::from_secs(5)).await;
    read_for(&mut c, &mut seen, Duration::from_secs(3)).await;
    let dropped = hub.get("cam").unwrap().snapshot().consumers[0].dropped;
    assert!(seen.gaps[0] > 0 && dropped > 0, "a 5 s pause is beyond a 1 s budget: {:?}, {dropped}", seen.gaps);
    assert!(
        !seen.resumed_on_keyframe.is_empty() && seen.resumed_on_keyframe.iter().all(|k| *k),
        "the picture resumes at a keyframe after each gap: {:?}",
        seen.resumed_on_keyframe
    );
    // Said once, not once a frame (at most once per 10 s per reader).
    let warnings = logs.lines("consumer fell behind its queue budget");
    assert_eq!(warnings.len(), 1, "{warnings:?}");
    assert!(warnings[0].contains("WARN") && warnings[0].contains("class=\"recorder\""), "{}", warnings[0]);
    assert!(warnings[0].contains("stream=\"cam\"") && warnings[0].contains("dropped="), "{}", warnings[0]);
}

/// Every log line written in this test binary.
#[derive(Clone, Default)]
struct Logs(Arc<Mutex<Vec<u8>>>);

impl Logs {
    fn lines(&self, needle: &str) -> Vec<String> {
        let text = String::from_utf8_lossy(&self.0.lock().unwrap()).into_owned();
        text.lines().filter(|l| l.contains(needle)).map(str::to_owned).collect()
    }
}

impl Write for Logs {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
