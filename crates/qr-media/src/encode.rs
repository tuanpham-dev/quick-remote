//! Low-latency encode pipeline: capture source → encoder → parser →
//! appsink, one encoded access unit per captured frame.
//!
//! Latency rules: no B-frames, no lookahead, a one-frame rate-control
//! buffer, an effectively infinite GOP with keyframes on demand, a leaky
//! one-frame queue in front of the encoder (a slow encoder drops captures,
//! never queues them), and frames skipped *before* encoding while the
//! network is backlogged.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result, anyhow};
use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_app as gst_app;
use qr_proto::Codec;

use crate::probe::{EncoderInfo, Family};
use crate::{ClockMap, encoded_caps, init, parser, set_prop};

#[derive(Debug, Clone)]
pub enum Source {
    /// `videotestsrc` (tests and `--source test`).
    Test { pattern: String },
    /// A PipeWire video node, optionally on a portal-provided fd.
    PipeWire { fd: Option<i32>, node: u32 },
    /// Frames pushed by the caller through [`Encoder::push`] (screencopy).
    /// `format`: a GStreamer raw format such as `BGRx`.
    App { format: String },
    /// An X11 display (`ximagesrc`), optionally one monitor's rectangle.
    X11 { display: String, rect: Option<(i32, i32, u32, u32)> },
    /// Windows desktop capture (`d3d11screencapturesrc`).
    D3d11 { monitor: i32 },
}

#[derive(Debug, Clone)]
pub struct EncodeConfig {
    pub encoder: EncoderInfo,
    pub source: Source,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub bitrate_kbps: u32,
    pub chroma444: bool,
}

#[derive(Debug, Clone)]
pub struct EncodedFrame {
    pub au: Vec<u8>,
    pub keyframe: bool,
    pub codec: Codec,
    pub width: u32,
    pub height: u32,
    /// Monotonic µs (caller's clock, via [`ClockMap`]).
    pub capture_us: u64,
    pub encode_start_us: u64,
    pub encode_end_us: u64,
}

#[derive(Default)]
struct Control {
    /// Skip captures before the encoder (network backlog).
    hold: AtomicBool,
    /// Minimum spacing of encoded frames, µs (frame-rate cap).
    min_interval_us: AtomicU64,
    last_kept_pts_us: AtomicU64,
    skipped: AtomicU32,
    encoded: AtomicU32,
    /// (capture µs, encode start µs) of frames inside the encoder, in
    /// order. Some encoders (VA) rewrite pts, so frames are matched by
    /// order, which holds because there are no B-frames.
    inflight: Mutex<VecDeque<(u64, u64)>>,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct EncodeStats {
    pub encoded: u32,
    pub skipped: u32,
}

pub struct Encoder {
    pipeline: gst::Pipeline,
    enc: gst::Element,
    family: Family,
    ctl: Arc<Control>,
    pub codec: Codec,
    pub element: &'static str,
    /// Frame rate the encoder was configured for (the source rate).
    source_fps: u32,
    /// (target kbit/s, frame-rate cap) last requested.
    rate: std::sync::Mutex<(u32, u32)>,
    /// The `appsrc` of a [`Source::App`] pipeline.
    appsrc: Option<gst_app::AppSrc>,
}

/// The GStreamer description of a capture source (also used by tests to
/// render reference pictures).
pub fn source_desc(s: &Source, w: u32, h: u32, fps: u32) -> String {
    match s {
        // A still page of 9-pixel monospace text (sharp-text tests).
        Source::Test { pattern } if pattern == "text" => format!(
            "videotestsrc is-live=true pattern=white ! video/x-raw,width={w},height={h},framerate={fps}/1 \
             ! textoverlay text=\"{}\" font-desc=\"DejaVu Sans Mono 7\" halignment=left valignment=top \
               line-alignment=left auto-resize=false color=0xff000000 shaded-background=false \
               draw-shadow=false draw-outline=false ! videoconvert",
            // textoverlay takes Pango markup.
            crate::TEST_TEXT.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
        ),
        Source::Test { pattern } => {
            format!("videotestsrc is-live=true pattern={pattern} ! video/x-raw,width={w},height={h},framerate={fps}/1")
        }
        Source::PipeWire { fd, node } => {
            let fd = fd.map(|f| format!(" fd={f}")).unwrap_or_default();
            // System memory: the converter in front of the encoder can't
            // take DMA-BUF.
            format!("pipewiresrc{fd} path={node} always-copy=false do-timestamp=true keepalive-time=1000 ! video/x-raw")
        }
        // Variable rate (frames come on damage); timestamped on push.
        Source::App { format } => format!(
            "appsrc name=src is-live=true do-timestamp=true format=time max-buffers=2 leaky-type=downstream \
             caps=\"video/x-raw,format={format},width={w},height={h},framerate=0/1\""
        ),
        Source::X11 { display, rect } => {
            let r = rect
                .map(|(x, y, w, h)| {
                    format!(" startx={x} starty={y} endx={} endy={}", x + w as i32 - 1, y + h as i32 - 1)
                })
                .unwrap_or_default();
            format!(
                "ximagesrc display-name={display} use-damage=false show-pointer=true{r} ! video/x-raw,framerate={fps}/1"
            )
        }
        Source::D3d11 { monitor } => format!(
            "d3d11screencapturesrc monitor-index={monitor} show-cursor=true capture-api=wgc ! video/x-raw(memory:D3D11Memory),framerate={fps}/1"
        ),
    }
}

/// Encoder properties for low latency. Values are set through
/// `gst_util_set_object_arg`, which ignores unknown enum strings (with a
/// warning) instead of panicking, since versions differ across distros.
fn configure(e: &gst::Element, f: Family, cfg: &EncodeConfig) {
    let kbps = cfg.bitrate_kbps.max(100);
    let fps = cfg.fps.max(1);
    let frame_kbit = (kbps / fps).max(1);
    let set = |k: &str, v: String| {
        set_prop(e, k, &v);
    };
    match f {
        Family::X264 => {
            set("tune", "zerolatency".into());
            set("speed-preset", "superfast".into());
            set("bitrate", kbps.to_string());
            set("key-int-max", "100000".into());
            set("bframes", "0".into());
            set("rc-lookahead", "0".into());
            set("sliced-threads", "true".into());
            set("threads", "4".into());
            set("vbv-buf-capacity", (1000 / fps).max(1).to_string());
            set("aud", "false".into());
        }
        Family::X265 => {
            set("tune", "zerolatency".into());
            set("speed-preset", "superfast".into());
            set("bitrate", kbps.to_string());
            set("key-int-max", "100000".into());
            set(
                "option-string",
                format!("bframes=0:vbv-maxrate={kbps}:vbv-bufsize={frame_kbit}:repeat-headers=1:aud=1"),
            );
        }
        Family::OpenH264 => {
            set("rate-control", "bitrate".into());
            set("bitrate", (kbps * 1000).to_string());
            set("max-bitrate", (kbps * 1500).to_string());
            set("gop-size", "2147483647".into());
            set("usage-type", "screen".into());
            set("complexity", "low".into());
            set("enable-frame-skip", "false".into());
            set("multi-thread", "4".into());
        }
        Family::SvtAv1 => {
            set("preset", "12".into());
            set("target-bitrate", kbps.to_string());
            set("intra-period-length", "-1".into());
            set("parameters-string", "pred-struct=1:lookahead=0:scd=0".into());
        }
        Family::Va => {
            set("rate-control", "cbr".into());
            set("bitrate", kbps.to_string());
            set("cpb-size", frame_kbit.max(kbps / 30).to_string());
            set("key-int-max", "1024".into());
            set("b-frames", "0".into());
            set("ref-frames", "1".into());
            set("target-usage", "7".into());
        }
        Family::Nv => {
            set("preset", "p1".into());
            set("tune", "ultra-low-latency".into());
            set("rc-mode", "cbr".into());
            set("bitrate", kbps.to_string());
            set("vbv-buffer-size", frame_kbit.to_string());
            set("gop-size", "-1".into());
            set("bframes", "0".into());
            set("zerolatency", "true".into());
        }
        Family::Qsv => {
            set("target-usage", "7".into());
            set("low-latency", "true".into());
            set("rate-control", "cbr".into());
            set("bitrate", kbps.to_string());
            set("gop-size", "0".into());
            set("b-frames", "0".into());
        }
        Family::Amf => {
            set("usage", "ultra-low-latency".into());
            set("preset", "speed".into());
            set("rate-control", "cbr".into());
            set("bitrate", kbps.to_string());
            set("gop-size", "-1".into());
            set("b-frames", "0".into());
        }
        Family::MediaFoundation => {
            set("low-latency", "true".into());
            set("rc-mode", "cbr".into());
            set("bitrate", kbps.to_string());
            set("gop-size", "-1".into());
            set("bframes", "0".into());
        }
    }
}

fn set_bitrate_prop(e: &gst::Element, f: Family, kbps: u32) {
    let (k, v) = match f {
        Family::OpenH264 => ("bitrate", kbps * 1000),
        Family::SvtAv1 => ("target-bitrate", kbps),
        _ => ("bitrate", kbps),
    };
    set_prop(e, k, &v.to_string());
}

/// Raw format the encoder is fed.
fn raw_format(cfg: &EncodeConfig) -> &'static str {
    let sw = matches!(cfg.encoder.family, Family::X264 | Family::X265 | Family::OpenH264 | Family::SvtAv1);
    match (cfg.chroma444 && cfg.encoder.chroma444, sw) {
        (true, _) => "Y444",
        (false, true) => "I420",
        (false, false) => "NV12",
    }
}

impl Encoder {
    /// Builds and starts the pipeline. `on_frame` runs on a GStreamer
    /// streaming thread for every encoded frame; keep it short.
    pub fn start(
        cfg: &EncodeConfig,
        clock: ClockMap,
        on_frame: impl Fn(EncodedFrame) + Send + Sync + 'static,
    ) -> Result<Self> {
        init()?;
        let e = &cfg.encoder;
        let src = source_desc(&cfg.source, cfg.width, cfg.height, cfg.fps);
        let fmt = raw_format(cfg);
        // VA and D3D11 encoders take GPU memory directly; everything else
        // gets system memory through videoconvert.
        let convert = match (e.family, &cfg.source) {
            (Family::Va, Source::PipeWire { .. } | Source::App { .. }) => {
                if gst::ElementFactory::find("vapostproc").is_some() {
                    format!("vapostproc ! video/x-raw(memory:VAMemory),format={fmt}")
                } else {
                    // No VPP on this driver: convert on the CPU, threaded.
                    format!("videoconvert n-threads=4 ! video/x-raw,format={fmt}")
                }
            }
            (Family::Nv | Family::Qsv | Family::Amf | Family::MediaFoundation, Source::D3d11 { .. }) => {
                format!("d3d11convert ! video/x-raw(memory:D3D11Memory),format={fmt}")
            }
            _ => format!("videoconvert n-threads=4 ! video/x-raw,format={fmt}"),
        };
        let desc = format!(
            "{src} ! queue leaky=downstream max-size-buffers=1 max-size-bytes=0 max-size-time=0 \
             ! {convert} ! {el} name=enc ! {parse} ! {caps} \
             ! appsink name=out sync=false max-buffers=8 emit-signals=true",
            el = e.element,
            parse = parser(e.codec),
            caps = encoded_caps(e.codec),
        );
        tracing::info!(pipeline = %desc, "encode pipeline");
        let pipeline = gst::parse::launch(&desc)
            .with_context(|| format!("building {desc}"))?
            .downcast::<gst::Pipeline>()
            .map_err(|_| anyhow!("not a pipeline"))?;
        let enc = pipeline.by_name("enc").expect("named");
        // Repeat SPS/PPS (VPS) on every keyframe so a decoder can join at
        // any IDR.
        if let Some(p) = enc.static_pad("src").and_then(|p| p.peer()).and_then(|p| p.parent_element()) {
            set_prop(&p, "config-interval", "-1");
        }
        configure(&enc, e.family, cfg);
        // The VA encoders output asynchronously, two frames late, when
        // upstream answers the latency query as not live, which pipewiresrc
        // does. Every source here is live: say so.
        enc.static_pad("sink").expect("encoder sink pad").add_probe(
            gst::PadProbeType::QUERY_UPSTREAM | gst::PadProbeType::PULL,
            |_, info| {
                if let Some(gst::PadProbeData::Query(q)) = &mut info.data
                    && let gst::QueryViewMut::Latency(l) = q.view_mut()
                {
                    let (live, min, max) = l.result();
                    if !live {
                        l.set(true, min, max);
                    }
                }
                gst::PadProbeReturn::Ok
            },
        );
        let sink = pipeline.by_name("out").expect("named").downcast::<gst_app::AppSink>().expect("appsink");

        let ctl = Arc::new(Control::default());
        ctl.min_interval_us.store(0, Ordering::Relaxed);

        // Encoder input: skip while held or above the frame-rate cap;
        // remember capture and encode-start times by pts.
        let sinkpad = enc.static_pad("sink").expect("encoder sink pad");
        let (c, pl) = (ctl.clone(), pipeline.downgrade());
        sinkpad.add_probe(gst::PadProbeType::BUFFER, move |_, info| {
            let Some(buf) = info.buffer() else {
                return gst::PadProbeReturn::Ok;
            };
            let Some(pts) = buf.pts() else {
                return gst::PadProbeReturn::Ok;
            };
            let pts_us = pts.useconds();
            if c.hold.load(Ordering::Relaxed) {
                c.skipped.fetch_add(1, Ordering::Relaxed);
                return gst::PadProbeReturn::Drop;
            }
            let min = c.min_interval_us.load(Ordering::Relaxed);
            let last = c.last_kept_pts_us.load(Ordering::Relaxed);
            if min > 0 && last != 0 && pts_us.saturating_sub(last) < min * 9 / 10 {
                return gst::PadProbeReturn::Drop;
            }
            c.last_kept_pts_us.store(pts_us, Ordering::Relaxed);
            let base = pl.upgrade().and_then(|p| p.base_time()).unwrap_or(gst::ClockTime::ZERO);
            let capture = clock.to_us(base + pts);
            let now = clock.to_us(gst::SystemClock::obtain().time());
            let mut q = c.inflight.lock().expect("lock");
            // An encoder that swallowed frames would leave stale entries.
            if q.len() >= 16 {
                q.pop_front();
            }
            q.push_back((capture, now));
            gst::PadProbeReturn::Ok
        });

        let (c, codec) = (ctl.clone(), e.codec);
        sink.set_callbacks(
            gst_app::AppSinkCallbacks::builder()
                .new_sample(move |s| {
                    let sample = s.pull_sample().map_err(|_| gst::FlowError::Eos)?;
                    let end = clock.to_us(gst::SystemClock::obtain().time());
                    let Some(buf) = sample.buffer() else {
                        return Ok(gst::FlowSuccess::Ok);
                    };
                    let (w, h) = sample
                        .caps()
                        .and_then(|caps| caps.structure(0).map(|st| (st.get::<i32>("width"), st.get::<i32>("height"))))
                        .map(|(w, h)| (w.unwrap_or(0) as u32, h.unwrap_or(0) as u32))
                        .unwrap_or((0, 0));
                    let (capture, start) = {
                        let mut q = c.inflight.lock().expect("lock");
                        tracing::trace!(inflight = q.len(), "encoded");
                        q.pop_front().unwrap_or((end, end))
                    };
                    let map = buf.map_readable().map_err(|_| gst::FlowError::Error)?;
                    c.encoded.fetch_add(1, Ordering::Relaxed);
                    on_frame(EncodedFrame {
                        au: map.as_slice().to_vec(),
                        keyframe: !buf.flags().contains(gst::BufferFlags::DELTA_UNIT),
                        codec,
                        width: w,
                        height: h,
                        capture_us: capture,
                        encode_start_us: start,
                        encode_end_us: end,
                    });
                    Ok(gst::FlowSuccess::Ok)
                })
                .build(),
        );
        let appsrc = pipeline.by_name("src").and_then(|e| e.downcast::<gst_app::AppSrc>().ok());
        pipeline.set_state(gst::State::Playing).context("starting the encode pipeline")?;
        Ok(Self {
            pipeline,
            enc,
            family: e.family,
            ctl,
            codec: e.codec,
            element: e.element,
            source_fps: cfg.fps.max(1),
            rate: std::sync::Mutex::new((cfg.bitrate_kbps, cfg.fps.max(1))),
            appsrc,
        })
    }

    /// Feeds one tightly packed frame to a [`Source::App`] pipeline.
    pub fn push(&self, frame: Vec<u8>) -> Result<()> {
        let src = self.appsrc.as_ref().context("this encoder has its own source")?;
        src.push_buffer(gst::Buffer::from_mut_slice(frame)).map_err(|e| anyhow!("pushing a frame: {e:?}"))?;
        Ok(())
    }

    pub fn set_bitrate(&self, kbps: u32) {
        let mut r = self.rate.lock().expect("lock");
        r.0 = kbps;
        self.apply_rate(*r);
    }

    /// Caps the encoded frame rate (0 = no cap).
    pub fn set_max_fps(&self, fps: u32) {
        let us = if fps == 0 { 0 } else { 1_000_000 / fps as u64 };
        self.ctl.min_interval_us.store(us, Ordering::Relaxed);
        let mut r = self.rate.lock().expect("lock");
        r.1 = if fps == 0 { self.source_fps } else { fps.min(self.source_fps) };
        self.apply_rate(*r);
    }

    /// Encoders budget bits per frame from the configured (source) frame
    /// rate, and frames dropped before them don't change that: at a 5 fps
    /// cap on a 60 fps source each frame would get 1/12 of its share. So
    /// the encoder is told `target × source_fps / cap`, which makes its
    /// per-frame budget `target / cap`.
    fn apply_rate(&self, (kbps, cap): (u32, u32)) {
        let scaled = (kbps as u64 * self.source_fps as u64 / cap.max(1) as u64) as u32;
        set_bitrate_prop(&self.enc, self.family, scaled.max(100));
    }

    /// While held, captures are skipped before encoding: the reference chain
    /// stays intact, so no keyframe is needed when the hold lifts.
    pub fn hold(&self, on: bool) {
        self.ctl.hold.store(on, Ordering::Relaxed);
    }

    pub fn force_keyframe(&self) {
        let ev = gstreamer_video::UpstreamForceKeyUnitEvent::builder().all_headers(true).build();
        if let Some(pad) = self.enc.static_pad("src") {
            pad.send_event(ev);
        }
    }

    pub fn stats(&self) -> EncodeStats {
        EncodeStats {
            encoded: self.ctl.encoded.swap(0, Ordering::Relaxed),
            skipped: self.ctl.skipped.swap(0, Ordering::Relaxed),
        }
    }

    /// The first error on the pipeline bus, if any (non-blocking).
    pub fn error(&self) -> Option<String> {
        let bus = self.pipeline.bus()?;
        while let Some(msg) = bus.pop_filtered(&[gst::MessageType::Error, gst::MessageType::Eos]) {
            match msg.view() {
                gst::MessageView::Error(e) => {
                    return Some(format!("{} ({:?})", e.error(), e.debug()));
                }
                gst::MessageView::Eos(_) => return Some("capture ended".into()),
                _ => {}
            }
        }
        None
    }

    pub fn pipeline(&self) -> &gst::Pipeline {
        &self.pipeline
    }
}

impl Drop for Encoder {
    fn drop(&mut self) {
        let _ = self.pipeline.set_state(gst::State::Null);
    }
}
