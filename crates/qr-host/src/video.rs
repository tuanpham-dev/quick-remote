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
use qr_proto::msg::Profile;
use qr_proto::Msg;

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
    pub profile: Profile,
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
}

impl Video {
    pub fn new(
        ctx: Arc<HostCtx>,
        conn: quinn::Connection,
        encoder: EncoderInfo,
        profile: Profile,
        max_datagram: Option<usize>,
        ctl: qr_net::files::Control,
    ) -> Self {
        Self {
            ctx,
            conn,
            encoder,
            profile,
            max_datagram,
            ctl,
            streams: Default::default(),
            sharp: Default::default(),
            next_frame: Default::default(),
            restart: Default::default(),
            resize_gen: Default::default(),
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

    pub fn set_sharp(&self, on: bool) {
        self.sharp.store(on, Ordering::Relaxed);
        for s in self.all() {
            s.sharp.store(on, Ordering::Relaxed);
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
        let mon = mons.iter().find(|m| m.id == monitor).or(mons.first()).cloned().context("no monitor")?;
        let (mut w, mut h, fps) = mode;
        let mut opened = None;
        let source = if let Some(vd) = &self.ctx.vd {
            // Stop the old capture first: the mode change would only
            // confuse it.
            drop(self.streams.lock().expect("lock").remove(&id));
            let socket = {
                let vd = vd.lock().expect("lock");
                if (w, h) != (mon.width as u32, mon.height as u32) && w > 0 && h > 0 {
                    vd.set_mode(&mon.name, w, h, fps)?;
                }
                vd.wayland_socket()
            };
            let name = mon.name.clone();
            let o = tokio::task::spawn_blocking(move || crate::screencopy::open(&socket, &name))
                .await?
                .context("capturing the virtual display")?;
            (w, h) = (o.width, o.height);
            let s = match o.format {
                crate::screencopy::Format::Raw(f) => Source::App { format: f.into() },
                crate::screencopy::Format::DmaNv12(modifier) => Source::DmaNv12 { modifier },
            };
            opened = Some(o);
            s
        } else {
            (w, h) = (mon.width as u32, mon.height as u32);
            self.ctx.source_for(&mon)
        };
        let limits = Limits { width: w, height: h, max_fps: fps, min_kbps: 300, max_kbps: self.ctx.args.max_kbps };
        let mut rate = RateCtl::new(self.profile, limits, Instant::now());
        if self.ctx.args.bitrate > 0 {
            rate.target_kbps = self.ctx.args.bitrate;
        }
        let cfg = EncodeConfig {
            encoder: self.encoder.clone(),
            source,
            width: w,
            height: h,
            fps,
            bitrate_kbps: rate.target_kbps,
            chroma444: false,
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
                    crate::screencopy::Frame::Dma(d) => enc.push_dma(d),
                };
                if let Err(e) = r {
                    tracing::debug!("frame not taken: {e:#}");
                }
            })
        });
        self.ctx.input.set_origin(id, (mon.x, mon.y));
        self.streams
            .lock()
            .expect("lock")
            .insert(id, Current { stream, monitor: mon.id, mode: (w, h, fps), _capture: capture });
        tracing::info!(stream = id, monitor = %mon.name, w, h, fps, "stream started");
        (self.ctl)(Msg::StreamStarted {
            stream: id,
            monitor: mon.id,
            codec: self.encoder.codec as u8,
            chroma444: false,
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
