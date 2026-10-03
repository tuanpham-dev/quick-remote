//! Little-endian primitive reader and writer shared by every message.
//!
//! Strings and byte blobs are a `u32` length followed by the bytes. Every
//! `Reader` method fails with [`DecodeError`] instead of panicking, so a
//! malformed peer can never crash a session.

use std::fmt;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DecodeError {
    Truncated,
    BadTag(&'static str, u8),
    BadUtf8,
    TooLong(usize),
    Trailing(usize),
}

impl fmt::Display for DecodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Truncated => write!(f, "message truncated"),
            Self::BadTag(what, t) => write!(f, "unknown {what} tag {t}"),
            Self::BadUtf8 => write!(f, "string is not UTF-8"),
            Self::TooLong(n) => write!(f, "field of {n} bytes exceeds the limit"),
            Self::Trailing(n) => write!(f, "{n} trailing bytes"),
        }
    }
}

impl std::error::Error for DecodeError {}

pub type Result<T> = std::result::Result<T, DecodeError>;

/// Largest string or blob a message may carry (a clipboard image).
pub const MAX_BLOB: usize = 16 * 1024 * 1024;

#[derive(Default, Debug)]
pub struct Writer {
    pub buf: Vec<u8>,
}

impl Writer {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_capacity(n: usize) -> Self {
        Self {
            buf: Vec::with_capacity(n),
        }
    }

    pub fn u8(&mut self, v: u8) -> &mut Self {
        self.buf.push(v);
        self
    }
    pub fn bool(&mut self, v: bool) -> &mut Self {
        self.u8(v as u8)
    }
    pub fn u16(&mut self, v: u16) -> &mut Self {
        self.buf.extend_from_slice(&v.to_le_bytes());
        self
    }
    pub fn i8(&mut self, v: i8) -> &mut Self {
        self.u8(v as u8)
    }
    pub fn i16(&mut self, v: i16) -> &mut Self {
        self.buf.extend_from_slice(&v.to_le_bytes());
        self
    }
    pub fn u32(&mut self, v: u32) -> &mut Self {
        self.buf.extend_from_slice(&v.to_le_bytes());
        self
    }
    pub fn i32(&mut self, v: i32) -> &mut Self {
        self.buf.extend_from_slice(&v.to_le_bytes());
        self
    }
    pub fn u64(&mut self, v: u64) -> &mut Self {
        self.buf.extend_from_slice(&v.to_le_bytes());
        self
    }
    pub fn i64(&mut self, v: i64) -> &mut Self {
        self.buf.extend_from_slice(&v.to_le_bytes());
        self
    }
    pub fn f32(&mut self, v: f32) -> &mut Self {
        self.buf.extend_from_slice(&v.to_le_bytes());
        self
    }
    pub fn raw(&mut self, v: &[u8]) -> &mut Self {
        self.buf.extend_from_slice(v);
        self
    }
    pub fn bytes(&mut self, v: &[u8]) -> &mut Self {
        self.u32(v.len() as u32).raw(v)
    }
    pub fn str(&mut self, v: &str) -> &mut Self {
        self.bytes(v.as_bytes())
    }
}

pub struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    pub fn new(buf: &'a [u8]) -> Self {
        Self { buf, pos: 0 }
    }

    pub fn remaining(&self) -> usize {
        self.buf.len() - self.pos
    }

    pub fn finish(&self) -> Result<()> {
        match self.remaining() {
            0 => Ok(()),
            n => Err(DecodeError::Trailing(n)),
        }
    }

    pub fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        if self.remaining() < n {
            return Err(DecodeError::Truncated);
        }
        let s = &self.buf[self.pos..self.pos + n];
        self.pos += n;
        Ok(s)
    }

    pub fn rest(&mut self) -> &'a [u8] {
        let s = &self.buf[self.pos..];
        self.pos = self.buf.len();
        s
    }

    fn arr<const N: usize>(&mut self) -> Result<[u8; N]> {
        Ok(self.take(N)?.try_into().expect("length checked"))
    }

    pub fn u8(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
    }
    pub fn bool(&mut self) -> Result<bool> {
        Ok(self.u8()? != 0)
    }
    pub fn u16(&mut self) -> Result<u16> {
        Ok(u16::from_le_bytes(self.arr()?))
    }
    pub fn i8(&mut self) -> Result<i8> {
        Ok(self.u8()? as i8)
    }
    pub fn i16(&mut self) -> Result<i16> {
        Ok(i16::from_le_bytes(self.arr()?))
    }
    pub fn u32(&mut self) -> Result<u32> {
        Ok(u32::from_le_bytes(self.arr()?))
    }
    pub fn i32(&mut self) -> Result<i32> {
        Ok(i32::from_le_bytes(self.arr()?))
    }
    pub fn u64(&mut self) -> Result<u64> {
        Ok(u64::from_le_bytes(self.arr()?))
    }
    pub fn i64(&mut self) -> Result<i64> {
        Ok(i64::from_le_bytes(self.arr()?))
    }
    pub fn f32(&mut self) -> Result<f32> {
        Ok(f32::from_le_bytes(self.arr()?))
    }
    pub fn fixed<const N: usize>(&mut self) -> Result<[u8; N]> {
        self.arr()
    }
    pub fn bytes(&mut self) -> Result<Vec<u8>> {
        let n = self.u32()? as usize;
        if n > MAX_BLOB {
            return Err(DecodeError::TooLong(n));
        }
        Ok(self.take(n)?.to_vec())
    }
    pub fn str(&mut self) -> Result<String> {
        String::from_utf8(self.bytes()?).map_err(|_| DecodeError::BadUtf8)
    }
}

/// Encodes a list as a `u16` count followed by the items.
pub fn put_list<T>(w: &mut Writer, items: &[T], mut f: impl FnMut(&mut Writer, &T)) {
    w.u16(items.len() as u16);
    for it in items {
        f(w, it);
    }
}

pub fn get_list<'a, T>(
    r: &mut Reader<'a>,
    mut f: impl FnMut(&mut Reader<'a>) -> Result<T>,
) -> Result<Vec<T>> {
    let n = r.u16()? as usize;
    let mut v = Vec::with_capacity(n.min(1024));
    for _ in 0..n {
        v.push(f(r)?);
    }
    Ok(v)
}
