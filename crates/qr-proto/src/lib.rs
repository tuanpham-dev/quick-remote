//! QuickRemote wire protocol, version 1.
//!
//! One QUIC connection per session (ALPN [`ALPN`]):
//! - the control stream: the first bidirectional stream, client-opened,
//!   carrying length-prefixed [`Msg`]s both ways;
//! - unidirectional streams, each starting with a [`uni`] type byte: the
//!   client's input stream ([`InputEvent`]s) and one stream per file;
//! - datagrams ([`datagram`]): video shards, audio, mic, motion, pads.
//!
//! The byte-level description is in `docs/protocol.md`; test vectors are in
//! `conformance/vectors.json`.

pub mod codec;
pub mod datagram;
pub mod input;
pub mod msg;
pub mod rendezvous;
pub mod samples;

pub use codec::{DecodeError, Reader, Writer};
pub use input::InputEvent;
pub use msg::{Codec, CodecCap, Feedback, HostStats, Monitor, Msg};

pub const PROTO_MAJOR: u16 = 1;
pub const PROTO_MINOR: u16 = 0;

pub const ALPN: &[u8] = b"qr/1";

/// Largest length-prefixed frame on a stream (a 16 MiB clipboard image plus
/// headers).
pub const MAX_FRAME: usize = 17 * 1024 * 1024;

/// Type byte at the start of every unidirectional stream.
pub mod uni {
    /// Client → host input events, length-prefixed, for the whole session.
    pub const INPUT: u8 = 1;
    /// A file body: `u32 id` then raw bytes until FIN.
    pub const FILE: u8 = 2;
}

/// Prefixes `body` with its little-endian `u32` length.
pub fn frame(body: &[u8]) -> Vec<u8> {
    let mut v = Vec::with_capacity(4 + body.len());
    v.extend_from_slice(&(body.len() as u32).to_le_bytes());
    v.extend_from_slice(body);
    v
}
