//! Input-stream events (client → host, reliable, in order).
//!
//! Key and button state travels here so a lost packet can never leave a key
//! held. Pointer motion and gamepad state also go out as datagrams
//! (`datagram::Motion`, `datagram::PadState`) for latency; those carry
//! running totals or full snapshots so loss never corrupts them.

use crate::codec::{DecodeError, Reader, Result, Writer};

/// Pen flags (`InputEvent::Pen`).
pub mod pen {
    pub const IN_RANGE: u8 = 1 << 0;
    pub const CONTACT: u8 = 1 << 1;
    pub const ERASER: u8 = 1 << 2;
    pub const BARREL: u8 = 1 << 3;
}

#[derive(Debug, Clone, PartialEq)]
pub enum InputEvent {
    /// `code` is a Linux evdev key code (KEY_*), by physical position.
    Key { code: u16, down: bool },
    /// 1 left, 2 middle, 3 right, 4 back, 5 forward.
    Button { button: u8, down: bool },
    /// Absolute pointer, in pixels of the given stream's picture.
    Abs { stream: u8, x: u16, y: u16 },
    /// Wheel in 1/120 notches (positive = up / right).
    Wheel { dx: i16, dy: i16 },
    /// Committed text from an IME or phone keyboard.
    Text { text: String },
    /// Release every held key and button.
    ReleaseAll,
    /// Pen sample. `x`,`y` in stream pixels ×16 (sub-pixel), `pressure`
    /// 0..=65535 (rescaled by the client from its device range), tilt in
    /// degrees −90..=90.
    Pen { stream: u8, x: u32, y: u32, pressure: u16, tilt_x: i8, tilt_y: i8, flags: u8 },
}

mod tag {
    pub const KEY: u8 = 1;
    pub const BUTTON: u8 = 2;
    pub const ABS: u8 = 3;
    pub const WHEEL: u8 = 4;
    pub const TEXT: u8 = 5;
    pub const RELEASE_ALL: u8 = 6;
    pub const PEN: u8 = 7;
}

impl InputEvent {
    pub fn encode(&self) -> Vec<u8> {
        let mut w = Writer::with_capacity(16);
        match self {
            Self::Key { code, down } => {
                w.u8(tag::KEY).u16(*code).bool(*down);
            }
            Self::Button { button, down } => {
                w.u8(tag::BUTTON).u8(*button).bool(*down);
            }
            Self::Abs { stream, x, y } => {
                w.u8(tag::ABS).u8(*stream).u16(*x).u16(*y);
            }
            Self::Wheel { dx, dy } => {
                w.u8(tag::WHEEL).i16(*dx).i16(*dy);
            }
            Self::Text { text } => {
                w.u8(tag::TEXT).str(text);
            }
            Self::ReleaseAll => {
                w.u8(tag::RELEASE_ALL);
            }
            Self::Pen { stream, x, y, pressure, tilt_x, tilt_y, flags } => {
                w.u8(tag::PEN).u8(*stream).u32(*x).u32(*y).u16(*pressure);
                w.i8(*tilt_x).i8(*tilt_y).u8(*flags);
            }
        }
        w.buf
    }

    pub fn decode(buf: &[u8]) -> Result<Self> {
        let mut r = Reader::new(buf);
        let ev = match r.u8()? {
            tag::KEY => Self::Key { code: r.u16()?, down: r.bool()? },
            tag::BUTTON => Self::Button { button: r.u8()?, down: r.bool()? },
            tag::ABS => Self::Abs { stream: r.u8()?, x: r.u16()?, y: r.u16()? },
            tag::WHEEL => Self::Wheel { dx: r.i16()?, dy: r.i16()? },
            tag::TEXT => Self::Text { text: r.str()? },
            tag::RELEASE_ALL => Self::ReleaseAll,
            tag::PEN => Self::Pen {
                stream: r.u8()?,
                x: r.u32()?,
                y: r.u32()?,
                pressure: r.u16()?,
                tilt_x: r.i8()?,
                tilt_y: r.i8()?,
                flags: r.u8()?,
            },
            t => return Err(DecodeError::BadTag("input", t)),
        };
        r.finish()?;
        Ok(ev)
    }
}
