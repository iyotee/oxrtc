//! The containers a feed writes its frames in, behind one track choice and
//! one muxer (`super::muxer`): fragmented MP4 (`oxrtc-mp4`: MSE,
//! `stream.mp4`, `frame.mp4`, HLS's fMP4 segments) and MPEG-TS
//! (`oxrtc-mpegts`: HLS's TS segments). Each says which tracks it takes,
//! is built from the tracks chosen, and gives one frame's bytes at a time.

use std::time::Duration;

use bytes::Bytes;
use oxrtc_core::{Frame, TrackInfo};
use oxrtc_mp4::{Fragmenting, HevcEntry, Mp4Error, Options, Span, Writer};

/// What a muxer writes into.
pub(crate) trait Container: Sized + Send + 'static {
    /// Whether it takes `track`, or will once its configuration arrives.
    fn supported(track: &TrackInfo) -> bool;
    /// Built from the tracks chosen; `None` while a track's configuration
    /// (parameter sets found in band later) has not come.
    fn build(tracks: &[TrackInfo], hevc: HevcEntry) -> Result<Option<Self>, String>;
    /// Its MIME type with the tracks' codec strings.
    fn mime(&self) -> String;
    /// Each track's RFC 6381 codec string.
    fn codecs(&self) -> &[String];
    /// What a reader takes before any frame: an init segment, or nothing.
    fn init(&self) -> Bytes;
    /// One frame's bytes, if it makes any yet.
    fn push(&mut self, track: usize, frame: &Frame) -> Result<Option<Bytes>, String>;
    /// What a segment of it starts with besides its frames (MPEG-TS's
    /// tables), written when asked.
    fn tables(&mut self) -> Option<Bytes> {
        None
    }
    /// Track `track`'s init segment alone, its fragments read with it (fMP4).
    fn track_init(&self, _track: usize) -> Option<Bytes> {
        None
    }
    /// Where the last frame's bytes sit, as the container placed it: its
    /// start and end from the zero (fMP4's sample), when it says.
    fn last_span(&self) -> Option<(Duration, Duration)> {
        None
    }
    /// A gap in the source's times longer than this (a stalled source) is
    /// closed in what it writes (`super::gaps`); `None` keeps the times.
    const CLOSES_GAPS_OVER: Option<Duration> = None;
}

/// Fragmented MP4, one fragment per frame.
#[derive(Debug)]
pub(crate) struct Mp4 {
    writer: Writer,
    last: Option<Span>,
}

impl Mp4 {
    fn with(tracks: &[TrackInfo], hevc: HevcEntry) -> Result<Option<Self>, String> {
        match Writer::new(tracks, Options { fragmenting: Fragmenting::EveryFrame, hevc }) {
            Ok(writer) => Ok(Some(Self { writer, last: None })),
            Err(Mp4Error::MissingConfig(_)) => Ok(None),
            Err(error) => Err(error.to_string()),
        }
    }
}

impl Container for Mp4 {
    fn supported(track: &TrackInfo) -> bool {
        oxrtc_mp4::supported(track)
    }

    fn build(tracks: &[TrackInfo], hevc: HevcEntry) -> Result<Option<Self>, String> {
        Self::with(tracks, hevc)
    }

    fn mime(&self) -> String {
        self.writer.mime_type()
    }

    fn codecs(&self) -> &[String] {
        self.writer.codecs()
    }

    fn init(&self) -> Bytes {
        self.writer.init_segment().clone()
    }

    fn push(&mut self, track: usize, frame: &Frame) -> Result<Option<Bytes>, String> {
        let fragment = self.writer.push(track, frame).map_err(|e| e.to_string())?;
        self.last = fragment.as_ref().map(|f| f.span);
        Ok(fragment.map(|f| f.bytes))
    }

    fn track_init(&self, track: usize) -> Option<Bytes> {
        self.writer.track_init_segment(track).ok()
    }

    fn last_span(&self) -> Option<(Duration, Duration)> {
        self.last.map(|span| (span.start(), span.start() + span.duration()))
    }
}

/// HLS's fMP4 (and DASH's): [`Mp4`], where the samples say where each
/// frame sits (the parts are closed at their ends) and a stalled source's
/// gap is closed ([`HLS_CLOSES_GAPS_OVER`]).
#[derive(Debug)]
pub(crate) struct HlsMp4(Mp4);

/// A gap in a source's times longer than this is closed in HLS's segments
/// (D47, F6P); a shorter one (a frame or a few lost) is left to the
/// container: fMP4 fills it as for MSE, MPEG-TS keeps it.
pub(crate) const HLS_CLOSES_GAPS_OVER: Option<Duration> = Some(Duration::from_secs(1));

impl Container for HlsMp4 {
    fn supported(track: &TrackInfo) -> bool {
        Mp4::supported(track)
    }

    fn build(tracks: &[TrackInfo], hevc: HevcEntry) -> Result<Option<Self>, String> {
        Ok(Mp4::with(tracks, hevc)?.map(Self))
    }

    const CLOSES_GAPS_OVER: Option<Duration> = HLS_CLOSES_GAPS_OVER;

    fn mime(&self) -> String {
        self.0.mime()
    }

    fn codecs(&self) -> &[String] {
        self.0.codecs()
    }

    fn init(&self) -> Bytes {
        self.0.init()
    }

    fn push(&mut self, track: usize, frame: &Frame) -> Result<Option<Bytes>, String> {
        self.0.push(track, frame)
    }

    fn track_init(&self, track: usize) -> Option<Bytes> {
        self.0.track_init(track)
    }

    fn last_span(&self) -> Option<(Duration, Duration)> {
        self.0.last_span()
    }
}

/// MPEG-TS, one PES per frame.
#[derive(Debug)]
pub(crate) struct Ts {
    writer: oxrtc_mpegts::Writer,
    codecs: Vec<String>,
}

impl Container for Ts {
    fn supported(track: &TrackInfo) -> bool {
        oxrtc_mpegts::supported(track)
    }

    fn build(tracks: &[TrackInfo], _hevc: HevcEntry) -> Result<Option<Self>, String> {
        let mut codecs = Vec::with_capacity(tracks.len());
        for track in tracks {
            match oxrtc_mp4::codec_string(track) {
                Ok(codec) => codecs.push(codec),
                Err(Mp4Error::MissingConfig(_)) => return Ok(None),
                Err(error) => return Err(error.to_string()),
            }
        }
        let writer = oxrtc_mpegts::Writer::new(tracks).map_err(|e| e.to_string())?;
        Ok(Some(Self { writer, codecs }))
    }

    fn mime(&self) -> String {
        format!("video/mp2t; codecs=\"{}\"", self.codecs.join(","))
    }

    fn codecs(&self) -> &[String] {
        &self.codecs
    }

    fn init(&self) -> Bytes {
        Bytes::new()
    }

    fn push(&mut self, track: usize, frame: &Frame) -> Result<Option<Bytes>, String> {
        self.writer.push(track, frame).map(Some).map_err(|e| e.to_string())
    }

    fn tables(&mut self) -> Option<Bytes> {
        Some(self.writer.tables())
    }

    const CLOSES_GAPS_OVER: Option<Duration> = HLS_CLOSES_GAPS_OVER;
}
