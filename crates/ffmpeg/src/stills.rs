//! JPEG stills (`/api/frame.jpeg`): the last keyframe of a stream, decoded and
//! encoded once by FFmpeg, fed on its standard input, scaled and turned as
//! asked ([`Shape`]). A Motion JPEG picture is passed on as it is when asked
//! as it is (with the standard Huffman tables written in when it has none),
//! unless its header is one go2rtc's `FixJPEG` encodes again (a comment or
//! an unknown segment first: [`wants_encoding_again`]), and encoded again
//! when asked otherwise - go2rtc 1.9.15 ignores `width`, `height` and
//! `rotate` for it.

use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use bytes::{BufMut, Bytes, BytesMut};
use oxrtc_core::frame::{h264_nal_type, h265_nal_type};
use oxrtc_core::jpeg::{Rotate, Shape, wants_encoding_again, with_huffman_tables};
use oxrtc_core::{Codec, CodecParams, ConsumerTag, Event, Frame, MediaWant, Stream, SubscribeError, Want};
use thiserror::Error;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};

use crate::FfmpegConfig;
use crate::limiter::Limiter;
use crate::process::{Launch, Tail, command, drain, spawn};
use crate::split::quote_split;
use crate::templates::Templates;

/// Stills encoded at once.
const MAX_CONCURRENT: usize = 2;
/// Longest a still may take, from subscribing to the last byte of JPEG.
const STILL_TIMEOUT: Duration = Duration::from_secs(10);
/// Largest JPEG read back.
const MAX_JPEG_BYTES: usize = 8 * 1024 * 1024;
/// Widest (and highest) still that may be asked for.
pub const MAX_WIDTH: u32 = 7680;

/// Why a still could not be made.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum StillError {
    /// The stream has no source offering video.
    #[error("cannot read the stream: {0}")]
    Subscribe(#[from] SubscribeError),
    /// The stream was closed before a keyframe arrived.
    #[error("stream closed")]
    Closed,
    /// No keyframe, or no JPEG, within the time limit.
    #[error("no still within {0:?}")]
    Timeout(Duration),
    /// A video codec FFmpeg is not fed here.
    #[error("cannot make a still from {0}")]
    Unsupported(&'static str),
    /// A width or a height of 0 or above [`MAX_WIDTH`].
    #[error("width and height must be 1 to {MAX_WIDTH}")]
    Width,
    /// The configuration's templates cannot be used.
    #[error("ffmpeg configuration: {0}")]
    Config(String),
    /// FFmpeg could not be started.
    #[error("ffmpeg: {0}")]
    Spawn(String),
    /// FFmpeg failed; its last error line, redacted.
    #[error("ffmpeg failed: {0}")]
    Ffmpeg(String),
    /// FFmpeg wrote more than the JPEG limit.
    #[error("ffmpeg output exceeds {MAX_JPEG_BYTES} bytes")]
    TooLarge,
}

/// Makes JPEG stills of streams.
pub struct Stills {
    launch: Launch,
    templates: Templates,
    limiter: Limiter,
}

impl std::fmt::Debug for Stills {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Stills").field("launch", &self.launch).finish_non_exhaustive()
    }
}

impl Stills {
    /// Stills made with `config.bin` and its `global` and `mjpeg` templates;
    /// at most two at once (fewer if `max_processes` is lower).
    pub fn new(config: FfmpegConfig) -> Self {
        Self {
            templates: Templates::new(&config.templates),
            limiter: Limiter::new(MAX_CONCURRENT.min(config.max_processes)),
            launch: Launch::new(&config),
        }
    }

    /// The stream's latest keyframe as a JPEG, shaped as `shape` asks: a side
    /// given alone keeps the aspect ratio. A Motion JPEG picture asked as it
    /// is comes as it is.
    pub async fn jpeg(&self, stream: Arc<Stream>, shape: Shape) -> Result<Bytes, StillError> {
        if [shape.width, shape.height].into_iter().flatten().any(|side| side == 0 || side > MAX_WIDTH) {
            return Err(StillError::Width);
        }
        tokio::time::timeout(STILL_TIMEOUT, self.make(stream, shape))
            .await
            .map_err(|_| StillError::Timeout(STILL_TIMEOUT))?
    }

    async fn make(&self, stream: Arc<Stream>, shape: Shape) -> Result<Bytes, StillError> {
        let (codec, params, frame) = keyframe(&stream).await?;
        // A Motion JPEG picture asked as it is is encoded again only when
        // go2rtc's FixJPEG would (a header the tools that sniff a file's type
        // do not read), and served as it came should that fail, as go2rtc does.
        let as_it_came = codec == Codec::Jpeg && shape.is_unchanged();
        let (format, input) = match codec {
            Codec::Jpeg if as_it_came && !wants_encoding_again(&frame.data) => {
                return Ok(with_huffman_tables(&frame.data));
            }
            Codec::Jpeg => ("mjpeg", with_huffman_tables(&frame.data)),
            Codec::H264 => ("h264", annex_b(&frame, &params, codec)),
            Codec::H265 => ("hevc", annex_b(&frame, &params, codec)),
            other => return Err(StillError::Unsupported(other.as_str())),
        };
        let args = self.args(format, shape)?;
        let _permit = self.limiter.acquire().await.ok_or(StillError::Closed)?;
        match self.encode(&args, input).await {
            Err(error) if as_it_came => {
                tracing::debug!(%error, "still: a Motion JPEG picture could not be encoded again, served as it came");
                Ok(with_huffman_tables(&frame.data))
            }
            made => made,
        }
    }

    /// go2rtc's command line for a still (`internal/ffmpeg/jpeg.go`): the
    /// scale first, then the turn.
    fn args(&self, format: &str, shape: Shape) -> Result<Vec<String>, StillError> {
        let split = |name: &str, fallback: &str| {
            quote_split(self.templates.get(name).unwrap_or(fallback)).map_err(|e| StillError::Config(e.to_string()))
        };
        let mut args = split("global", "-hide_banner")?;
        args.extend(["-f", format, "-i", "pipe:0", "-frames:v", "1"].map(String::from));
        let mut filters = Vec::new();
        if shape.width.is_some() || shape.height.is_some() {
            let side = |s: Option<u32>| s.map_or_else(|| "-1".to_owned(), |s| s.to_string());
            filters.push(format!("scale={}:{}", side(shape.width), side(shape.height)));
        }
        match shape.rotate {
            Rotate::None => {}
            Rotate::Quarter => filters.push("transpose=1".to_owned()),
            Rotate::Half => filters.push("transpose=1,transpose=1".to_owned()),
            Rotate::ThreeQuarters => filters.push("transpose=2".to_owned()),
        }
        if !filters.is_empty() {
            args.extend(["-vf".to_owned(), filters.join(",")]);
        }
        args.extend(split("mjpeg", "-c:v mjpeg")?);
        args.extend(["-f", "image2", "pipe:1"].map(String::from));
        Ok(args)
    }

    async fn encode(&self, args: &[String], input: Bytes) -> Result<Bytes, StillError> {
        // Dropped on every way out (the 10 s limit included): FFmpeg is killed.
        let mut process = spawn(command(&self.launch, args, Stdio::piped(), Stdio::piped()), &self.launch.bin)
            .await
            .map_err(StillError::Spawn)?;
        let child = &mut process.child;
        let (Some(mut stdin), Some(stdout), Some(stderr)) =
            (child.stdin.take(), child.stdout.take(), child.stderr.take())
        else {
            return Err(StillError::Spawn("no pipes".into()));
        };
        let tail = Tail::default();
        let write = async move {
            // FFmpeg may stop reading early (a bad frame): its exit says why.
            let _ = stdin.write_all(&input).await;
            drop(stdin);
        };
        let side = async { tokio::join!(write, drain(stderr, tail.clone(), "still")) };
        let read = async {
            let out = read_bounded(stdout, MAX_JPEG_BYTES).await;
            if out.is_err() {
                // The whole tree: a launcher's child holds the error pipe open.
                process.kill();
            }
            out
        };
        let (out, _) = tokio::join!(read, side);
        let status = process.child.wait().await.map_err(|e| StillError::Ffmpeg(e.to_string()))?;
        let out = out?;
        let why = || tail.joined().unwrap_or_else(|| "no error output".into());
        if !status.success() {
            return Err(StillError::Ffmpeg(format!("{status}: {}", why())));
        }
        if !(out.starts_with(&[0xff, 0xd8]) && out.ends_with(&[0xff, 0xd9])) {
            return Err(StillError::Ffmpeg(format!("not a JPEG ({} bytes): {}", out.len(), why())));
        }
        Ok(out)
    }
}

/// The first video keyframe of the stream - at once from the GOP cache when
/// the source runs - with its track's codec and parameters.
async fn keyframe(stream: &Arc<Stream>) -> Result<(Codec, CodecParams, Arc<Frame>), StillError> {
    let want = Want { video: MediaWant::Any, audio: MediaWant::Skip };
    let mut subscription = stream.subscribe(&want, ConsumerTag::new("still")).await?;
    let mut tracks = None;
    loop {
        match subscription.recv().await.map_err(|_| StillError::Closed)? {
            Event::Tracks(list) => tracks = Some(list),
            Event::Frame { track, frame } if frame.keyframe => {
                if let Some(info) = tracks.as_ref().and_then(|t| t.get(track)) {
                    return Ok((info.codec, info.params.clone(), frame));
                }
            }
            Event::Frame { .. } | Event::Discontinuity => {}
        }
    }
}

/// The frame in Annex B, preceded by its parameter sets when it lacks them.
fn annex_b(frame: &Frame, params: &CodecParams, codec: Codec) -> Bytes {
    let has_sets = frame.nal_units().any(|nal| match codec {
        Codec::H264 => h264_nal_type(nal) == Some(7),
        _ => h265_nal_type(nal) == Some(33),
    });
    let sets: Vec<&Bytes> = match params {
        _ if has_sets => Vec::new(),
        CodecParams::H264 { sps, pps } => vec![sps, pps],
        CodecParams::H265 { vps, sps, pps } => vec![vps, sps, pps],
        _ => Vec::new(),
    };
    let body = frame.to_annex_b();
    let mut out = BytesMut::with_capacity(body.len() + sets.iter().map(|s| s.len() + 4).sum::<usize>());
    for set in sets {
        out.put_slice(&[0, 0, 0, 1]);
        out.put_slice(set);
    }
    out.put_slice(&body);
    out.freeze()
}

/// Reads `reader` to its end, refusing more than `limit` bytes.
async fn read_bounded(reader: impl AsyncRead + Unpin, limit: usize) -> Result<Bytes, StillError> {
    let mut out = Vec::new();
    let read = reader.take(limit as u64 + 1).read_to_end(&mut out).await;
    read.map_err(|e| StillError::Ffmpeg(e.to_string()))?;
    if out.len() > limit {
        return Err(StillError::TooLarge);
    }
    Ok(Bytes::from(out))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parameter_sets_are_prepended_only_when_missing() {
        let params = CodecParams::H264 { sps: Bytes::from_static(&[0x67, 1]), pps: Bytes::from_static(&[0x68, 2]) };
        let idr = Frame::new(0, 0, true, Bytes::from_static(&[0, 0, 0, 2, 0x65, 0x88]));
        assert_eq!(
            &annex_b(&idr, &params, Codec::H264)[..],
            &[0, 0, 0, 1, 0x67, 1, 0, 0, 0, 1, 0x68, 2, 0, 0, 0, 1, 0x65, 0x88]
        );
        let full = Frame::new(0, 0, true, Bytes::from_static(&[0, 0, 0, 2, 0x67, 9, 0, 0, 0, 2, 0x65, 0x88]));
        assert_eq!(&annex_b(&full, &params, Codec::H264)[..], &[0, 0, 0, 1, 0x67, 9, 0, 0, 0, 1, 0x65, 0x88]);
        let hevc = CodecParams::H265 {
            vps: Bytes::from_static(&[0x40, 1]),
            sps: Bytes::from_static(&[0x42, 1]),
            pps: Bytes::from_static(&[0x44, 1]),
        };
        let irap = Frame::new(0, 0, true, Bytes::from_static(&[0, 0, 0, 2, 0x26, 1]));
        assert_eq!(annex_b(&irap, &hevc, Codec::H265).len(), 4 * 6);
    }

    #[test]
    fn arguments_scale_and_turn_as_go2rtc_and_write_one_jpeg() {
        let stills = Stills::new(FfmpegConfig::default());
        let width = |w| Shape { width: Some(w), ..Shape::default() };
        assert_eq!(
            stills.args("hevc", width(320)).unwrap().join(" "),
            "-hide_banner -v error -f hevc -i pipe:0 -frames:v 1 -vf scale=320:-1 -c:v mjpeg -f image2 pipe:1"
        );
        assert_eq!(
            stills.args("h264", Shape::default()).unwrap().join(" "),
            "-hide_banner -v error -f h264 -i pipe:0 -frames:v 1 -c:v mjpeg -f image2 pipe:1"
        );
        let cases = [
            (Shape { height: Some(240), ..Shape::default() }, "scale=-1:240"),
            (Shape { width: Some(320), height: Some(200), rotate: Rotate::Quarter }, "scale=320:200,transpose=1"),
            (Shape { rotate: Rotate::Half, ..Shape::default() }, "transpose=1,transpose=1"),
            (Shape { rotate: Rotate::ThreeQuarters, ..Shape::default() }, "transpose=2"),
        ];
        for (shape, filters) in cases {
            let line = stills.args("mjpeg", shape).unwrap().join(" ");
            assert!(line.contains(&format!("-f mjpeg -i pipe:0 -frames:v 1 -vf {filters} -c:v mjpeg")), "{line}");
        }
    }

    #[tokio::test]
    async fn output_is_bounded() {
        let big = [0u8; 100];
        assert_eq!(read_bounded(&big[..], 99).await, Err(StillError::TooLarge));
        assert_eq!(read_bounded(&big[..], 100).await.unwrap().len(), 100);
    }
}
