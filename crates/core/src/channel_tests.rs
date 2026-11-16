#![allow(clippy::unwrap_used, clippy::indexing_slicing)]

use std::io::Write;
use std::sync::{Arc, Mutex};

use bytes::Bytes;

use super::*;
use crate::media::Codec;

/// Every log line written on this thread while the guard lives.
#[derive(Clone, Default)]
struct Logs(Arc<Mutex<Vec<u8>>>);

impl Logs {
    fn capture() -> (Self, tracing::subscriber::DefaultGuard) {
        let logs = Self::default();
        let writer = logs.clone();
        let subscriber = tracing_subscriber::fmt().with_ansi(false).with_writer(move || writer.clone()).finish();
        let guard = tracing::subscriber::set_default(subscriber);
        // A callsite another test thread registered first, with no
        // subscriber of its own, must not stay disabled for this one.
        tracing::callsite::rebuild_interest_cache();
        (logs, guard)
    }

    fn text(&self) -> String {
        String::from_utf8_lossy(&self.0.lock().unwrap()).into_owned()
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

/// An H.264 frame of `size` bytes: one NAL unit behind its length.
fn h264(size: usize, key: bool) -> Bytes {
    let mut data = u32::try_from(size - 4).unwrap().to_be_bytes().to_vec();
    data.push(if key { 0x65 } else { 0x41 });
    data.resize(size, 0x88);
    Bytes::from(data)
}

/// A group of pictures: a keyframe and `deltas` frames, each of `size` bytes.
fn group(channel: &Channel, at: &mut i64, size: usize, deltas: usize) {
    for n in 0..=deltas {
        channel.publish(Frame::new(0, *at, n == 0, h264(size, n == 0))).unwrap();
        *at += 3600;
    }
}

const OVERFLOW: &str = "group of pictures larger than the keyframe cache";

#[test]
fn a_group_of_pictures_over_the_cache_is_said_once_per_source() {
    let (logs, _guard) = Logs::capture();
    let limits = Limits { gop_cache_bytes: 1000, ..Limits::default() };
    let channel = Channel::new(&limits, Instant::now(), "rtsp://***@cam/1".into());
    channel.set_tracks(vec![TrackInfo::video(Codec::H264)]).unwrap();
    let mut at = 0;
    // Three frames of 400 bytes: the third outgrows the cache.
    group(&channel, &mut at, 400, 2);
    assert!(channel.gop().is_empty(), "the group is not cached");
    let first = logs.text();
    assert_eq!(first.matches(OVERFLOW).count(), 1, "{first}");
    assert!(first.contains("WARN"), "{first}");
    assert!(first.contains("source=rtsp://***@cam/1"), "{first}");
    // The group as it outgrew the cache: its three frames and their bytes
    // (the count left out the frame that outgrew it, the bytes did not).
    assert!(first.contains("frames=3 bytes=1200 limit=1000"), "{first}");
    // Every later group, a reconnection included, is the same source.
    group(&channel, &mut at, 400, 4);
    channel.reset();
    channel.set_tracks(vec![TrackInfo::video(Codec::H264)]).unwrap();
    group(&channel, &mut at, 600, 1);
    assert_eq!(logs.text().matches(OVERFLOW).count(), 1, "{}", logs.text());
    // A group that fits is cached again, and said nothing about.
    group(&channel, &mut at, 300, 2);
    assert_eq!(channel.gop().len(), 3);
    // Another source has its own warning.
    let other = Channel::new(&limits, Instant::now(), "rtsp://***@cam/2".into());
    other.set_tracks(vec![TrackInfo::video(Codec::H264)]).unwrap();
    group(&other, &mut at, 2000, 0);
    let text = logs.text();
    assert_eq!(text.matches(OVERFLOW).count(), 2, "{text}");
    assert!(text.contains("source=rtsp://***@cam/2"), "{text}");
    // A keyframe alone over the cache is a group of one frame, not of none.
    assert!(text.contains("frames=1 bytes=2000 limit=1000"), "{text}");
}

#[test]
fn a_disabled_cache_says_nothing() {
    let (logs, _guard) = Logs::capture();
    let limits = Limits { gop_cache_bytes: 0, ..Limits::default() };
    let channel = Channel::new(&limits, Instant::now(), "cam".into());
    channel.set_tracks(vec![TrackInfo::video(Codec::H264)]).unwrap();
    group(&channel, &mut 0, 4000, 3);
    assert!(channel.gop().is_empty());
    assert!(!logs.text().contains(OVERFLOW), "{}", logs.text());
}
