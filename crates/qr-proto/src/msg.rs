//! Control-stream messages (reliable, both directions).
//!
//! Each message is `tag: u8` followed by its fields in declaration order.
//! `docs/protocol.md` lists the tags and field layouts; keep both in sync.

use crate::codec::{DecodeError, Reader, Result, Writer, get_list, put_list};

#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Codec {
    H264 = 1,
    H265 = 2,
    Av1 = 3,
}

impl Codec {
    pub fn from_u8(v: u8) -> Result<Self> {
        Ok(match v {
            1 => Self::H264,
            2 => Self::H265,
            3 => Self::Av1,
            t => return Err(DecodeError::BadTag("codec", t)),
        })
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::H264 => "h264",
            Self::H265 => "h265",
            Self::Av1 => "av1",
        }
    }

    /// Ranking used by codec negotiation: higher is preferred.
    pub fn efficiency(self) -> u8 {
        match self {
            Self::Av1 => 3,
            Self::H265 => 2,
            Self::H264 => 1,
        }
    }
}

/// What one side can encode (host) or decode (client) for one codec.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodecCap {
    pub codec: Codec,
    pub hw: bool,
    pub max_width: u16,
    pub max_height: u16,
    pub chroma444: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Monitor {
    pub id: u8,
    pub name: String,
    pub width: u16,
    pub height: u16,
    pub refresh_mhz: u32,
    pub x: i32,
    pub y: i32,
    pub primary: bool,
}

/// Session mode flags (`SetMode`).
pub mod mode {
    pub const RELATIVE_MOUSE: u32 = 1 << 0;
    pub const SHARP_TEXT: u32 = 1 << 1;
    pub const MIC: u32 = 1 << 2;
    pub const CLIPBOARD: u32 = 1 << 3;
    pub const AUDIO: u32 = 1 << 4;
}

#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Profile {
    Lan = 1,
    Internet = 2,
}

impl Profile {
    pub fn from_u8(v: u8) -> Result<Self> {
        match v {
            1 => Ok(Self::Lan),
            2 => Ok(Self::Internet),
            t => Err(DecodeError::BadTag("profile", t)),
        }
    }
}

#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PathKind {
    Lan = 1,
    Direct = 2,
    Relayed = 3,
}

impl PathKind {
    pub fn from_u8(v: u8) -> Result<Self> {
        match v {
            1 => Ok(Self::Lan),
            2 => Ok(Self::Direct),
            3 => Ok(Self::Relayed),
            t => Err(DecodeError::BadTag("path", t)),
        }
    }
}

#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClipKind {
    Text = 1,
    Png = 2,
}

#[repr(u16)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorCode {
    VersionMismatch = 1,
    PinRequired = 2,
    PinWrong = 3,
    RateLimited = 4,
    NoCommonCodec = 5,
    Busy = 6,
    CaptureFailed = 7,
    Internal = 8,
    NotPaired = 9,
}

impl ErrorCode {
    pub fn from_u16(v: u16) -> Self {
        match v {
            1 => Self::VersionMismatch,
            2 => Self::PinRequired,
            3 => Self::PinWrong,
            4 => Self::RateLimited,
            5 => Self::NoCommonCodec,
            6 => Self::Busy,
            7 => Self::CaptureFailed,
            9 => Self::NotPaired,
            _ => Self::Internal,
        }
    }
}

/// Client feedback, sent every 50 ms; drives host rate control.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Feedback {
    pub seq: u32,
    pub window_ms: u16,
    pub bytes_rx: u32,
    pub shards_rx: u32,
    pub shards_lost: u32,
    pub shards_repaired: u32,
    pub frames_rx: u16,
    pub frames_lost: u16,
    /// Frame transit delay (client arrival − host send, offset-corrected), µs.
    pub delay_min_us: i32,
    pub delay_max_us: i32,
    /// Least-squares slope of transit delay over the window, µs per second.
    pub delay_slope: i32,
    pub decode_us: u32,
    pub present_us: u32,
}

/// Host-side numbers the client shows in its overlay.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct HostStats {
    pub stream: u8,
    pub bitrate_kbps: u32,
    pub fps: u16,
    pub parity_pct: u8,
    pub profile: u8,
    pub encoder: String,
    pub capture_us: u32,
    pub encode_us: u32,
    pub dropped_by_backpressure: u32,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Msg {
    /// Client → host, first message. `want_*` is the size the client asks
    /// a virtual display to be (0 = host decides).
    Hello {
        major: u16,
        minor: u16,
        name: String,
        os: String,
        want_width: u16,
        want_height: u16,
        want_refresh: u16,
        decoders: Vec<CodecCap>,
    },
    /// Host → client. `need_pin` asks the client to run pairing.
    HelloAck {
        major: u16,
        minor: u16,
        name: String,
        host_id: String,
        need_pin: bool,
        rendezvous: String,
    },
    /// SPAKE2 message, both directions, once each.
    PairSpake {
        data: Vec<u8>,
    },
    /// HMAC confirmation of the SPAKE2 transcript, both directions.
    PairConfirm {
        mac: [u8; 32],
    },
    /// Host → client: authenticated; media follows.
    SessionStart {
        profile: u8,
        path: u8,
        modes: u32,
    },
    Ping {
        seq: u32,
        t_client: u64,
    },
    Pong {
        seq: u32,
        t_client: u64,
        t_host: u64,
    },
    Feedback(Feedback),
    FrameLost {
        stream: u8,
        frame: u32,
    },
    KeyframeReq {
        stream: u8,
    },
    SetMode {
        modes: u32,
    },
    MonitorList {
        monitors: Vec<Monitor>,
    },
    SelectMonitor {
        stream: u8,
        monitor: u8,
    },
    StreamStarted {
        stream: u8,
        monitor: u8,
        codec: u8,
        chroma444: bool,
        width: u16,
        height: u16,
        fps: u16,
    },
    StreamStop {
        stream: u8,
    },
    Resize {
        stream: u8,
        width: u16,
        height: u16,
        refresh: u16,
    },
    Clipboard {
        kind: u8,
        data: Vec<u8>,
    },
    FileOffer {
        id: u32,
        name: String,
        size: u64,
        sha256: [u8; 32],
    },
    FileAccept {
        id: u32,
    },
    FileCancel {
        id: u32,
        reason: String,
    },
    FileDone {
        id: u32,
        ok: bool,
    },
    PadAttach {
        idx: u8,
        kind: u8,
    },
    PadDetach {
        idx: u8,
    },
    Rumble {
        idx: u8,
        strong: u16,
        weak: u16,
        ms: u16,
    },
    HostStats(HostStats),
    Error {
        code: u16,
        text: String,
    },
    /// Client → host: the session ends on purpose (no auto-reconnect).
    Bye,
}

mod tag {
    pub const HELLO: u8 = 1;
    pub const HELLO_ACK: u8 = 2;
    pub const PAIR_SPAKE: u8 = 3;
    pub const PAIR_CONFIRM: u8 = 4;
    pub const SESSION_START: u8 = 5;
    pub const PING: u8 = 6;
    pub const PONG: u8 = 7;
    pub const FEEDBACK: u8 = 8;
    pub const FRAME_LOST: u8 = 9;
    pub const KEYFRAME_REQ: u8 = 10;
    pub const SET_MODE: u8 = 11;
    pub const MONITOR_LIST: u8 = 12;
    pub const SELECT_MONITOR: u8 = 13;
    pub const STREAM_STARTED: u8 = 14;
    pub const STREAM_STOP: u8 = 15;
    pub const RESIZE: u8 = 16;
    pub const CLIPBOARD: u8 = 17;
    pub const FILE_OFFER: u8 = 18;
    pub const FILE_ACCEPT: u8 = 19;
    pub const FILE_CANCEL: u8 = 20;
    pub const FILE_DONE: u8 = 21;
    pub const PAD_ATTACH: u8 = 22;
    pub const PAD_DETACH: u8 = 23;
    pub const RUMBLE: u8 = 24;
    pub const HOST_STATS: u8 = 25;
    pub const ERROR: u8 = 26;
    pub const BYE: u8 = 27;
}

fn put_cap(w: &mut Writer, c: &CodecCap) {
    w.u8(c.codec as u8)
        .bool(c.hw)
        .u16(c.max_width)
        .u16(c.max_height)
        .bool(c.chroma444);
}

fn get_cap(r: &mut Reader) -> Result<CodecCap> {
    Ok(CodecCap {
        codec: Codec::from_u8(r.u8()?)?,
        hw: r.bool()?,
        max_width: r.u16()?,
        max_height: r.u16()?,
        chroma444: r.bool()?,
    })
}

fn put_monitor(w: &mut Writer, m: &Monitor) {
    w.u8(m.id)
        .str(&m.name)
        .u16(m.width)
        .u16(m.height)
        .u32(m.refresh_mhz)
        .i32(m.x)
        .i32(m.y)
        .bool(m.primary);
}

fn get_monitor(r: &mut Reader) -> Result<Monitor> {
    Ok(Monitor {
        id: r.u8()?,
        name: r.str()?,
        width: r.u16()?,
        height: r.u16()?,
        refresh_mhz: r.u32()?,
        x: r.i32()?,
        y: r.i32()?,
        primary: r.bool()?,
    })
}

impl Msg {
    pub fn encode(&self) -> Vec<u8> {
        let mut w = Writer::with_capacity(64);
        self.encode_into(&mut w);
        w.buf
    }

    pub fn encode_into(&self, w: &mut Writer) {
        match self {
            Msg::Hello {
                major,
                minor,
                name,
                os,
                want_width,
                want_height,
                want_refresh,
                decoders,
            } => {
                w.u8(tag::HELLO).u16(*major).u16(*minor).str(name).str(os);
                w.u16(*want_width).u16(*want_height).u16(*want_refresh);
                put_list(w, decoders, put_cap);
            }
            Msg::HelloAck {
                major,
                minor,
                name,
                host_id,
                need_pin,
                rendezvous,
            } => {
                w.u8(tag::HELLO_ACK)
                    .u16(*major)
                    .u16(*minor)
                    .str(name)
                    .str(host_id);
                w.bool(*need_pin).str(rendezvous);
            }
            Msg::PairSpake { data } => {
                w.u8(tag::PAIR_SPAKE).bytes(data);
            }
            Msg::PairConfirm { mac } => {
                w.u8(tag::PAIR_CONFIRM).raw(mac);
            }
            Msg::SessionStart {
                profile,
                path,
                modes,
            } => {
                w.u8(tag::SESSION_START).u8(*profile).u8(*path).u32(*modes);
            }
            Msg::Ping { seq, t_client } => {
                w.u8(tag::PING).u32(*seq).u64(*t_client);
            }
            Msg::Pong {
                seq,
                t_client,
                t_host,
            } => {
                w.u8(tag::PONG).u32(*seq).u64(*t_client).u64(*t_host);
            }
            Msg::Feedback(f) => {
                w.u8(tag::FEEDBACK)
                    .u32(f.seq)
                    .u16(f.window_ms)
                    .u32(f.bytes_rx);
                w.u32(f.shards_rx).u32(f.shards_lost).u32(f.shards_repaired);
                w.u16(f.frames_rx).u16(f.frames_lost);
                w.i32(f.delay_min_us).i32(f.delay_max_us).i32(f.delay_slope);
                w.u32(f.decode_us).u32(f.present_us);
            }
            Msg::FrameLost { stream, frame } => {
                w.u8(tag::FRAME_LOST).u8(*stream).u32(*frame);
            }
            Msg::KeyframeReq { stream } => {
                w.u8(tag::KEYFRAME_REQ).u8(*stream);
            }
            Msg::SetMode { modes } => {
                w.u8(tag::SET_MODE).u32(*modes);
            }
            Msg::MonitorList { monitors } => {
                w.u8(tag::MONITOR_LIST);
                put_list(w, monitors, put_monitor);
            }
            Msg::SelectMonitor { stream, monitor } => {
                w.u8(tag::SELECT_MONITOR).u8(*stream).u8(*monitor);
            }
            Msg::StreamStarted {
                stream,
                monitor,
                codec,
                chroma444,
                width,
                height,
                fps,
            } => {
                w.u8(tag::STREAM_STARTED)
                    .u8(*stream)
                    .u8(*monitor)
                    .u8(*codec)
                    .bool(*chroma444);
                w.u16(*width).u16(*height).u16(*fps);
            }
            Msg::StreamStop { stream } => {
                w.u8(tag::STREAM_STOP).u8(*stream);
            }
            Msg::Resize {
                stream,
                width,
                height,
                refresh,
            } => {
                w.u8(tag::RESIZE)
                    .u8(*stream)
                    .u16(*width)
                    .u16(*height)
                    .u16(*refresh);
            }
            Msg::Clipboard { kind, data } => {
                w.u8(tag::CLIPBOARD).u8(*kind).bytes(data);
            }
            Msg::FileOffer {
                id,
                name,
                size,
                sha256,
            } => {
                w.u8(tag::FILE_OFFER)
                    .u32(*id)
                    .str(name)
                    .u64(*size)
                    .raw(sha256);
            }
            Msg::FileAccept { id } => {
                w.u8(tag::FILE_ACCEPT).u32(*id);
            }
            Msg::FileCancel { id, reason } => {
                w.u8(tag::FILE_CANCEL).u32(*id).str(reason);
            }
            Msg::FileDone { id, ok } => {
                w.u8(tag::FILE_DONE).u32(*id).bool(*ok);
            }
            Msg::PadAttach { idx, kind } => {
                w.u8(tag::PAD_ATTACH).u8(*idx).u8(*kind);
            }
            Msg::PadDetach { idx } => {
                w.u8(tag::PAD_DETACH).u8(*idx);
            }
            Msg::Rumble {
                idx,
                strong,
                weak,
                ms,
            } => {
                w.u8(tag::RUMBLE).u8(*idx).u16(*strong).u16(*weak).u16(*ms);
            }
            Msg::HostStats(s) => {
                w.u8(tag::HOST_STATS)
                    .u8(s.stream)
                    .u32(s.bitrate_kbps)
                    .u16(s.fps);
                w.u8(s.parity_pct).u8(s.profile).str(&s.encoder);
                w.u32(s.capture_us)
                    .u32(s.encode_us)
                    .u32(s.dropped_by_backpressure);
            }
            Msg::Error { code, text } => {
                w.u8(tag::ERROR).u16(*code).str(text);
            }
            Msg::Bye => {
                w.u8(tag::BYE);
            }
        }
    }

    pub fn decode(buf: &[u8]) -> Result<Self> {
        let mut r = Reader::new(buf);
        let m = Self::decode_from(&mut r)?;
        r.finish()?;
        Ok(m)
    }

    pub fn decode_from(r: &mut Reader) -> Result<Self> {
        let t = r.u8()?;
        Ok(match t {
            tag::HELLO => Msg::Hello {
                major: r.u16()?,
                minor: r.u16()?,
                name: r.str()?,
                os: r.str()?,
                want_width: r.u16()?,
                want_height: r.u16()?,
                want_refresh: r.u16()?,
                decoders: get_list(r, get_cap)?,
            },
            tag::HELLO_ACK => Msg::HelloAck {
                major: r.u16()?,
                minor: r.u16()?,
                name: r.str()?,
                host_id: r.str()?,
                need_pin: r.bool()?,
                rendezvous: r.str()?,
            },
            tag::PAIR_SPAKE => Msg::PairSpake { data: r.bytes()? },
            tag::PAIR_CONFIRM => Msg::PairConfirm { mac: r.fixed()? },
            tag::SESSION_START => Msg::SessionStart {
                profile: r.u8()?,
                path: r.u8()?,
                modes: r.u32()?,
            },
            tag::PING => Msg::Ping {
                seq: r.u32()?,
                t_client: r.u64()?,
            },
            tag::PONG => Msg::Pong {
                seq: r.u32()?,
                t_client: r.u64()?,
                t_host: r.u64()?,
            },
            tag::FEEDBACK => Msg::Feedback(Feedback {
                seq: r.u32()?,
                window_ms: r.u16()?,
                bytes_rx: r.u32()?,
                shards_rx: r.u32()?,
                shards_lost: r.u32()?,
                shards_repaired: r.u32()?,
                frames_rx: r.u16()?,
                frames_lost: r.u16()?,
                delay_min_us: r.i32()?,
                delay_max_us: r.i32()?,
                delay_slope: r.i32()?,
                decode_us: r.u32()?,
                present_us: r.u32()?,
            }),
            tag::FRAME_LOST => Msg::FrameLost {
                stream: r.u8()?,
                frame: r.u32()?,
            },
            tag::KEYFRAME_REQ => Msg::KeyframeReq { stream: r.u8()? },
            tag::SET_MODE => Msg::SetMode { modes: r.u32()? },
            tag::MONITOR_LIST => Msg::MonitorList {
                monitors: get_list(r, get_monitor)?,
            },
            tag::SELECT_MONITOR => Msg::SelectMonitor {
                stream: r.u8()?,
                monitor: r.u8()?,
            },
            tag::STREAM_STARTED => Msg::StreamStarted {
                stream: r.u8()?,
                monitor: r.u8()?,
                codec: r.u8()?,
                chroma444: r.bool()?,
                width: r.u16()?,
                height: r.u16()?,
                fps: r.u16()?,
            },
            tag::STREAM_STOP => Msg::StreamStop { stream: r.u8()? },
            tag::RESIZE => Msg::Resize {
                stream: r.u8()?,
                width: r.u16()?,
                height: r.u16()?,
                refresh: r.u16()?,
            },
            tag::CLIPBOARD => Msg::Clipboard {
                kind: r.u8()?,
                data: r.bytes()?,
            },
            tag::FILE_OFFER => Msg::FileOffer {
                id: r.u32()?,
                name: r.str()?,
                size: r.u64()?,
                sha256: r.fixed()?,
            },
            tag::FILE_ACCEPT => Msg::FileAccept { id: r.u32()? },
            tag::FILE_CANCEL => Msg::FileCancel {
                id: r.u32()?,
                reason: r.str()?,
            },
            tag::FILE_DONE => Msg::FileDone {
                id: r.u32()?,
                ok: r.bool()?,
            },
            tag::PAD_ATTACH => Msg::PadAttach {
                idx: r.u8()?,
                kind: r.u8()?,
            },
            tag::PAD_DETACH => Msg::PadDetach { idx: r.u8()? },
            tag::RUMBLE => Msg::Rumble {
                idx: r.u8()?,
                strong: r.u16()?,
                weak: r.u16()?,
                ms: r.u16()?,
            },
            tag::HOST_STATS => Msg::HostStats(HostStats {
                stream: r.u8()?,
                bitrate_kbps: r.u32()?,
                fps: r.u16()?,
                parity_pct: r.u8()?,
                profile: r.u8()?,
                encoder: r.str()?,
                capture_us: r.u32()?,
                encode_us: r.u32()?,
                dropped_by_backpressure: r.u32()?,
            }),
            tag::ERROR => Msg::Error {
                code: r.u16()?,
                text: r.str()?,
            },
            tag::BYE => Msg::Bye,
            t => return Err(DecodeError::BadTag("message", t)),
        })
    }
}
