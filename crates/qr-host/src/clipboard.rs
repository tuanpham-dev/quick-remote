//! Host clipboard ↔ client, for X11 displays (CLIPBOARD selection).

use std::sync::mpsc;

use qr_clip::{Command, Content};
use qr_proto::Msg;
use qr_proto::msg::ClipKind;

use crate::HostCtx;

pub struct Clipboard {
    tx: mpsc::Sender<Command>,
}

/// Starts clipboard sync for an X11 source; local changes go to the client.
pub async fn start(ctx: &HostCtx, ctl: qr_net::files::Control) -> Option<Clipboard> {
    // A Wayland desktop or the virtual display: wlr-data-control.
    #[cfg(target_os = "linux")]
    let wayland = ctx
        .screencopy_socket()
        .or_else(|| ctx.kwin.as_ref().map(|k| k.socket.clone()))
        .map(|s| crate::wlclip::spawn(&s));
    #[cfg(not(target_os = "linux"))]
    let wayland: Option<anyhow::Result<_>> = None;
    // GNOME: the remote-desktop session's selection.
    #[cfg(target_os = "linux")]
    let gnome = match &ctx.mutter {
        Some(m) => Some(m.clipboard().await),
        None => None,
    };
    #[cfg(not(target_os = "linux"))]
    let gnome: Option<anyhow::Result<_>> = None;
    let started = match gnome.or(wayland) {
        Some(r) => r,
        None => qr_clip::spawn(ctx.args.source.strip_prefix("x11:")?),
    };
    let (tx, rx) = match started {
        Ok(x) => x,
        Err(e) => {
            tracing::warn!("clipboard is off: {e:#}");
            return None;
        }
    };
    std::thread::spawn(move || {
        while let Ok(c) = rx.recv() {
            let m = match c {
                Content::Text(t) => Msg::Clipboard { kind: ClipKind::Text as u8, data: t.into_bytes() },
                Content::Png(p) => Msg::Clipboard { kind: ClipKind::Png as u8, data: p },
            };
            ctl(m);
        }
    });
    Some(Clipboard { tx })
}

pub fn from_client(c: &Clipboard, kind: u8, data: &[u8]) {
    let content = if kind == ClipKind::Png as u8 {
        Content::Png(data.to_vec())
    } else {
        Content::Text(String::from_utf8_lossy(data).into_owned())
    };
    let _ = c.tx.send(Command::Set(content));
}
