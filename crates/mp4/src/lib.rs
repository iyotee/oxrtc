//! Fragmented MP4 for oxrtc: the core's tracks and frames in, bytes out.
//!
//! A [`Writer`] is built from a subscription's track list ([`TrackInfo`]) and
//! gives an **init segment** (`ftyp`, then `moov` with one `trak` per track and
//! `mvex`/`trex`) and **media fragments** (`moof` with `mfhd` and one `traf` per
//! track - `tfhd`, `tfdt`, `trun` with each sample's duration, size, flags and,
//! when a decode time differs from the presentation time, its composition
//! offset - then `mdat`). It is the base of MSE, MP4 over HTTP, `frame.mp4`, HLS
//! with fMP4 segments, Low-Latency HLS, DASH and recording; it does no I/O.
//!
//! | Track | Sample entry | Codec string |
//! |---|---|---|
//! | H.264 | `avc1` + `avcC` from the parameter sets | `avc1.PPCCLL` from the SPS |
//! | H.265 | `hvc1` (or `hev1`) + `hvcC`, VPS, SPS and PPS arrays complete | `hvc1.P.C.TL.B...` from the profile, tier and level |
//! | AAC | `mp4a` + `esds` from the `AudioSpecificConfig` | `mp4a.40.<object type>` |
//! | Opus | `Opus` + `dOps` | `opus` |
//! | G.711 A-law, mu-law, L16 | `fLaC` + `dfLa`: decoded to 16-bit PCM, written as FLAC frames with verbatim subframes | `flac` |
//!
//! How frames are grouped is the caller's choice ([`Fragmenting`]): one
//! fragment per frame (MSE over a WebSocket), per group of pictures, or per
//! duration (HLS segments and parts, recording). Each [`Fragment`] says whether
//! it is independent (its video starts on a keyframe), where it starts and how
//! long it lasts.
//!
//! Decode times are contiguous per track: a sample starts where the previous
//! one ended, and a duration that has to be guessed (the last video frame of a
//! fragment written before the next frame arrived) is corrected on the next
//! sample, so that a jitter in the source never leaves a gap a browser stops at
//! ([`timing`]).
//!
//! Everything is bounded and nothing panics on input: a frame the box format
//! cannot carry is refused with an [`Mp4Error`], never truncated.
#![forbid(unsafe_code)]

mod aac;
mod boxes;
mod error;
mod flac;
mod fragment;
mod h264;
mod h265;
mod init;
mod opus;
mod timing;
mod track;
mod writer;

pub use error::Mp4Error;
pub use track::{codec_string, supported};
pub use writer::{Fragment, Fragmenting, HevcEntry, Options, Span, Writer};

/// Most tracks in one writer.
pub const MAX_TRACKS: usize = 8;

/// Largest sample, in bytes, before packing: a frame over it is refused.
pub const MAX_SAMPLE_BYTES: usize = 16 * 1024 * 1024;

/// Most bytes of samples one fragment holds: a fragment is closed before a
/// frame would take it past this, whatever the [`Fragmenting`].
pub const MAX_FRAGMENT_BYTES: usize = 32 * 1024 * 1024;

/// Most samples one fragment holds, all tracks together.
pub const MAX_FRAGMENT_SAMPLES: usize = 4096;

/// The independent reader of the tests and the fuzz targets: the box tree
/// and the sample tables, written apart from the writer.
#[cfg(any(test, feature = "fuzzing"))]
#[doc(hidden)]
pub mod reader;

/// The harnesses of the fuzz targets (`fuzz/`) and their property tests.
#[cfg(any(test, feature = "fuzzing"))]
#[doc(hidden)]
pub mod fuzzing;
