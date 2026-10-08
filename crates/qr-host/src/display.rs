//! What screen a stream shows, and at what size and scale: a client's
//! `DisplayRequest` applied to this desktop, answered with `DisplayInfo`.
//!
//! - The host's own virtual display (sway): any size, by output mode, and
//!   the desktop scale by output scale.
//! - KDE and GNOME: a virtual monitor of the client's own, or the real
//!   monitor's mode (see `kwin.rs`, `mutter.rs`).
//! - X11 and Windows: the desktop keeps its size; the client fits it.

use std::sync::Arc;

use qr_proto::Msg;
use qr_proto::msg::display_kind;

use crate::HostCtx;
use crate::video::Video;

/// A client's request, as received.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Request {
    pub width: u16,
    pub height: u16,
    pub refresh: u16,
    pub scale_pct: u16,
    pub target: u8,
}

/// What this desktop can do.
pub fn kind(ctx: &HostCtx) -> u8 {
    if ctx.vd.is_some() {
        display_kind::OWN
    } else if ctx.kwin.is_some() {
        display_kind::KDE
    } else if ctx.mutter.is_some() {
        display_kind::GNOME
    } else {
        display_kind::FIXED
    }
}

/// Stream `stream`'s screen as it is now.
pub fn info(ctx: &HostCtx, video: &Video, stream: u8, note: String) -> Msg {
    let (width, height) = video.current(stream).map(|(_, (w, h, _))| (w as u16, h as u16)).unwrap_or((0, 0));
    Msg::DisplayInfo { stream, kind: kind(ctx), width, height, scale_pct: video.scale_pct(stream), note }
}

/// An even size within what encoders take.
fn even(w: u16, h: u16) -> (u32, u32) {
    ((w.clamp(320, 7680) & !1) as u32, (h.clamp(200, 4320) & !1) as u32)
}

/// Applies `r` to stream `stream`; returns the `DisplayInfo` to send.
pub async fn apply(ctx: &HostCtx, video: &Arc<Video>, stream: u8, r: Request) -> Msg {
    let Some((monitor, (cw, ch, cfps))) = video.current(stream) else {
        return info(ctx, video, stream, String::new());
    };
    let fps = if r.refresh > 0 { (r.refresh as u32).min(240) } else { cfps };
    let scale = r.scale_pct.clamp(50, 400);
    match kind(ctx) {
        display_kind::OWN => {
            let (w, h) = even(r.width, r.height);
            let name = ctx.monitors().into_iter().find(|m| m.id == monitor).map(|m| m.name);
            if let (Some(vd), Some(name)) = (&ctx.vd, &name)
                && scale != video.scale_pct(stream)
            {
                if let Err(e) = vd.lock().expect("lock").set_scale(name, scale) {
                    tracing::warn!("setting the desktop scale: {e:#}");
                }
                video.set_scale_pct(stream, scale);
            }
            if (w, h, fps) != (cw, ch, cfps)
                && let Err(e) = video.start(stream, monitor, (w, h, fps)).await
            {
                tracing::warn!("resizing the display: {e:#}");
                return info(ctx, video, stream, format!("Couldn't change the size: {e}"));
            }
            info(ctx, video, stream, String::new())
        }
        _ => info(ctx, video, stream, "This desktop keeps its own size; the picture is fitted to your window.".into()),
    }
}
