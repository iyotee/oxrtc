//! MPEG transport stream (ISO/IEC 13818-1) for oxrtc: the core's tracks and
//! frames in, 188-byte packets out.
//!
//! A [`Writer`] is built from a subscription's track list and gives the
//! program's tables ([`Writer::tables`]: a PAT naming one program and its
//! PMT, one packet each) and, for each frame, its PES packet cut into
//! transport packets. It is the base of HLS with MPEG-TS segments (M5, F6)
//! and of `stream.ts` (F14, which adds the reader's public side and the
//! sources); it does no I/O.
//!
//! | Track | Stream type | Elementary stream |
//! |---|---|---|
//! | H.264 | `0x1B` | Annex B, an access unit delimiter first, the SPS and PPS before every IDR picture that lacks them |
//! | H.265 | `0x24` | Annex B, an access unit delimiter first, the VPS, SPS and PPS before every IRAP picture that lacks them |
//! | AAC | `0x0F` | ADTS: a 7-byte header per access unit, from the `AudioSpecificConfig` |
//! | Opus | `0x06` with the `Opus` registration and its extension descriptor | each packet behind its control header (the Opus project's "Encapsulation of Opus in MPEG-2 Transport Stream") |
//!
//! Every PES carries its presentation time, and its decode time when it
//! differs; times are on a 90 kHz clock, [`TIME_OFFSET`] ahead of the
//! caller's, so that the program clock reference (carried on the first video
//! track, else the first track, at each of its PES) is never ahead of a decode
//! time. Continuity counters run on across calls: the bytes of successive
//! calls are one stream, and an HLS segment is the tables then the frames
//! of its span.
//!
//! Everything is bounded and nothing panics on input: a frame the format
//! cannot carry is refused with a [`TsError`], never truncated.
#![forbid(unsafe_code)]

mod crc;
mod error;
mod es;
mod packet;
mod pes;
mod psi;
mod writer;

pub use error::TsError;
pub use writer::{Writer, supported};

/// A transport packet's size.
pub const PACKET_SIZE: usize = 188;

/// The PID of the program map table (FFmpeg's).
pub const PMT_PID: u16 = 0x1000;

/// The PID of the first track; the next ones follow (FFmpeg's).
pub const FIRST_PID: u16 = 0x100;

/// Most tracks in one writer.
pub const MAX_TRACKS: usize = 8;

/// Largest frame, in bytes: a frame over it is refused.
pub const MAX_FRAME_BYTES: usize = 16 * 1024 * 1024;

/// The clock of every time in the stream.
pub const CLOCK: u32 = 90_000;

/// What every presentation and decode time is moved ahead by, in 90 kHz
/// ticks (0.7 s, FFmpeg's default `muxdelay`): the program clock reference
/// is the decode time without it, so it never runs ahead of a frame's decode.
pub const TIME_OFFSET: u64 = 63_000;

/// The independent reader of the tests and the fuzz targets: packets, the
/// program's tables and each PES, read apart from the writer. F14 makes it
/// the reader of MPEG-TS sources.
#[cfg(any(test, feature = "fuzzing"))]
#[doc(hidden)]
pub mod reader;

/// The harnesses of the fuzz targets (`fuzz/`) and their property tests.
#[cfg(any(test, feature = "fuzzing"))]
#[doc(hidden)]
pub mod fuzzing;
