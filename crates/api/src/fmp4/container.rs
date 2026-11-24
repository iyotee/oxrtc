//! The containers a feed writes its frames in, behind one track choice and
//! one muxer (`super::muxer`): fragmented MP4 (`oxrtc-mp4`: MSE,
//! `stream.mp4`, `frame.mp4`, HLS's fMP4 segments) and MPEG-TS
//! (`oxrtc-mpegts`: HLS's TS segments). Each says which tracks it takes,
//! is built from the tracks chosen, and gives one frame's bytes at a time.

use bytes::Bytes;
use oxrtc_core::{Frame, TrackInfo};
use oxrtc_mp4::{Fragmenting, HevcEntry, Mp4Error, Options, Writer};

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
}

/// Fragmented MP4, one fragment per frame.
#[derive(Debug)]
pub(crate) struct Mp4(Writer);

impl Container for Mp4 {
    fn supported(track: &TrackInfo) -> bool {
        oxrtc_mp4::supported(track)
    }

    fn build(tracks: &[TrackInfo], hevc: HevcEntry) -> Result<Option<Self>, String> {
        match Writer::new(tracks, Options { fragmenting: Fragmenting::EveryFrame, hevc }) {
            Ok(writer) => Ok(Some(Self(writer))),
            Err(Mp4Error::MissingConfig(_)) => Ok(None),
            Err(error) => Err(error.to_string()),
        }
    }

    fn mime(&self) -> String {
        self.0.mime_type()
    }

    fn codecs(&self) -> &[String] {
        self.0.codecs()
    }

    fn init(&self) -> Bytes {
        self.0.init_segment().clone()
    }

    fn push(&mut self, track: usize, frame: &Frame) -> Result<Option<Bytes>, String> {
        self.0.push(track, frame).map(|f| f.map(|f| f.bytes)).map_err(|e| e.to_string())
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
}
