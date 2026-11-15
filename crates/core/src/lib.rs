//! Media core of oxrtc.
//!
//! Everything a protocol needs to exchange media, and nothing that touches the
//! network. A [`Hub`] holds named [`Stream`]s; a stream holds an ordered list of
//! sources, each described by a [`SourceSpec`] and run by a [`SourceDriver`]
//! that a protocol crate registers in [`Drivers`]. Consumers [`Stream::subscribe`]
//! with a [`Want`] (which media, which codecs) and receive [`Event`]s.
//!
//! Three properties hold everywhere in this crate, and the tests pin them:
//!
//! - **Nothing is unbounded.** Every queue, cache and list has a limit taken from
//!   [`Limits`]; a slow consumer loses frames and resumes at the next keyframe, it
//!   never makes the producer wait or memory grow.
//! - **Sources run on demand.** A source is started by its first consumer (or a
//!   [`Preload`]) and stopped a grace period after its last one leaves; it is
//!   restarted with exponential backoff and jitter when it fails or stalls.
//! - **Input from a driver is validated.** A frame whose length prefixes do not
//!   add up is refused, not forwarded to a decoder.
#![forbid(unsafe_code)]

pub mod backoff;
mod channel;
pub mod driver;
pub mod error;
pub mod frame;
pub mod hub;
pub mod info;
pub mod limits;
pub mod local;
pub mod media;
pub mod per_ip;
pub mod refusals;
mod slot;
pub mod spec;
pub mod stream;
pub mod subscription;
pub mod timeline;
pub mod want;

pub use channel::ProducerSink;
pub use driver::{BoxFuture, Declared, Drivers, FactoryCtx, SourceDriver, SourceError, SourceFactory};
pub use error::{FrameError, HubError, PublishError, SpecError, SubEnd, SubscribeError, WantError};
pub use frame::Frame;
pub use hub::Hub;
pub use info::{ConsumerSnapshot, SourceSnapshot, SourceState, StreamSnapshot};
pub use limits::Limits;
pub use media::{Codec, CodecParams, MediaKind, TrackInfo};
pub use per_ip::{PerIpLimit, PerIpPermit};
pub use refusals::Refusals;
pub use spec::SourceSpec;
pub use stream::{ConsumerTag, Preload, Stream};
pub use subscription::{Event, Subscription};
pub use tokio_util::sync::CancellationToken;
pub use want::{MediaWant, Want};
