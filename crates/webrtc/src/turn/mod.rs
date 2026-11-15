//! A TURN client (RFC 8656, UDP, long-term credentials) and a STUN binding
//! client, for the candidates a viewer outside the LAN needs: str0m has
//! neither (D3, D13).
//!
//! The protocol is Sans-I/O ([`Allocation`], [`Reflexive`]); the
//! [`driver`] runs it on sockets and hands what peers send through the relay
//! to the WebRTC sessions, as if it had arrived on the shared socket.

pub(crate) mod allocation;
pub(crate) mod driver;
pub(crate) mod peers;
pub(crate) mod reflexive;
pub(crate) mod servers;
mod transactions;
pub(crate) mod url;

pub(crate) use allocation::{Allocation, Credentials};
pub(crate) use reflexive::Reflexive;
