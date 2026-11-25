//! The writer: tracks in, an init segment, frames in, fragments out.

use std::time::Duration;

use bytes::Bytes;
use oxrtc_core::{Frame, MediaKind, TrackInfo};

use crate::fragment::{self, NON_SYNC, Run, SYNC, Sample};
use crate::timing::Timing;
use crate::track::Track;
use crate::{MAX_FRAGMENT_BYTES, MAX_FRAGMENT_SAMPLES, MAX_TRACKS, Mp4Error, init};

/// How frames are grouped into fragments. Whatever the choice, a fragment is
/// also closed before it would hold more than [`MAX_FRAGMENT_SAMPLES`]
/// samples or [`MAX_FRAGMENT_BYTES`] bytes, and [`Writer::flush`] closes one
/// at any time.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Fragmenting {
    /// One fragment per frame, written as it comes: the lowest latency, what
    /// MSE over a WebSocket wants. A video sample's duration is then the
    /// track's typical step, corrected on the next sample ([`crate`]'s
    /// "contiguous" rule).
    #[default]
    EveryFrame,
    /// A fragment per group of pictures: closed before each keyframe of the
    /// first video track (each fragment of a stream that starts on one is
    /// independent). With no video track, a fragment per second of sound.
    GroupOfPictures,
    /// A fragment once it holds this long (on the track of the frame that
    /// comes next), and also before each keyframe of the first video track,
    /// so that a keyframe always starts one: HLS parts, and segments made of
    /// them.
    Duration(Duration),
}

/// The sample entry of H.265: `hvc1` (parameter sets in the entry, what
/// Safari wants) or `hev1` (parameter sets also in band).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum HevcEntry {
    /// `hvc1`.
    #[default]
    Hvc1,
    /// `hev1`.
    Hev1,
}

/// A writer's options.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Options {
    /// How frames are grouped.
    pub fragmenting: Fragmenting,
    /// The H.265 sample entry.
    pub hevc: HevcEntry,
}

/// Where a fragment sits in time, on one of its tracks: the first track (in
/// the writer's order, video first in a subscription) that has samples in it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Span {
    /// The track's index.
    pub track: usize,
    /// The decode time of its first sample, in `timescale` ticks: the track's
    /// clock, on the core's timeline.
    pub start: u64,
    /// The sum of its samples' durations, in ticks.
    pub duration: u64,
    /// Ticks per second.
    pub timescale: u32,
}

impl Span {
    /// The duration as time.
    pub fn duration(&self) -> Duration {
        ticks(self.duration, self.timescale)
    }

    /// The start as time on the core's timeline.
    pub fn start(&self) -> Duration {
        ticks(self.start, self.timescale)
    }
}

fn ticks(value: u64, timescale: u32) -> Duration {
    let scale = u64::from(timescale.max(1));
    Duration::from_secs(value / scale) + Duration::from_nanos((value % scale) * 1_000_000_000 / scale)
}

/// A media fragment: `moof` and `mdat`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fragment {
    /// Its `mfhd` sequence number: 1, 2, 3...
    pub sequence: u32,
    /// Every video track's first sample in it is a keyframe (always true for
    /// a writer with no video track): a decoder can start here, and an HLS
    /// segment or an independent part can be cut here.
    pub independent: bool,
    /// Where it sits in time.
    pub span: Span,
    /// Its samples, all tracks.
    pub samples: usize,
    /// The bytes.
    pub bytes: Bytes,
}

#[derive(Debug, Clone)]
struct State {
    track: Track,
    timing: Timing,
}

#[derive(Debug, Clone)]
struct Pending {
    track: usize,
    placed: i64,
    keyframe: bool,
    sample: Sample,
}

/// A fragmented MP4 writer for one track list. A new list (a source that
/// reconnected with other tracks or parameter sets) takes a new writer.
#[derive(Debug, Clone)]
pub struct Writer {
    tracks: Vec<State>,
    /// The first video track: the one whose keyframes cut fragments.
    lead: Option<usize>,
    options: Options,
    init: Bytes,
    codecs: Vec<String>,
    sequence: u32,
    pending: Vec<Pending>,
    pending_bytes: usize,
}

impl Writer {
    /// A writer for `tracks`, numbered 1, 2... in their order. Every track
    /// must be [`crate::supported`] and carry the configuration it needs.
    pub fn new(tracks: &[TrackInfo], options: Options) -> Result<Self, Mp4Error> {
        if tracks.is_empty() || tracks.len() > MAX_TRACKS {
            return Err(Mp4Error::TrackCount { count: tracks.len() });
        }
        let mut states = Vec::with_capacity(tracks.len());
        for (id, info) in (1..).zip(tracks) {
            let track = Track::new(info, id, options.hevc)?;
            states.push(State { timing: Timing::new(track.timescale), track });
        }
        let built: Vec<Track> = states.iter().map(|s| s.track.clone()).collect();
        let init = Bytes::from(init::segment(&built)?);
        Ok(Self {
            lead: states.iter().position(|s| s.track.kind == MediaKind::Video),
            codecs: states.iter().map(|s| s.track.codec.clone()).collect(),
            tracks: states,
            options,
            init,
            sequence: 0,
            pending: Vec::new(),
            pending_bytes: 0,
        })
    }

    /// The init segment: what a player reads before any fragment.
    pub fn init_segment(&self) -> &Bytes {
        &self.init
    }

    /// The init segment of track `track` alone, under its own id: the
    /// fragments this writer makes of that track read with it, whatever the
    /// other tracks (DASH's one adaptation set per track).
    pub fn track_init_segment(&self, track: usize) -> Result<Bytes, Mp4Error> {
        let state = self.tracks.get(track).ok_or(Mp4Error::NoTrack(track))?;
        let id = u32::try_from(track + 1).map_err(|_| Mp4Error::NoTrack(track))?;
        Ok(Bytes::from(init::segment_of(&[(id, &state.track)])?))
    }

    /// Each track's codec string, in track order.
    pub fn codecs(&self) -> &[String] {
        &self.codecs
    }

    /// `video/mp4; codecs="avc1.64001f,mp4a.40.2"` (`audio/mp4` with no video
    /// track): what MSE's `isTypeSupported` and `addSourceBuffer` take.
    pub fn mime_type(&self) -> String {
        let kind = if self.lead.is_some() { "video" } else { "audio" };
        format!("{kind}/mp4; codecs=\"{}\"", self.codecs.join(","))
    }

    /// Samples written but not yet in a fragment.
    pub fn pending_samples(&self) -> usize {
        self.pending.len()
    }

    /// Adds a frame of track `track`, decoded at its presentation time (the
    /// core's frames carry no other). Returns the fragment this closed, if
    /// any. A refused frame changes nothing.
    pub fn push(&mut self, track: usize, frame: &Frame) -> Result<Option<Fragment>, Mp4Error> {
        self.push_decoded(track, frame, frame.pts)
    }

    /// [`Writer::push`] for a frame whose decode time `dts` differs from its
    /// presentation time (B-frames): the difference is its composition
    /// offset. Frames of a track come in decode order.
    pub fn push_decoded(&mut self, track: usize, frame: &Frame, dts: i64) -> Result<Option<Fragment>, Mp4Error> {
        if dts < 0 {
            return Err(Mp4Error::Time("a negative decode time"));
        }
        let offset = frame.pts.checked_sub(dts).and_then(|o| i32::try_from(o).ok());
        let composition_offset = offset.ok_or(Mp4Error::Time("a composition offset over 32 bits"))?;
        let state = self.tracks.get_mut(track).ok_or(Mp4Error::NoTrack(track))?;
        let (data, content) = state.track.pack(&frame.data)?;
        let video = state.track.kind == MediaKind::Video;

        // The track's last unwritten sample now ends where this one goes.
        if let Some(last) = self.pending.iter_mut().rev().find(|p| p.track == track) {
            last.sample.duration = state.timing.reach(last.placed, dts);
        }
        let keyframe = !video || frame.keyframe;
        let mut out = None;
        if !self.pending.is_empty() && self.cuts_before(track, keyframe, data.len()) {
            out = self.take()?;
        }
        let state = self.tracks.get_mut(track).ok_or(Mp4Error::NoTrack(track))?;
        let (placed, duration) = state.timing.place(dts, content);
        let flags = if keyframe { SYNC } else { NON_SYNC };
        self.pending_bytes += data.len();
        self.pending.push(Pending {
            track,
            placed,
            keyframe,
            sample: Sample { duration, flags, composition_offset, data },
        });
        if self.options.fragmenting == Fragmenting::EveryFrame {
            out = self.take()?;
        }
        Ok(out)
    }

    /// Closes the fragment being filled, if it holds anything.
    pub fn flush(&mut self) -> Result<Option<Fragment>, Mp4Error> {
        self.take()
    }

    /// Whether a frame of `track` starts a new fragment.
    fn cuts_before(&self, track: usize, keyframe: bool, size: usize) -> bool {
        if self.pending.len() >= MAX_FRAGMENT_SAMPLES || self.pending_bytes.saturating_add(size) > MAX_FRAGMENT_BYTES {
            return true;
        }
        let lead_keyframe = self.lead == Some(track) && keyframe;
        match self.options.fragmenting {
            Fragmenting::EveryFrame => false,
            Fragmenting::GroupOfPictures if self.lead.is_some() => lead_keyframe,
            Fragmenting::GroupOfPictures => self.held(track) >= Duration::from_secs(1),
            Fragmenting::Duration(target) => lead_keyframe || self.held(track) >= target,
        }
    }

    /// How long the samples of `track` waiting for a fragment last.
    fn held(&self, track: usize) -> Duration {
        let Some(state) = self.tracks.get(track) else { return Duration::ZERO };
        let ticks: u64 = self.pending.iter().filter(|p| p.track == track).map(|p| u64::from(p.sample.duration)).sum();
        self::ticks(ticks, state.track.timescale)
    }

    /// The pending samples as a fragment, track after track.
    fn take(&mut self) -> Result<Option<Fragment>, Mp4Error> {
        if self.pending.is_empty() {
            return Ok(None);
        }
        let mut grouped: Vec<(usize, i64, Vec<Sample>)> = Vec::new();
        let mut independent = true;
        for (index, state) in self.tracks.iter().enumerate() {
            let mut of_track = self.pending.iter().filter(|p| p.track == index).peekable();
            let first = of_track.peek().map(|p| (p.placed, p.keyframe));
            if state.track.kind == MediaKind::Video && first.is_none_or(|(_, key)| !key) {
                independent = false;
            }
            if let Some((placed, _)) = first {
                grouped.push((index, placed, of_track.map(|p| p.sample.clone()).collect()));
            }
        }
        let mut runs = Vec::with_capacity(grouped.len());
        for (index, placed, samples) in &grouped {
            let base_decode_time = u64::try_from(*placed).map_err(|_| Mp4Error::Time("a negative decode time"))?;
            runs.push(Run { track_id: u32::try_from(*index + 1).unwrap_or(u32::MAX), base_decode_time, samples });
        }
        let sequence = self.sequence.wrapping_add(1).max(1);
        let bytes = Bytes::from(fragment::write(sequence, &runs)?);
        let span = match runs.first().zip(grouped.first()) {
            Some((run, (index, _, samples))) => Span {
                track: *index,
                start: run.base_decode_time,
                duration: samples.iter().map(|s| u64::from(s.duration)).sum(),
                timescale: self.tracks.get(*index).map_or(1, |s| s.track.timescale),
            },
            None => return Ok(None),
        };
        self.sequence = sequence;
        let samples = self.pending.len();
        self.pending.clear();
        self.pending_bytes = 0;
        Ok(Some(Fragment { sequence, independent, span, samples, bytes }))
    }
}

#[cfg(test)]
#[path = "writer_tests.rs"]
mod tests;
