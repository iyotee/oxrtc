//! JPEG stills: from an H.265 stream through real FFmpeg (skipped when absent),
//! scaled and turned; a Motion JPEG frame passed through untouched when asked
//! as it is, and encoded again when asked another shape.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing, clippy::panic)]

mod common;

use std::path::Path;
use std::time::Duration;

use bytes::Bytes;
use common::{Clip, Harness, encode, ffmpeg, jpeg_size};
use oxrtc_core::Codec;
use oxrtc_core::jpeg::{Rotate, Shape};
use oxrtc_ffmpeg::{FfmpegConfig, StillError, Stills};

#[tokio::test(flavor = "multi_thread")]
async fn an_h265_keyframe_becomes_a_jpeg_320_wide() {
    let Some(bin) = ffmpeg() else { return };
    let source = encode(&bin, Codec::H265, "640x360", 2).await;
    let h = Harness::start(
        &bin,
        vec![("cam", Clip::from_annex_b(Codec::H265, &source))],
        &[],
        4,
        Duration::from_secs(20),
        None,
    )
    .await;
    let stream = h.stream("camera", &["clip:cam"]);
    let stills = Stills::new(h.config.clone());
    let jpeg = stills.jpeg(stream.clone(), width(320)).await.unwrap();
    assert_eq!(&jpeg[..2], &[0xff, 0xd8]);
    assert_eq!(&jpeg[jpeg.len() - 2..], &[0xff, 0xd9]);
    assert_eq!(jpeg_size(&jpeg), Some((320, 180)));
    // At its own size, and twice at once.
    let (a, b) =
        tokio::join!(stills.jpeg(stream.clone(), Shape::default()), stills.jpeg(stream.clone(), Shape::default()));
    assert_eq!(jpeg_size(&a.unwrap()), Some((640, 360)));
    assert_eq!(jpeg_size(&b.unwrap()), Some((640, 360)));
    // go2rtc's `height`, `width` and `height`, and `rotate`: scaled, then turned.
    let cases = [
        (Shape { height: Some(90), ..Shape::default() }, (160, 90)),
        (Shape { width: Some(320), height: Some(100), rotate: Rotate::None }, (320, 100)),
        (Shape { width: Some(320), rotate: Rotate::Quarter, ..Shape::default() }, (180, 320)),
        (Shape { rotate: Rotate::Half, ..Shape::default() }, (640, 360)),
        (Shape { rotate: Rotate::ThreeQuarters, ..Shape::default() }, (360, 640)),
    ];
    for (shape, size) in cases {
        assert_eq!(jpeg_size(&stills.jpeg(stream.clone(), shape).await.unwrap()), Some(size), "{shape:?}");
    }
}

fn width(w: u32) -> Shape {
    Shape { width: Some(w), ..Shape::default() }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_jpeg_frame_asked_another_shape_is_encoded_again() {
    let Some(bin) = ffmpeg() else { return };
    let dir = std::env::temp_dir().join(format!("oxrtc-still-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("picture.jpg");
    let made = std::process::Command::new(&bin)
        .args(["-v", "error", "-y", "-f", "lavfi", "-i", "testsrc=size=640x360", "-frames:v", "1"])
        .arg(&file)
        .status()
        .unwrap();
    assert!(made.success());
    let picture = Bytes::from(std::fs::read(&file).unwrap());
    let _ = std::fs::remove_dir_all(&dir);
    let h =
        Harness::start(&bin, vec![("webcam", Clip::jpeg(picture.clone()))], &[], 4, Duration::from_secs(5), None).await;
    let stream = h.stream("webcam", &["clip:webcam"]);
    let stills = Stills::new(h.config.clone());
    assert_eq!(stills.jpeg(stream.clone(), Shape::default()).await.unwrap(), picture, "as it is");
    let turned = Shape { width: Some(320), rotate: Rotate::Quarter, ..Shape::default() };
    assert_eq!(jpeg_size(&stills.jpeg(stream, turned).await.unwrap()), Some((180, 320)));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_jpeg_frame_is_returned_as_it_is() {
    let frame = Bytes::from_static(&[0xff, 0xd8, 0xff, 0xe0, 0, 2, 0xff, 0xd9]);
    let missing = Path::new("no-such-ffmpeg-binary");
    let h =
        Harness::start(missing, vec![("mjpeg", Clip::jpeg(frame.clone()))], &[], 4, Duration::from_secs(5), None).await;
    let stream = h.stream("webcam", &["clip:mjpeg"]);
    let stills = Stills::new(FfmpegConfig { bin: missing.to_owned(), ..FfmpegConfig::default() });
    assert_eq!(stills.jpeg(stream, Shape::default()).await.unwrap(), frame);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_missing_ffmpeg_and_bad_widths_are_errors() {
    let source = Bytes::from_static(&[0, 0, 0, 2, 0x65, 0x88]);
    let missing = Path::new("no-such-ffmpeg-binary");
    let mut clip = Clip::jpeg(source);
    clip.track = oxrtc_core::TrackInfo::video(Codec::H264);
    let h = Harness::start(missing, vec![("cam", clip)], &[], 4, Duration::from_secs(5), None).await;
    let stream = h.stream("camera", &["clip:cam"]);
    let stills = Stills::new(FfmpegConfig { bin: missing.to_owned(), ..FfmpegConfig::default() });
    assert!(matches!(stills.jpeg(stream.clone(), Shape::default()).await, Err(StillError::Spawn(_))));
    assert_eq!(stills.jpeg(stream.clone(), width(0)).await, Err(StillError::Width));
    assert_eq!(stills.jpeg(stream.clone(), width(100_000)).await, Err(StillError::Width));
    let high = Shape { height: Some(7681), ..Shape::default() };
    assert_eq!(stills.jpeg(stream, high).await, Err(StillError::Width));
}
