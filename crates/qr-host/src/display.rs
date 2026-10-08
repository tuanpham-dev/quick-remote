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
            if let (Some(name), Some(k)) = (name, &ctx.kwin) {
                k.remove_virtual(&name);
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
    Msg::DisplayInfo { stream, kind: kind(ctx), width, height, scale_pct: video.scale_pct(stream), note }
}

/// An even size within what encoders take.
fn even(w: u16, h: u16) -> (u32, u32) {
    ((w.clamp(320, 7680) & !1) as u32, (h.clamp(200, 4320) & !1) as u32)
}

/// Applies `r` to stream `stream` of `client`'s session; returns the
/// `DisplayInfo` to send.
pub async fn apply(ctx: &Arc<HostCtx>, video: &Arc<Video>, client: &str, stream: u8, r: Request) -> Msg {
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
        _ => info(ctx, video, stream, "This desktop keeps its own size; the picture is fitted to your window.".into()),
    }
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
