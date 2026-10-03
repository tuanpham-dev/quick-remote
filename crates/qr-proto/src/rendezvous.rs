//! Rendezvous protocol (ALPN [`ALPN`]) between hosts, clients and
//! `qr-relay`, on one bidirectional stream with length-prefixed messages.
//!
//! Host:   ← Challenge, → Register, ← Registered, then ← Introduce /
//!         ← RelayAllocated whenever a client asks for it.
//! Client: → Lookup, ← Introduce, (→ RelayRequest, ← RelayAllocated).

use crate::codec::{DecodeError, Reader, Result, Writer, get_list, put_list};

pub const ALPN: &[u8] = b"qr-rv/1";

/// Bytes the host signs, followed by the challenge nonce.
pub const REGISTER_CONTEXT: &[u8] = b"quickremote rendezvous register v1";

/// First bytes of a hole-punch packet; QUIC never starts a packet like
/// this (the first byte has the fixed bit clear), so receivers drop them.
pub const PUNCH_MAGIC: &[u8; 8] = b"QRPUNCH\0";

/// Length of the session token the relay strips from relayed packets.
pub const TOKEN_LEN: usize = 16;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RvMsg {
    Challenge {
        nonce: [u8; 32],
    },
    Register {
        host_id: String,
        cert_der: Vec<u8>,
        signature: Vec<u8>,
        locals: Vec<String>,
    },
    Registered {
        observed: String,
    },
    Lookup {
        host_id: String,
        locals: Vec<String>,
    },
    /// Sent to both peers. `punch_in_ms` is when to start punching,
    /// relative to receipt. `initiator` is true for the client.
    Introduce {
        token: [u8; TOKEN_LEN],
        peer_observed: String,
        peer_locals: Vec<String>,
        punch_in_ms: u16,
        initiator: bool,
    },
    RelayRequest {
        token: [u8; TOKEN_LEN],
    },
    /// Sent to both peers: send relayed packets to `relay_addr`, each
    /// prefixed with the token.
    RelayAllocated {
        token: [u8; TOKEN_LEN],
        relay_addr: String,
    },
    Error {
        code: u16,
        text: String,
    },
}

pub mod rv_error {
    pub const UNKNOWN_HOST: u16 = 1;
    pub const BAD_SIGNATURE: u16 = 2;
    pub const NO_RELAY_CAPACITY: u16 = 3;
    pub const BAD_TOKEN: u16 = 4;
}

mod tag {
    pub const CHALLENGE: u8 = 1;
    pub const REGISTER: u8 = 2;
    pub const REGISTERED: u8 = 3;
    pub const LOOKUP: u8 = 4;
    pub const INTRODUCE: u8 = 5;
    pub const RELAY_REQUEST: u8 = 6;
    pub const RELAY_ALLOCATED: u8 = 7;
    pub const ERROR: u8 = 8;
}

fn put_strs(w: &mut Writer, v: &[String]) {
    put_list(w, v, |w, s| {
        w.str(s);
    });
}

impl RvMsg {
    pub fn encode(&self) -> Vec<u8> {
        let mut w = Writer::with_capacity(64);
        match self {
            Self::Challenge { nonce } => {
                w.u8(tag::CHALLENGE).raw(nonce);
            }
            Self::Register { host_id, cert_der, signature, locals } => {
                w.u8(tag::REGISTER).str(host_id).bytes(cert_der).bytes(signature);
                put_strs(&mut w, locals);
            }
            Self::Registered { observed } => {
                w.u8(tag::REGISTERED).str(observed);
            }
            Self::Lookup { host_id, locals } => {
                w.u8(tag::LOOKUP).str(host_id);
                put_strs(&mut w, locals);
            }
            Self::Introduce { token, peer_observed, peer_locals, punch_in_ms, initiator } => {
                w.u8(tag::INTRODUCE).raw(token).str(peer_observed);
                put_strs(&mut w, peer_locals);
                w.u16(*punch_in_ms).bool(*initiator);
            }
            Self::RelayRequest { token } => {
                w.u8(tag::RELAY_REQUEST).raw(token);
            }
            Self::RelayAllocated { token, relay_addr } => {
                w.u8(tag::RELAY_ALLOCATED).raw(token).str(relay_addr);
            }
            Self::Error { code, text } => {
                w.u8(tag::ERROR).u16(*code).str(text);
            }
        }
        w.buf
    }

    pub fn decode(buf: &[u8]) -> Result<Self> {
        let mut r = Reader::new(buf);
        let m = match r.u8()? {
            tag::CHALLENGE => Self::Challenge { nonce: r.fixed()? },
            tag::REGISTER => Self::Register {
                host_id: r.str()?,
                cert_der: r.bytes()?,
                signature: r.bytes()?,
                locals: get_list(&mut r, |r| r.str())?,
            },
            tag::REGISTERED => Self::Registered { observed: r.str()? },
            tag::LOOKUP => Self::Lookup { host_id: r.str()?, locals: get_list(&mut r, |r| r.str())? },
            tag::INTRODUCE => Self::Introduce {
                token: r.fixed()?,
                peer_observed: r.str()?,
                peer_locals: get_list(&mut r, |r| r.str())?,
                punch_in_ms: r.u16()?,
                initiator: r.bool()?,
            },
            tag::RELAY_REQUEST => Self::RelayRequest { token: r.fixed()? },
            tag::RELAY_ALLOCATED => Self::RelayAllocated { token: r.fixed()?, relay_addr: r.str()? },
            tag::ERROR => Self::Error { code: r.u16()?, text: r.str()? },
            t => return Err(DecodeError::BadTag("rendezvous", t)),
        };
        r.finish()?;
        Ok(m)
    }
}
