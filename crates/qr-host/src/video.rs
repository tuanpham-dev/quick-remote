//! The session's video streams: for each (stream 0 for the main window,
//! more for extra windows) which monitor, at what mode, and the encoder
//! running it. Restartable, for monitor switches and resizes.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use qr_media::encode::{EncodeConfig, Source};
use qr_media::probe::EncoderInfo;
use qr_proto::{Monitor, Msg};

use crate::HostCtx;
use crate::ratectl::{Limits, RateCtl};
use crate::stream::VideoStream;

/// Streams a session may run at once.
pub const MAX_STREAMS: u8 = 4;

struct Current {
    stream: Arc<VideoStream>,
    monitor: u8,
    mode: (u32, u32, u32),
    // The virtual display's capture, feeding the stream's encoder.
    _capture: Option<crate::screencopy::ScreenCopy>,
}

pub struct Video {
    ctx: Arc<HostCtx>,
    conn: quinn::Connection,
    pub encoder: EncoderInfo,
    /// For sharp-text mode: a 4:4:4 encoder the client can decode.
    encoder444: Option<EncoderInfo>,
    /// Streams run in 4:4:4 (sharp text with `encoder444`).
    chroma444: AtomicBool,
    max_datagram: Option<usize>,
    ctl: qr_net::files::Control,
    streams: Mutex<BTreeMap<u8, Current>>,
    /// Sharp-text mode, for every stream.
    sharp: AtomicBool,
    /// Next frame id per stream: a restarted stream carries on numbering,
    /// so the client's reassembler needs no reset.
    next_frame: Mutex<BTreeMap<u8, Arc<AtomicU32>>>,
    /// One (re)start at a time; and the latest resize request per stream
    /// (debounced).
    restart: tokio::sync::Mutex<()>,
    resize_gen: AtomicU32,
    /// The client asked for no video (it is hidden).
    paused: AtomicBool,
    /// The client's quality level (`qr_proto::msg::quality`), for every
    /// stream and every restart.
    quality: std::sync::atomic::AtomicU8,
    /// Desktop scale per stream, percent (100 = none), for input mapping.
    scale_pct: Mutex<BTreeMap<u8, u16>>,
    /// A stream about to read a node whose monitor isn't laid out yet (a
    /// GNOME virtual monitor appears once its stream is read).
    pending: Mutex<BTreeMap<u8, u32>>,
}

impl Video {
    pub fn new(
        ctx: Arc<HostCtx>,
        conn: quinn::Connection,
        encoder: EncoderInfo,
        encoder444: Option<EncoderInfo>,
        max_datagram: Option<usize>,
        ctl: qr_net::files::Control,
    ) -> Self {
        Self {
            ctx,
            conn,
            encoder,
            encoder444,
            chroma444: AtomicBool::new(false),
            max_datagram,
            ctl,
            streams: Default::default(),
            sharp: Default::default(),
            next_frame: Default::default(),
            restart: Default::default(),
            resize_gen: Default::default(),
            paused: Default::default(),
            quality: Default::default(),
            scale_pct: Default::default(),
            pending: Default::default(),
        }
    }

    /// Starts stream `id` from PipeWire `node` at `mode`'s size, for a
    /// monitor that only appears once read; [`Video::attach`] names it.
    pub async fn start_pending(&self, id: u8, node: u32, mode: (u32, u32, u32)) -> Result<()> {
        self.pending.lock().expect("lock").insert(id, node);
        self.start(id, crate::mutter::PENDING_MONITOR, mode).await
    }

    /// Stream `id` shows `monitor` (laid out now): input maps onto it.
    pub fn attach(&self, id: u8, monitor: u8) {
        if let Some(c) = self.streams.lock().expect("lock").get_mut(&id) {
            c.monitor = monitor;
        }
        self.set_scale_pct(id, self.scale_pct(id));
    }

    /// Stops (true) or resumes (false) all video; resuming sends keyframes.
    /// The client's quality level, applied to every stream now and to
    /// streams started later.
    pub fn set_quality(&self, level: u8) {
        self.quality.store(level, Ordering::Relaxed);
        for s in self.all() {
            s.set_quality(level);
        }
    }

    pub fn pause(&self, on: bool) {
        self.paused.store(on, Ordering::Relaxed);
        for s in self.all() {
            s.encoder.pause(on);
            if !on {
                s.encoder.force_keyframe();
            }
        }
    }

    /// Stream `id`'s desktop scale, percent.
    pub fn scale_pct(&self, id: u8) -> u16 {
        self.scale_pct.lock().expect("lock").get(&id).copied().unwrap_or(100)
    }

    pub fn set_scale_pct(&self, id: u8, pct: u16) {
        self.scale_pct.lock().expect("lock").insert(id, pct);
        // Input mapping follows at once (the stream may not restart).
        if let Some((m, _)) = self.current(id)
            && let Some(mon) = self.ctx.monitors().into_iter().find(|x| x.id == m)
        {
            self.ctx.input.set_origin(id, (mon.x, mon.y), pct as f64 / 100.0);
        }
    }

    /// Stream `id`, if running.
    pub fn stream(&self, id: u8) -> Option<Arc<VideoStream>> {
        self.streams.lock().expect("lock").get(&id).map(|c| c.stream.clone())
    }

    /// Every running stream.
    pub fn all(&self) -> Vec<Arc<VideoStream>> {
        self.streams.lock().expect("lock").values().map(|c| c.stream.clone()).collect()
    }

    /// Sharp-text mode: a still-screen refresh at high quality, and 4:4:4
    /// chroma when both sides have a 4:4:4 codec (the streams restart).
    pub async fn set_sharp(&self, on: bool) {
        self.sharp.store(on, Ordering::Relaxed);
        for s in self.all() {
            s.sharp.store(on, Ordering::Relaxed);
        }
        let want444 = on && self.encoder444.is_some();
        if self.chroma444.swap(want444, Ordering::Relaxed) == want444 {
            return;
        }
        let running: Vec<(u8, u8, (u32, u32, u32))> =
            self.streams.lock().expect("lock").iter().map(|(id, c)| (*id, c.monitor, c.mode)).collect();
        for (id, monitor, mode) in running {
            if let Err(e) = self.start(id, monitor, mode).await {
                tracing::warn!("restarting stream {id} for 4:4:4={want444}: {e:#}");
            }
        }
    }

    /// (Re)starts stream `id` on `monitor` at `mode` (virtual displays
    /// change their mode first), and announces it.
    pub async fn start(&self, id: u8, monitor: u8, mode: (u32, u32, u32)) -> Result<()> {
        if id >= MAX_STREAMS {
            bail!("at most {MAX_STREAMS} streams");
        }
        let _one = self.restart.lock().await;
        let mons = self.ctx.monitors();
        let pending = (monitor == crate::mutter::PENDING_MONITOR)
            .then(|| self.pending.lock().expect("lock").remove(&id))
            .flatten();
        let mon = match pending {
            Some(_) => Monitor {
                id: monitor,
                name: "pending".into(),
                width: mode.0 as u16,
                height: mode.1 as u16,
                refresh_mhz: mode.2 * 1000,
                x: 0,
                y: 0,
                primary: false,
            },
            None => mons.iter().find(|m| m.id == monitor).or(mons.first()).cloned().context("no monitor")?,
        };
        let (mut w, mut h, fps) = mode;
        let mut opened = None;
        let source = if let Some(socket) = self.ctx.screencopy_socket() {
            // Stop the old capture first: the mode change would only
            // confuse it.
            drop(self.streams.lock().expect("lock").remove(&id));
            // Only the virtual display changes mode for the client.
            if let Some(vd) = &self.ctx.vd
                && (w, h) != (mon.width as u32, mon.height as u32)
                && w > 0
                && h > 0
            {
                vd.lock().expect("lock").set_mode(&mon.name, w, h, fps)?;
            }
            let name = mon.name.clone();
            let gpu = !self.chroma444.load(Ordering::Relaxed);
            // The pointer stays out of the picture when its shapes are
            // forwarded (the managed desktop); otherwise it is drawn in.
            let overlay = self.ctx.cursor.is_none();
            let o = tokio::task::spawn_blocking(move || crate::screencopy::open(&socket, &name, gpu, overlay))
                .await?
                .context("capturing the Wayland output")?;
            (w, h) = (o.width, o.height);
            let s = match o.format {
                crate::screencopy::Format::Raw(f) => Source::App { format: f.into() },
                crate::screencopy::Format::DmaNv12(modifier) => Source::DmaNv12 { modifier },
            };
            opened = Some(o);
            s
        } else if let Some(node) = pending {
            // A monitor that appears when read: ask for its size.
            drop(self.streams.lock().expect("lock").remove(&id));
            Source::PipeWire { fd: None, node, size: Some((w, h)) }
        } else if let Some(m) = &self.ctx.mutter {
            // GNOME: the monitor's stream; recorded anew (at its current
            // layout) after a capture of it ended.
            drop(self.streams.lock().expect("lock").remove(&id));
            let (now, node) = m.stream(mon.id).await?;
            // The layout is in logical units; the stream has the pixels.
            let k = self.scale_pct(id) as f64 / 100.0;
            (w, h) = (((now.width as f64 * k).round() as u32) & !1, ((now.height as f64 * k).round() as u32) & !1);
            // A virtual monitor keeps the size its reader asks for.
            let size = m.is_virtual(mon.id).then_some((w, h));
            Source::PipeWire { fd: None, node, size }
        } else if self.ctx.kwin.is_some() {
            // KDE: a fresh screencast at the monitor's current size (a
            // running PipeWire stream can't change size under pipewiresrc).
            drop(self.streams.lock().expect("lock").remove(&id));
            // The layout is in logical units; the stream has the pixels.
            let k = self.scale_pct(id) as f64 / 100.0;
            (w, h) = (((mon.width as f64 * k).round() as u32) & !1, ((mon.height as f64 * k).round() as u32) & !1);
            let (ctx, m) = (self.ctx.clone(), mon.clone());
            let node = tokio::task::spawn_blocking(move || {
                let kwin = ctx.kwin.as_ref().expect("kwin");
                // A virtual output of ours streams through its own stream.
                kwin.virtual_node(&m.name).unwrap_or_else(|| kwin.stream(&m))
            })
            .await??;
            Source::PipeWire { fd: None, node, size: None }
        } else {
            (w, h) = (mon.width as u32, mon.height as u32);
            self.ctx.source_for(&mon)
        };
        let limits = Limits { width: w, height: h, max_fps: fps, min_kbps: 300, max_kbps: self.ctx.args.max_kbps };
        let mut rate = RateCtl::new(limits, Instant::now());
        rate.grace(Instant::now());
        rate.set_quality(self.quality.load(Ordering::Relaxed));
        if self.ctx.args.bitrate > 0 {
            rate.target_kbps = self.ctx.args.bitrate;
        }
        let chroma444 = self.chroma444.load(Ordering::Relaxed);
        let encoder = match (&self.encoder444, chroma444) {
            (Some(e), true) => e.clone(),
            _ => self.encoder.clone(),
        };
        // A 4:4:4 encoder takes system memory: a GPU-converted (NV12)
        // capture can't feed it.
        let source = match source {
            Source::DmaNv12 { .. } if chroma444 => bail!("4:4:4 needs the shared-memory capture"),
            s => s,
        };
        let cfg = EncodeConfig {
            encoder: encoder.clone(),
            source,
            width: w,
            height: h,
            fps,
            bitrate_kbps: rate.target_kbps,
            chroma444,
        };
        drop(self.streams.lock().expect("lock").remove(&id));
        let next = self.next_frame.lock().expect("lock").entry(id).or_default().clone();
        let stream = VideoStream::start(id, &cfg, rate, self.conn.clone(), self.max_datagram, next)?;
        stream.sharp.store(self.sharp.load(Ordering::Relaxed), Ordering::Relaxed);
        let capture = opened.map(|o| {
            let enc = stream.encoder.clone();
            o.run(Duration::from_millis(500), move |frame| {
                let r = match frame {
                    crate::screencopy::Frame::Packed(p) => enc.push(p),
                    #[cfg(target_os = "linux")]
                    crate::screencopy::Frame::Dma(d) => enc.push_dma(d),
                    #[cfg(not(target_os = "linux"))]
                    crate::screencopy::Frame::Dma(d) => match *d {},
                };
                if let Err(e) = r {
                    tracing::debug!("frame not taken: {e:#}");
                }
            })
        });
        self.ctx.input.set_origin(id, (mon.x, mon.y), self.scale_pct(id) as f64 / 100.0);
        stream.encoder.pause(self.paused.load(Ordering::Relaxed));
        self.streams
            .lock()
            .expect("lock")
            .insert(id, Current { stream, monitor: mon.id, mode: (w, h, fps), _capture: capture });
        tracing::info!(stream = id, monitor = %mon.name, w, h, fps, encoder = encoder.element, chroma444, "stream started");
        (self.ctl)(Msg::StreamStarted {
            stream: id,
            monitor: mon.id,
            codec: encoder.codec as u8,
            chroma444,
            width: w as u16,
            height: h as u16,
            fps: fps as u16,
        });
        Ok(())
    }

    /// Stops stream `id` (the main stream keeps running).
    pub async fn stop(&self, id: u8) {
        if id == 0 {
            return;
        }
        let _one = self.restart.lock().await;
        if self.streams.lock().expect("lock").remove(&id).is_some() {
            tracing::info!(stream = id, "stream stopped");
            (self.ctl)(Msg::StreamStop { stream: id });
        }
    }

    /// Resizes stream `id`'s monitor once the requests stop for 300 ms (a
    /// window being dragged sends many).
    pub fn request_resize(self: &Arc<Self>, id: u8, mode: (u32, u32, u32)) {
        let generation = self.resize_gen.fetch_add(1, Ordering::Relaxed) + 1;
        let me = self.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(300)).await;
            let Some((monitor, current)) = me.current(id) else { return };
            if me.resize_gen.load(Ordering::Relaxed) != generation || mode == current {
                return;
            }
            if let Err(e) = me.start(id, monitor, mode).await {
                tracing::warn!("resize failed: {e:#}");
            }
        });
    }

    /// Stream `id`'s monitor and mode.
    pub fn current(&self, id: u8) -> Option<(u8, (u32, u32, u32))> {
        self.streams.lock().expect("lock").get(&id).map(|c| (c.monitor, c.mode))
    }
}
