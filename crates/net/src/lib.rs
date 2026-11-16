//! What the camera clients share: the TCP connection to a camera, which tries
//! every address of its name in turn ([`connect`]), and TLS over it with a
//! choice of verification ([`Tls`]). Neither knows a protocol; the HTTP and
//! RTSP clients each speak theirs on top.
#![forbid(unsafe_code)]

mod connect;
mod tls;

pub use connect::connect;
pub use tls::{Tls, TlsStream};
