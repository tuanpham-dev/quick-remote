//! What screen a stream shows, and at what size and scale: a client's
//! `DisplayRequest` applied to this desktop, answered with `DisplayInfo`.
//!
//! - The host's own virtual display (sway): any size, by output mode, and
//!   the desktop scale by output scale.
//! - KDE and GNOME: a virtual monitor of the client's own, or the real
//!   monitor's mode (see `kwin.rs`, `mutter.rs`).
//! - X11 and Windows: the desktop keeps its size; the client fits it.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use qr_proto::Msg;
use qr_proto::msg::{display_kind, display_target};

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

/// How long a client's virtual monitor outlives its session, so a
/// reconnect finds its windows where they were.
const KEEP: Duration = Duration::from_secs(60);

/// The virtual monitors clients own, by client identity.
#[derive(Default)]
pub struct Registry {
    held: Mutex<HashMap<String, Held>>,
}

struct Held {
    name: String,
    size: (u32, u32),
    scale_pct: u16,
    /// A session uses it now.
    in_use: bool,
    /// Bumped on every acquire and release: a removal timer only acts when
    /// nothing happened since it started.
    generation: u64,
}

/// The name a client's virtual monitor gets.
pub fn virtual_name(client: &str) -> String {
    format!("QuickRemote-{}", &client[..client.len().min(8)])
}

impl Registry {
    /// The client's virtual monitor if it still has `size` and `scale_pct`.
    fn same(&self, client: &str, size: (u32, u32), scale_pct: u16) -> Option<String> {
        let h = self.held.lock().expect("lock");
        h.get(client).filter(|v| v.size == size && v.scale_pct == scale_pct).map(|v| v.name.clone())
    }

    fn hold(&self, client: &str, name: String, size: (u32, u32), scale_pct: u16) {
        let mut h = self.held.lock().expect("lock");
        let generation = h.get(client).map_or(0, |v| v.generation + 1);
        h.insert(client.into(), Held { name, size, scale_pct, in_use: true, generation });
    }

    /// The session using `client`'s virtual monitor ended (or switched to
    /// the real monitor): remove the monitor in a minute unless it comes
    /// back.
    pub fn release(&self, ctx: &Arc<HostCtx>, client: &str) {
        let generation = {
            let mut h = self.held.lock().expect("lock");
            let Some(v) = h.get_mut(client) else { return };
            v.in_use = false;
            v.generation += 1;
            v.generation
        };
        let (ctx, client) = (ctx.clone(), client.to_string());
        tokio::spawn(async move {
            tokio::time::sleep(KEEP).await;
            let name = {
                let mut h = ctx.displays.held.lock().expect("lock");
                match h.get(&client) {
                    Some(v) if !v.in_use && v.generation == generation => h.remove(&client).map(|v| v.name),
                    _ => None,
                }
            };
            if let Some(name) = name {
                if let Some(k) = &ctx.kwin {
                    k.remove_virtual(&name);
                }
                if let Some(m) = &ctx.mutter {
                    m.stop_virtual(&name).await;
                }
            }
        });
    }
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
    let kind = kind(ctx);
    // A desktop that keeps its size says so from the start (the client
    // sends it no requests).
    let note = if note.is_empty() && kind == display_kind::FIXED { FIXED_NOTE.into() } else { note };
    Msg::DisplayInfo { stream, kind, width, height, scale_pct: video.scale_pct(stream), note }
}

const FIXED_NOTE: &str = "This desktop keeps its own size; the picture is fitted to your window.";

/// The largest picture a session can carry: the codec's limits (H.264
/// level 5.2: 4096 wide, 36 864 macroblocks) and the client decoder's.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Limit {
    pub width: u32,
    pub height: u32,
    /// Most 16×16 macroblocks in a frame.
    pub mbs: u32,
}

impl Limit {
    pub fn new(enc: &qr_media::probe::EncoderInfo, client: &qr_proto::msg::CodecCap) -> Self {
        let (w, h, mbs) = match enc.codec {
            qr_proto::Codec::H264 => (4096, 4096, 36_864),
            _ => (8192, 4352, 139_264),
        };
        let cap = |ours: u32, theirs: u32| if theirs > 0 { ours.min(theirs) } else { ours };
        Self {
            width: cap(cap(w, enc.max_width), client.max_width as u32),
            height: cap(cap(h, enc.max_height), client.max_height as u32),
            mbs,
        }
    }

    /// `r` shrunk to fit, its scale lowered by as much so the desktop keeps
    /// its logical size; with a note saying so when it had to shrink.
    pub fn fit(&self, r: Request) -> (Request, Option<String>) {
        let (w, h) = (r.width.max(1) as f64, r.height.max(1) as f64);
        let mbs = (w / 16.0).ceil() * (h / 16.0).ceil();
        let f = (self.width as f64 / w).min(self.height as f64 / h).min((self.mbs as f64 / mbs).sqrt()).min(1.0);
        if f >= 1.0 {
            return (r, None);
        }
        // A little under, so rounding up to whole macroblocks still fits.
        let f = f * 0.995;
        let even = |v: f64| ((v.floor() as u16) & !1).max(2);
        let out = Request {
            width: even(w * f),
            height: even(h * f),
            scale_pct: ((r.scale_pct as f64 * f).round() as u16).max(50),
            ..r
        };
        let note = format!(
            "{}×{} is more than this connection's video can carry; using {}×{} at {}% (the same desktop space), scaled to your window.",
            r.width, r.height, out.width, out.height, out.scale_pct
        );
        (out, Some(note))
    }
}

/// An even size within what encoders take.
fn even(w: u16, h: u16) -> (u32, u32) {
    ((w.clamp(320, 7680) & !1) as u32, (h.clamp(200, 4320) & !1) as u32)
}

/// Applies `r` to stream `stream` of `client`'s session; returns the
/// `DisplayInfo` to send.
pub async fn apply(ctx: &Arc<HostCtx>, video: &Arc<Video>, client: &str, stream: u8, r: Request, limit: Limit) -> Msg {
    apply_fitted(ctx, video, client, stream, limit.fit(r)).await
}

async fn apply_fitted(
    ctx: &Arc<HostCtx>,
    video: &Arc<Video>,
    client: &str,
    stream: u8,
    (r, shrunk): (Request, Option<String>),
) -> Msg {
    let Some((monitor, (cw, ch, cfps))) = video.current(stream) else {
        return info(ctx, video, stream, String::new());
    };
    // A note about the size limit, unless a later step has its own.
    let info = |ctx: &HostCtx, video: &Video, stream: u8, note: String| {
        info(ctx, video, stream, if note.is_empty() { shrunk.clone().unwrap_or_default() } else { note })
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
        display_kind::KDE => {
            let (w, h) = even(r.width, r.height);
            let can_virtual = ctx.kwin.as_ref().is_some_and(|k| k.can_create_virtual());
            let note = if r.target == display_target::VIRTUAL && can_virtual {
                kde_virtual(ctx, video, client, stream, (w, h, fps), scale).await
            } else if r.target == display_target::VIRTUAL {
                // Headless KWin: no virtual monitors; the real one instead.
                let real = kde_real(ctx, video, stream, (w, h, fps), scale).await;
                Err(anyhow::anyhow!(
                    "This KDE session can't add virtual monitors (headless KWin), so the real monitor is used{}",
                    real.err().map(|e| format!(": {e:#}")).unwrap_or_default()
                ))
            } else {
                ctx.displays.release(ctx, client);
                kde_real(ctx, video, stream, (w, h, fps), scale).await
            };
            info(ctx, video, stream, note.err().map(|e| format!("{e:#}")).unwrap_or_default())
        }
        display_kind::GNOME => {
            let (w, h) = even(r.width, r.height);
            let note = if r.target == display_target::VIRTUAL {
                gnome_virtual(ctx, video, client, stream, (w, h, fps), scale).await
            } else {
                ctx.displays.release(ctx, client);
                gnome_real(ctx, video, stream, (w, h, fps), scale).await
            };
            info(ctx, video, stream, note.err().map(|e| format!("{e:#}")).unwrap_or_default())
        }
        _ => info(ctx, video, stream, FIXED_NOTE.into()),
    }
}

/// GNOME: the client's own virtual monitor. mutter creates it at the size
/// its first reader asks for, so capture starts before it is laid out;
/// then it gets its scale (and becomes primary when every monitor is
/// virtual).
async fn gnome_virtual(
    ctx: &Arc<HostCtx>,
    video: &Arc<Video>,
    client: &str,
    stream: u8,
    (w, h, fps): (u32, u32, u32),
    scale: u16,
) -> anyhow::Result<()> {
    let m = ctx.mutter.as_ref().expect("mutter");
    let name = virtual_name(client);
    if let Some(mon) = m.virtual_monitor(&name)
        && ctx.displays.same(client, (w, h), scale).is_some()
    {
        ctx.displays.hold(client, name, (w, h), scale);
        // Its scale first: the capture asks for scale × the logical size.
        video.set_scale_pct(stream, scale);
        return video.start(stream, mon.id, (w, h, fps)).await;
    }
    m.stop_virtual(&name).await;
    let before: Vec<String> = m.layout().await?.into_iter().map(|l| l.0).collect();
    let (path, node) = m.record_virtual().await?;
    video.set_scale_pct(stream, 100);
    video.start_pending(stream, node, (w, h, fps)).await?;
    // The monitor appears with the first frame read.
    let mut new = None;
    for _ in 0..60 {
        if let Some(l) = m.layout().await?.into_iter().find(|l| !before.contains(&l.0)) {
            new = Some(l);
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let (connector, x, y, lw, lh, mhz, primary) =
        new.ok_or_else(|| anyhow::anyhow!("mutter didn't lay out the virtual monitor"))?;
    let monitor = qr_proto::Monitor {
        id: 0,
        name: connector.clone(),
        width: lw as u16,
        height: lh as u16,
        refresh_mhz: mhz,
        x,
        y,
        primary,
    };
    let id = m.add_virtual(&name, monitor, path, node);
    ctx.displays.hold(client, name, (w, h), scale);
    // Primary only on a desktop without a physical screen (mutter's own
    // headless and virtual monitors are Meta-N).
    let physical = m.layout().await?.iter().any(|l| !l.0.starts_with("Meta-"));
    let applied = match m.configure(&connector, None, scale, !physical).await {
        Ok((pct, _)) => pct,
        Err(e) => {
            tracing::warn!("configuring {connector}: {e:#}");
            100
        }
    };
    m.refresh().await?;
    video.set_scale_pct(stream, applied);
    video.attach(stream, id);
    if applied != scale {
        anyhow::bail!("scale {}% isn't supported here; using {applied}%", scale);
    }
    Ok(())
}

/// GNOME: the real monitor switched to the requested mode when it lists
/// it, with the scale.
async fn gnome_real(
    ctx: &HostCtx,
    video: &Arc<Video>,
    stream: u8,
    (w, h, fps): (u32, u32, u32),
    scale: u16,
) -> anyhow::Result<()> {
    let m = ctx.mutter.as_ref().expect("mutter");
    let current = video.current(stream).map(|(id, _)| id);
    let mons = ctx.monitors();
    let mon = mons
        .iter()
        .find(|x| Some(x.id) == current && !m.is_virtual(x.id))
        .or_else(|| mons.iter().find(|x| !m.is_virtual(x.id)))
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("this desktop has no monitor of its own"))?;
    let (applied, fits) = m.configure(&mon.name, Some((w, h, fps)), scale, false).await?;
    m.refresh().await?;
    video.set_scale_pct(stream, applied);
    // The monitor's stream ends with the change: start it anew.
    let now = ctx.monitors().into_iter().find(|x| x.name == mon.name).unwrap_or(mon);
    let k = applied as f64 / 100.0;
    let size = ((now.width as f64 * k).round() as u32, (now.height as f64 * k).round() as u32);
    video.start(stream, now.id, (size.0, size.1, fps)).await?;
    if !fits {
        anyhow::bail!("{w}×{h} isn't available on {}; showing {}×{} scaled", now.name, size.0, size.1);
    }
    Ok(())
}

/// The monitor called `name` once the desktop lists it (a new output takes
/// a moment to appear).
async fn monitor_named(ctx: &HostCtx, name: &str) -> anyhow::Result<qr_proto::Monitor> {
    for _ in 0..40 {
        if let Some(m) = ctx.monitors().into_iter().find(|m| m.name == name) {
            return Ok(m);
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    anyhow::bail!("the new monitor {name} didn't appear")
}

/// KDE: the client's own virtual monitor at exactly its size and scale,
/// made primary when the desktop has no physical screen.
async fn kde_virtual(
    ctx: &Arc<HostCtx>,
    video: &Arc<Video>,
    client: &str,
    stream: u8,
    (w, h, fps): (u32, u32, u32),
    scale: u16,
) -> anyhow::Result<()> {
    let name = virtual_name(client);
    let exists = ctx.kwin.as_ref().is_some_and(|k| k.is_ours(&name));
    if !(exists && ctx.displays.same(client, (w, h), scale).is_some()) {
        let (c, n) = (ctx.clone(), name.clone());
        tokio::task::spawn_blocking(move || c.kwin.as_ref().expect("kwin").create_virtual(&n, (w, h), scale)).await??;
    }
    ctx.displays.hold(client, name.clone(), (w, h), scale);
    let mon = monitor_named(ctx, &name).await?;
    let physical = ctx
        .monitors()
        .iter()
        .any(|m| !m.name.starts_with("Virtual-") && !ctx.kwin.as_ref().is_some_and(|k| k.is_ours(&m.name)));
    if !physical && let Err(e) = crate::kwin::kscreen(&[&format!("output.{name}.priority.1")]) {
        tracing::warn!("making {name} the primary monitor: {e:#}");
    }
    video.set_scale_pct(stream, scale);
    video.start(stream, mon.id, (w, h, fps)).await
}

/// KDE: the real monitor switched to the requested mode when it lists it.
async fn kde_real(
    ctx: &HostCtx,
    video: &Arc<Video>,
    stream: u8,
    (w, h, fps): (u32, u32, u32),
    scale: u16,
) -> anyhow::Result<()> {
    let ours = |n: &str| ctx.kwin.as_ref().is_some_and(|k| k.is_ours(n));
    let current = video.current(stream).map(|(m, _)| m);
    let mons = ctx.monitors();
    let mon = mons
        .iter()
        .find(|m| Some(m.id) == current && !ours(&m.name))
        .or_else(|| mons.iter().find(|m| !ours(&m.name)))
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("this desktop has no monitor of its own"))?;
    let modes = crate::kwin::kscreen_modes(&mon.name).unwrap_or_default();
    let fits = modes.iter().any(|&(mw, mh, hz)| (mw, mh) == (w, h) && (fps == 0 || hz.abs_diff(fps) <= 1));
    let mut args = vec![format!("output.{}.scale.{}", mon.name, scale as f64 / 100.0)];
    if fits {
        args.push(format!("output.{}.mode.{w}x{h}@{fps}", mon.name));
    }
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    if let Err(e) = crate::kwin::kscreen(&args) {
        tracing::warn!("changing {}: {e:#}", mon.name);
    }
    // What the monitor has now (a change may be refused).
    let applied = crate::kwin::kscreen_scale_pct(&mon.name).unwrap_or(100);
    video.set_scale_pct(stream, applied);
    let now = ctx.monitors().into_iter().find(|m| m.name == mon.name).unwrap_or(mon);
    let k = applied as f64 / 100.0;
    let size = ((now.width as f64 * k).round() as u32, (now.height as f64 * k).round() as u32);
    video.start(stream, now.id, (size.0, size.1, fps)).await?;
    if !fits {
        anyhow::bail!("{w}×{h} isn't available on {}; showing {}×{} scaled", now.name, size.0, size.1);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use qr_proto::Codec;
    use qr_media::probe::{EncoderInfo, Family};
    use qr_proto::msg::CodecCap;

    fn enc(codec: Codec, max: u32) -> EncoderInfo {
        EncoderInfo { element: "x", codec, family: Family::X264, hw: true, chroma444: false, max_width: max, max_height: max }
    }

    fn req(width: u16, height: u16, scale_pct: u16) -> Request {
        Request { width, height, refresh: 60, scale_pct, target: 0 }
    }

    #[test]
    fn fits_requests_to_the_codec_keeping_the_desktop_size() {
        let cap = CodecCap { codec: Codec::H264, hw: true, max_width: 0, max_height: 0, chroma444: false };
        let h264 = Limit::new(&enc(Codec::H264, 16384), &cap);
        // Within limits: unchanged.
        assert_eq!(h264.fit(req(2548, 1320, 200)), (req(2548, 1320, 200), None));
        // A 2560×1285 fullscreen window at 2×: too many macroblocks.
        let (r, note) = h264.fit(req(5120, 2570, 200));
        let mbs = r.width.div_ceil(16) as u32 * r.height.div_ceil(16) as u32;
        assert!(r.width <= 4096 && mbs <= 36_864 && note.is_some(), "{r:?}");
        assert!(r.width % 2 == 0 && r.height % 2 == 0);
        // Same logical size (within a percent), same shape.
        let logical = r.width as f64 / (r.scale_pct as f64 / 100.0);
        assert!((logical - 2560.0).abs() < 26.0, "{logical}");
        assert!((r.width as f64 / r.height as f64 - 5120.0 / 2570.0).abs() < 0.01);
        // The client's decoder limit counts too.
        let small = CodecCap { max_width: 1920, max_height: 1080, ..cap };
        let (r, _) = Limit::new(&enc(Codec::H265, 16384), &small).fit(req(3840, 2160, 100));
        assert!(r.width <= 1920 && r.height <= 1080 && r.scale_pct == 50, "{r:?}");
        // And the encoder's: H.265 on a 4096-wide GPU takes 5K at 4096 wide.
        let (r, _) = Limit::new(&enc(Codec::H265, 4096), &cap).fit(req(5120, 2570, 200));
        assert!(r.width <= 4096 && r.width >= 4070 && r.scale_pct == 159, "{r:?}");
    }
}
