//! A subscription's events as what an MSE `SourceBuffer` (F3) and a player of
//! `stream.mp4` (F4) read: the MIME type once, an init segment, then one
//! fragment per frame, written by `oxrtc-mp4`. No I/O: the egress sends what
//! it returns.
//!
//! Times are the core's timeline (D12) moved back by one constant, the time
//! of the reader's first frame written, so that its media starts at zero
//! (D40): go2rtc's player plays from `currentTime` 0 and seeks only once
//! 5 s are buffered, so a timeline that starts minutes in left it 5 s behind
//! the live edge, playing at five times the speed to catch up. Every track
//! moves by the same time, in its own clock, so they stay in step.

use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use oxrtc_core::{Frame, MediaKind, TrackInfo, Want};
use oxrtc_mp4::HevcEntry;

use super::container::Container;
#[cfg(test)]
use super::container::Mp4;

/// What the egress sends, in order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Out {
    /// `{"type":"mse","value":<mime>}`: the type the player's `SourceBuffer`
    /// is made with. Sent once, before the first init segment.
    Mime(String),
    /// An init segment: the first one after the MIME type, and another one
    /// whenever the stream comes back with other parameter sets of the same
    /// codecs.
    Init(Bytes),
    /// A fragment.
    Binary(Bytes),
}

/// Why the muxer cannot go on.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub(crate) enum MuxError {
    /// The stream has no track the list takes (go2rtc's words).
    #[error("codecs not matched")]
    NoCodec,
    /// The stream came back with tracks of other codecs than the player's
    /// `SourceBuffer` was made for, which it cannot change to.
    #[error("the stream's codecs changed")]
    CodecsChanged,
    /// The writer refused the tracks.
    #[error("{0}")]
    Writer(String),
}

/// The state of one reader's fMP4 ([`Muxer`]) or MPEG-TS.
#[derive(Debug)]
pub(crate) struct MuxerOf<C> {
    want: Want,
    hevc: HevcEntry,
    writer: Option<C>,
    /// The subscription's tracks the writer was built from, as they were.
    chosen: Vec<TrackInfo>,
    /// For each track of the subscription's latest list, its writer track.
    map: Vec<Option<usize>>,
    /// The writer's video track, whose keyframe the fragments start at.
    video: Option<usize>,
    /// The MIME type the player was told.
    mime: Option<String>,
    /// Frames are left out until the video's next keyframe: at the start,
    /// after a loss, and after a frame the writer refused.
    waiting: bool,
    /// The reader's zero: the first frame written, its time and its
    /// track's clock rate.
    zero: Option<(i64, u32)>,
    /// Frames left out while waiting.
    pub(crate) skipped: u64,
    /// The time of the latest frame written, from the zero.
    pub(crate) latest: Duration,
    /// Fragments written.
    pub(crate) fragments: u64,
    /// Whether the latest frame written starts a picture a decoder can begin
    /// at (the video's keyframe; any frame of a feed with no video).
    pub(crate) last_key: bool,
    /// Whether the latest frame written is of the track whose times cut
    /// segments (the video; any frame of a feed with no video).
    pub(crate) last_lead: bool,
}

/// The fragmented MP4 muxer of MSE and `stream.mp4`, as its tests name it.
#[cfg(test)]
pub(crate) type Muxer = MuxerOf<Mp4>;

impl<C: Container> MuxerOf<C> {
    pub(crate) fn new(want: Want, hevc: HevcEntry) -> Self {
        Self {
            want,
            hevc,
            writer: None,
            chosen: Vec::new(),
            map: Vec::new(),
            video: None,
            mime: None,
            zero: None,
            waiting: true,
            skipped: 0,
            latest: Duration::ZERO,
            fragments: 0,
            last_key: false,
            last_lead: false,
        }
    }

    /// Whether the player was told the MIME type.
    pub(crate) fn started(&self) -> bool {
        self.mime.is_some()
    }

    /// A new track list. The first video and the first audio track the list
    /// takes are written; a track whose configuration has not come yet
    /// (parameter sets found in band later) holds the writer back until a
    /// list that carries it.
    pub(crate) fn tracks(&mut self, tracks: &Arc<[TrackInfo]>) -> Result<Vec<Out>, MuxError> {
        let mut map = vec![None; tracks.len()];
        let mut chosen = Vec::new();
        for kind in [MediaKind::Video, MediaKind::Audio] {
            let pick =
                tracks.iter().position(|t| t.kind == kind && self.want.get(kind).accepts(t.codec) && C::supported(t));
            if let (Some(at), Some(track)) = (pick, pick.and_then(|at| tracks.get(at))) {
                if let Some(slot) = map.get_mut(at) {
                    *slot = Some(chosen.len());
                }
                chosen.push(track.clone());
            }
        }
        if chosen.is_empty() {
            return Err(MuxError::NoCodec);
        }
        self.map = map;
        if self.writer.is_some() && chosen == self.chosen {
            // The same tracks published again: the writer goes on.
            return Ok(Vec::new());
        }
        let writer = match C::build(&chosen, self.hevc) {
            Ok(Some(writer)) => writer,
            Ok(None) => {
                self.writer = None;
                self.chosen = Vec::new();
                return Ok(Vec::new());
            }
            Err(error) => return Err(MuxError::Writer(error)),
        };
        let mime = writer.mime();
        let mut out = Vec::with_capacity(2);
        match &self.mime {
            None => out.push(Out::Mime(mime.clone())),
            Some(told) if *told != mime => return Err(MuxError::CodecsChanged),
            Some(_) => {}
        }
        out.push(Out::Init(writer.init()));
        self.video = chosen.iter().position(|t| t.kind == MediaKind::Video);
        self.mime = Some(mime);
        self.chosen = chosen;
        self.writer = Some(writer);
        // A new init segment: its samples start at a keyframe.
        self.waiting = true;
        Ok(out)
    }

    /// A frame of track `track` of the latest list: its fragment, or nothing
    /// when the frame is not written (a track not chosen, no writer yet, or
    /// waiting for a keyframe).
    pub(crate) fn frame(&mut self, track: usize, frame: &Frame) -> Option<Bytes> {
        let at = self.map.get(track).copied().flatten()?;
        let clock = self.chosen.get(at)?.clock_rate;
        let writer = self.writer.as_mut()?;
        if self.waiting {
            if self.video.is_some() && !(self.video == Some(at) && frame.keyframe) {
                self.skipped += 1;
                return None;
            }
            self.waiting = false;
        }
        let (zero, zero_clock) = *self.zero.get_or_insert((frame.pts, clock));
        let Some(pts) = moved(frame.pts, clock, zero, zero_clock) else {
            // Before the reader's zero: a sound that left the source just
            // before the keyframe the reader starts at.
            self.skipped += 1;
            return None;
        };
        let moved = Frame { pts, ..frame.clone() };
        let key = self.video.is_none() || (self.video == Some(at) && frame.keyframe);
        match writer.push(at, &moved) {
            Ok(Some(bytes)) => {
                self.fragments += 1;
                self.latest = at_time(pts, clock);
                self.last_key = key;
                self.last_lead = self.video.is_none() || self.video == Some(at);
                Some(bytes)
            }
            Ok(None) => None,
            Err(error) => {
                // A picture lost breaks the ones after it until a keyframe.
                tracing::debug!("api: a frame was not written: {error}");
                if self.video == Some(at) {
                    self.waiting = true;
                }
                None
            }
        }
    }

    /// What a segment starts with besides its frames (MPEG-TS's tables),
    /// once the container exists.
    pub(crate) fn tables(&mut self) -> Option<Bytes> {
        self.writer.as_mut().and_then(C::tables)
    }

    /// The codec strings of the tracks written, once the container exists.
    pub(crate) fn codecs(&self) -> Vec<String> {
        self.writer.as_ref().map(|w| w.codecs().to_vec()).unwrap_or_default()
    }

    /// Whether a video track is written (whose keyframes cut segments).
    pub(crate) fn has_video(&self) -> bool {
        self.video.is_some()
    }

    /// The reader's zero on the core's timeline: the first frame written's
    /// time and its track's clock rate.
    pub(crate) fn zero(&self) -> Option<(i64, u32)> {
        self.zero
    }

    /// The MIME type told was not sent after all (an HTTP head is held until
    /// its first fragment): the next tracks may have another one, told again
    /// with their init segment, where they would otherwise end the muxer.
    pub(crate) fn untell(&mut self) {
        self.mime = None;
    }

    /// Frames were lost (the subscription's queue overflowed): nothing is
    /// written until the video's next keyframe.
    pub(crate) fn lost(&mut self) {
        self.waiting = true;
    }
}

/// `ticks` of `clock` as time.
fn at_time(ticks: i64, clock: u32) -> Duration {
    let nanos = u128::try_from(ticks).unwrap_or(0) * 1_000_000_000 / u128::from(clock.max(1));
    Duration::from_nanos(u64::try_from(nanos).unwrap_or(u64::MAX))
}

/// `pts`, in ticks of `clock`, less the zero `zero` in ticks of
/// `zero_clock`; `None` when that is before the zero.
fn moved(pts: i64, clock: u32, zero: i64, zero_clock: u32) -> Option<i64> {
    let zero = i128::from(zero) * i128::from(clock) / i128::from(zero_clock.max(1));
    let pts = i128::from(pts) - zero;
    if pts < 0 { None } else { i64::try_from(pts).ok() }
}

#[cfg(test)]
#[path = "muxer_tests.rs"]
mod tests;
