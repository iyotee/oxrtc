//! The oxrtc binary's parts, as a library so that tests can start a server
//! in-process: the command line, the go2rtc-shaped logger, the wiring of
//! every protocol crate around one [`oxrtc_core::Hub`], and the glue traits.
//!
//! `main` does only this: parse the command line, install the logger, load
//! the configuration, [`App::start`], wait for a signal, [`App::shutdown`].
#![forbid(unsafe_code)]

pub mod app;
pub mod backends;
pub mod cli;
pub mod clock;
pub mod confine;
pub mod listen;
pub mod log_writer;
pub mod logging;
mod memory;

pub use app::{Addresses, App, SHUTDOWN_DEADLINE, StartError, VERSION};
