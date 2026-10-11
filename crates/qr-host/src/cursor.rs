//! Pointer shape forwarding: on displays whose capture leaves the pointer
//! out (X11 through XFixes, and the managed desktop's Xwayland), the client
//! draws the shape itself. XFixes reports every change; each new image
//! goes out as a PNG with its hotspot (`Msg::Cursor`). One service per
//! display, with its own X connection and thread.

use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use qr_proto::Msg;
use tokio::sync::broadcast;
use x11rb::connection::Connection;
use x11rb::protocol::Event;
use x11rb::protocol::xfixes::{self, ConnectionExt as _};
use x11rb::rust_connection::RustConnection;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CursorImage {
    pub width: u16,
    pub height: u16,
    pub xhot: u16,
    pub yhot: u16,
    pub png: Vec<u8>,
    pub serial: u32,
}

impl CursorImage {
    pub fn msg(&self) -> Msg {
        Msg::Cursor { width: self.width, height: self.height, xhot: self.xhot, yhot: self.yhot, png: self.png.clone() }
    }
}

pub struct CursorService {
    tx: broadcast::Sender<CursorImage>,
    current: Arc<Mutex<Option<CursorImage>>>,
}

impl CursorService {
    /// Watches the pointer shape on `display` (`None`: `$DISPLAY`).
    /// `hide`: also hide the pointer on that display (a managed desktop,
    /// whose compositor would otherwise draw it into the capture; never a
    /// real screen someone is sitting at). Hidden or not, XFixes keeps
    /// reporting the shapes.
    pub fn start(display: Option<&str>, hide: bool) -> Result<Self> {
        let (conn, screen_num) = x11rb::connect(display).context("connecting to the X display for the cursor")?;
        let root = conn.setup().roots[screen_num].root;
        conn.xfixes_query_version(5, 0)
            .context("XFixes QueryVersion")?
            .reply()
            .context("the X server has no XFixes")?;
        if hide {
            conn.xfixes_hide_cursor(root)?.check().context("XFixes HideCursor")?;
        }
        conn.xfixes_select_cursor_input(root, xfixes::CursorNotifyMask::DISPLAY_CURSOR)?
            .check()
            .context("XFixes SelectCursorInput")?;
        conn.flush()?;
        let (tx, _) = broadcast::channel(16);
        let current = Arc::new(Mutex::new(None));
        let (bcast, shared) = (tx.clone(), current.clone());
        std::thread::Builder::new().name("cursor".into()).spawn(move || run(conn, bcast, shared))?;
        let shown = display.unwrap_or("$DISPLAY").to_string();
        tracing::info!(x_display = shown, "cursor shapes forwarded through XFixes");
        Ok(Self { tx, current })
    }

    pub fn subscribe(&self) -> broadcast::Receiver<CursorImage> {
        self.tx.subscribe()
    }

    pub fn current(&self) -> Option<CursorImage> {
        self.current.lock().expect("lock").clone()
    }
}

fn run(conn: RustConnection, bcast: broadcast::Sender<CursorImage>, current: Arc<Mutex<Option<CursorImage>>>) {
    let mut last_serial: Option<u32> = None;
    let mut publish = |serial_hint: Option<u32>| {
        if serial_hint.is_some() && serial_hint == last_serial {
            return;
        }
        match fetch(&conn) {
            Ok(img) => {
                if Some(img.serial) == last_serial {
                    return;
                }
                last_serial = Some(img.serial);
                *current.lock().expect("lock") = Some(img.clone());
                let _ = bcast.send(img);
            }
            // BadAccess: the cursor belongs to a client that has exited
            // (xsetroot does that); the client keeps the last shape.
            Err(e) => tracing::debug!("cursor: fetching the image failed: {e:#}"),
        }
    };
    publish(None);
    loop {
        match conn.wait_for_event() {
            Ok(Event::XfixesCursorNotify(e)) => publish(Some(e.cursor_serial)),
            Ok(_) => {}
            Err(e) => {
                tracing::warn!("cursor: the X connection ended: {e:#}");
                return;
            }
        }
    }
}

fn fetch(conn: &RustConnection) -> Result<CursorImage> {
    let reply =
        conn.xfixes_get_cursor_image().context("XFixes GetCursorImage")?.reply().context("GetCursorImage reply")?;
    let (w, h) = (reply.width as u32, reply.height as u32);
    Ok(CursorImage {
        width: reply.width,
        height: reply.height,
        xhot: reply.xhot,
        yhot: reply.yhot,
        png: encode_png(w, h, &reply.cursor_image)?,
        serial: reply.cursor_serial,
    })
}

/// X hands back premultiplied ARGB; PNG and CSS want straight RGBA.
pub fn encode_png(w: u32, h: u32, argb: &[u32]) -> Result<Vec<u8>> {
    let mut rgba = Vec::with_capacity((w * h * 4) as usize);
    for px in argb.iter().take((w * h) as usize) {
        let a = (px >> 24) & 0xff;
        let un = |c: u32| -> u8 { (c * 255 + a / 2).checked_div(a).map_or(0, |v| v.min(255) as u8) };
        rgba.extend_from_slice(&[un((px >> 16) & 0xff), un((px >> 8) & 0xff), un(px & 0xff), a as u8]);
    }
    rgba.resize((w * h * 4) as usize, 0);
    let mut png = Vec::new();
    {
        let mut enc = png::Encoder::new(&mut png, w.max(1), h.max(1));
        enc.set_color(png::ColorType::Rgba);
        enc.set_depth(png::BitDepth::Eight);
        let mut writer = enc.write_header().context("PNG header")?;
        let data = if w == 0 || h == 0 { vec![0u8; 4] } else { rgba };
        writer.write_image_data(&data).context("PNG data")?;
    }
    Ok(png)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn png_round_trip_unpremultiplies() {
        // Half-transparent red, premultiplied: a=128, r=64.
        let png = encode_png(1, 1, &[0x8040_0000]).unwrap();
        let dec = png::Decoder::new(std::io::Cursor::new(png));
        let mut reader = dec.read_info().unwrap();
        let mut buf = vec![0; reader.output_buffer_size().unwrap()];
        let info = reader.next_frame(&mut buf).unwrap();
        assert_eq!((info.width, info.height), (1, 1));
        assert_eq!(&buf[..4], &[128, 0, 0, 128]);
    }

    #[test]
    fn empty_cursor_is_a_valid_png() {
        let png = encode_png(0, 0, &[]).unwrap();
        assert!(png.starts_with(b"\x89PNG"));
    }
}
