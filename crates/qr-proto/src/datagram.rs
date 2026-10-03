//! Unreliable QUIC datagrams. The first byte is the kind.
//!
//! Video frames are cut into FEC shards (see `qr-fec`); every other kind is
//! small and self-contained. Motion carries running totals and pad state
//! carries full snapshots, so a lost datagram never corrupts input.

use crate::codec::{DecodeError, Reader, Result, Writer};

pub mod kind {
    pub const VIDEO: u8 = 1;
    pub const AUDIO: u8 = 2;
    pub const MIC: u8 = 3;
    pub const MOTION: u8 = 4;
    pub const PAD: u8 = 5;
    pub const PROBE: u8 = 6;
}

/// Bytes before the payload of a video shard.
pub const VIDEO_HEADER_LEN: usize = 13;

/// Header of one video shard. Within a block every shard has the same
/// length; shards `0..k` carry data and `k..k+m` carry Reed-Solomon parity.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VideoShardHeader {
    pub stream: u8,
    pub flags: u8,
    pub frame: u32,
    pub block: u8,
    pub blocks: u8,
    pub shard: u8,
    pub k: u8,
    pub m: u8,
}

pub mod vflags {
    pub const KEYFRAME: u8 = 1 << 0;
}

impl VideoShardHeader {
    pub fn write(&self, w: &mut Writer) {
        w.u8(kind::VIDEO).u8(self.stream).u8(self.flags).u32(self.frame);
        w.u8(self.block).u8(self.blocks).u8(self.shard).u8(self.k).u8(self.m);
    }

    /// Parses a video datagram; returns the header and the shard payload.
    pub fn parse(buf: &[u8]) -> Result<(Self, &[u8])> {
        let mut r = Reader::new(buf);
        if r.u8()? != kind::VIDEO {
            return Err(DecodeError::BadTag("datagram", buf[0]));
        }
        let h = Self {
            stream: r.u8()?,
            flags: r.u8()?,
            frame: r.u32()?,
            block: r.u8()?,
            blocks: r.u8()?,
            shard: r.u8()?,
            k: r.u8()?,
            m: r.u8()?,
        };
        if h.k == 0 || h.blocks == 0 || h.block >= h.blocks || h.shard as u16 >= h.k as u16 + h.m as u16 {
            return Err(DecodeError::BadTag("video shard", h.shard));
        }
        Ok((h, r.rest()))
    }
}

pub const FRAME_META_LEN: usize = 42;

/// Prefixed to every encoded frame before it is cut into shards, so it is
/// FEC-protected with the frame. Times are host-clock microseconds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct FrameMeta {
    pub codec: u8,
    pub flags: u8,
    pub width: u16,
    pub height: u16,
    /// Length of the encoded access unit that follows the meta.
    pub len: u32,
    pub capture_us: u64,
    pub encode_start_us: u64,
    pub encode_end_us: u64,
    pub send_us: u64,
}

pub mod fflags {
    pub const KEYFRAME: u8 = 1 << 0;
    pub const CHROMA444: u8 = 1 << 1;
    /// A still-screen refinement frame (sharp-text mode).
    pub const REFRESH: u8 = 1 << 2;
}

impl FrameMeta {
    pub fn write(&self, w: &mut Writer) {
        w.u8(self.codec).u8(self.flags).u16(self.width).u16(self.height).u32(self.len);
        w.u64(self.capture_us).u64(self.encode_start_us).u64(self.encode_end_us).u64(self.send_us);
    }

    pub fn parse(buf: &[u8]) -> Result<(Self, &[u8])> {
        let mut r = Reader::new(buf);
        let m = Self {
            codec: r.u8()?,
            flags: r.u8()?,
            width: r.u16()?,
            height: r.u16()?,
            len: r.u32()?,
            capture_us: r.u64()?,
            encode_start_us: r.u64()?,
            encode_end_us: r.u64()?,
            send_us: r.u64()?,
        };
        let au = r.take(m.len as usize)?;
        Ok((m, au))
    }
}

/// Opus packet, host audio (`AUDIO`) or client microphone (`MIC`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AudioPacket {
    pub mic: bool,
    pub seq: u32,
    /// Capture time on the sender's clock, µs.
    pub ts_us: u64,
    pub data: Vec<u8>,
}

impl AudioPacket {
    pub fn encode(&self) -> Vec<u8> {
        let mut w = Writer::with_capacity(16 + self.data.len());
        w.u8(if self.mic { kind::MIC } else { kind::AUDIO }).u32(self.seq).u64(self.ts_us);
        w.raw(&self.data);
        w.buf
    }

    pub fn decode(buf: &[u8]) -> Result<Self> {
        let mut r = Reader::new(buf);
        let mic = match r.u8()? {
            kind::AUDIO => false,
            kind::MIC => true,
            t => return Err(DecodeError::BadTag("datagram", t)),
        };
        Ok(Self { mic, seq: r.u32()?, ts_us: r.u64()?, data: r.rest().to_vec() })
    }
}

/// Pointer motion. `rel_*` are running totals of relative motion since the
/// session started: the host applies the difference to the last total it
/// applied, so motion stays exact across loss and reordering (by `seq`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Motion {
    pub seq: u32,
    pub rel_x: i64,
    pub rel_y: i64,
    /// Absolute position (stream pixels) when `has_abs`.
    pub has_abs: bool,
    pub stream: u8,
    pub abs_x: u16,
    pub abs_y: u16,
    /// Client time the latest sample was taken, µs (input-latency check).
    pub t_client_us: u64,
}

impl Motion {
    pub fn encode(&self) -> Vec<u8> {
        let mut w = Writer::with_capacity(40);
        w.u8(kind::MOTION).u32(self.seq).i64(self.rel_x).i64(self.rel_y);
        w.bool(self.has_abs).u8(self.stream).u16(self.abs_x).u16(self.abs_y);
        w.u64(self.t_client_us);
        w.buf
    }

    pub fn decode(buf: &[u8]) -> Result<Self> {
        let mut r = Reader::new(buf);
        if r.u8()? != kind::MOTION {
            return Err(DecodeError::BadTag("datagram", buf[0]));
        }
        let m = Self {
            seq: r.u32()?,
            rel_x: r.i64()?,
            rel_y: r.i64()?,
            has_abs: r.bool()?,
            stream: r.u8()?,
            abs_x: r.u16()?,
            abs_y: r.u16()?,
            t_client_us: r.u64()?,
        };
        r.finish()?;
        Ok(m)
    }
}

/// Gamepad buttons (`PadState::buttons`), Xbox layout.
pub mod pad {
    pub const A: u32 = 1 << 0;
    pub const B: u32 = 1 << 1;
    pub const X: u32 = 1 << 2;
    pub const Y: u32 = 1 << 3;
    pub const LB: u32 = 1 << 4;
    pub const RB: u32 = 1 << 5;
    pub const BACK: u32 = 1 << 6;
    pub const START: u32 = 1 << 7;
    pub const GUIDE: u32 = 1 << 8;
    pub const LS: u32 = 1 << 9;
    pub const RS: u32 = 1 << 10;
    pub const UP: u32 = 1 << 11;
    pub const DOWN: u32 = 1 << 12;
    pub const LEFT: u32 = 1 << 13;
    pub const RIGHT: u32 = 1 << 14;
    /// Axis order in `PadState::axes`: LX, LY, RX, RY (−32768..32767, +Y
    /// down), LT, RT (0..32767).
    pub const AXES: usize = 6;
}

/// Full state of one pad; the host keeps the newest by `seq`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PadState {
    pub idx: u8,
    pub seq: u32,
    pub buttons: u32,
    pub axes: [i16; pad::AXES],
}

impl PadState {
    pub fn encode(&self) -> Vec<u8> {
        let mut w = Writer::with_capacity(24);
        w.u8(kind::PAD).u8(self.idx).u32(self.seq).u32(self.buttons);
        for a in self.axes {
            w.i16(a);
        }
        w.buf
    }

    pub fn decode(buf: &[u8]) -> Result<Self> {
        let mut r = Reader::new(buf);
        if r.u8()? != kind::PAD {
            return Err(DecodeError::BadTag("datagram", buf[0]));
        }
        let idx = r.u8()?;
        let seq = r.u32()?;
        let buttons = r.u32()?;
        let mut axes = [0i16; pad::AXES];
        for a in &mut axes {
            *a = r.i16()?;
        }
        r.finish()?;
        Ok(Self { idx, seq, buttons, axes })
    }
}

/// Padding datagram for bandwidth probing; echoed by nothing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Probe {
    pub seq: u32,
    pub ts_us: u64,
    pub pad_len: u16,
}

impl Probe {
    pub fn encode(&self) -> Vec<u8> {
        let mut w = Writer::with_capacity(15 + self.pad_len as usize);
        w.u8(kind::PROBE).u32(self.seq).u64(self.ts_us).u16(self.pad_len);
        w.buf.resize(w.buf.len() + self.pad_len as usize, 0);
        w.buf
    }

    pub fn decode(buf: &[u8]) -> Result<Self> {
        let mut r = Reader::new(buf);
        if r.u8()? != kind::PROBE {
            return Err(DecodeError::BadTag("datagram", buf[0]));
        }
        let p = Self { seq: r.u32()?, ts_us: r.u64()?, pad_len: r.u16()? };
        r.take(p.pad_len as usize)?;
        r.finish()?;
        Ok(p)
    }
}
