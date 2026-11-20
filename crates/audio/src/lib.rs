//! Sound for oxrtc, in process (W5, R33, D37): what a camera's G.711 or L16
//! becomes for a reader that wants something else, with no FFmpeg.
//!
//! - [`g711`]: A-law and mu-law to and from 16-bit linear PCM.
//! - [`pcm`]: L16 (big-endian, RTP) and little-endian (WAV) samples.
//! - [`resample`]: a windowed-sinc polyphase resampler between 8, 16, 32,
//!   44.1 and 48 kHz (any two rates whose ratio reduces to small numbers).
//! - [`g722`]: the G.722 encoder (64 kbit/s, ITU-T G.722 sub-band ADPCM).
//! - [`wav`]: a WAV stream reader, bounded: a header, then PCM or G.711.
//! - [`opus`] (feature `opus`): libopus' encoder, through the `opus` crate.
//! - [`transcode`]: frames of one codec in, packets of another out, the
//!   input's clock kept.
//!
//! No I/O and no unsafe code: the protocol crates feed it and publish what
//! it gives. Every input is untrusted: nothing here panics on it, and every
//! buffer has a bound.
#![forbid(unsafe_code)]

pub mod g711;
pub mod g722;
#[cfg(feature = "opus")]
pub mod opus;
pub mod pcm;
pub mod resample;
pub mod transcode;
pub mod wav;

/// The harness of the fuzz target (`fuzz/`) and its stable property tests.
#[cfg(any(test, feature = "fuzzing"))]
#[doc(hidden)]
pub mod fuzzing;
