//! Opus audio: capture → 5 ms Opus packets (host audio, client microphone),
//! and packets → playback (client speakers, host virtual microphone).

use anyhow::{Context, Result, anyhow};
use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_app as gst_app;

use crate::{ClockMap, init};

#[derive(Debug, Clone)]
pub enum AudioSource {
    /// `audiotestsrc` with the given wave (`sine`, `ticks`, …).
    Test { wave: String },
    /// A PipeWire node or sink monitor (`target-object`).
    PipeWire { target: String },
    /// The default PulseAudio/PipeWire-Pulse input (or `device`).
    Pulse { device: Option<String> },
    /// The system's default input (`autoaudiosrc`).
    Default,
    /// A WAV file played in real time (tests).
    File { path: String },
    /// Windows loopback capture of the default output.
    WasapiLoopback,
}

#[derive(Debug, Clone)]
pub enum AudioSink {
    Default,
    PipeWire {
        target: String,
    },
    /// Writes a WAV file (tests).
    File {
        path: String,
    },
}

pub const RATE: u32 = 48_000;

fn source_desc(s: &AudioSource) -> String {
    match s {
        AudioSource::Test { wave } => format!("audiotestsrc is-live=true wave={wave} volume=0.5"),
        AudioSource::PipeWire { target } => format!("pipewiresrc target-object={target} do-timestamp=true"),
        AudioSource::Pulse { device } => {
            let d = device.as_ref().map(|d| format!(" device={d}")).unwrap_or_default();
            format!("pulsesrc{d} buffer-time=10000 latency-time=5000 do-timestamp=true")
        }
        AudioSource::Default => "autoaudiosrc".into(),
        AudioSource::File { path } => format!("filesrc location={path} ! wavparse ! identity sync=true"),
        AudioSource::WasapiLoopback => "wasapi2src loopback=true low-latency=true".into(),
    }
}

/// Encodes captured audio into Opus packets.
pub struct AudioCapture {
    pipeline: gst::Pipeline,
}

impl AudioCapture {
    /// `on_packet(opus, capture_us)` runs on a streaming thread.
    pub fn start(
        source: &AudioSource,
        channels: u32,
        bitrate: u32,
        clock: ClockMap,
        on_packet: impl Fn(Vec<u8>, u64) + Send + Sync + 'static,
    ) -> Result<Self> {
        init()?;
        let desc = format!(
            "{src} ! audioconvert ! audioresample ! audio/x-raw,rate={RATE},channels={channels} \
             ! opusenc frame-size=5 bitrate={bitrate} audio-type=restricted-lowdelay \
             ! appsink name=out sync=false emit-signals=true max-buffers=50",
            src = source_desc(source),
        );
        let pipeline = gst::parse::launch(&desc)
            .with_context(|| format!("building {desc}"))?
            .downcast::<gst::Pipeline>()
            .map_err(|_| anyhow!("not a pipeline"))?;
        let sink = pipeline.by_name("out").expect("named").downcast::<gst_app::AppSink>().expect("appsink");
        let pl = pipeline.downgrade();
        sink.set_callbacks(
            gst_app::AppSinkCallbacks::builder()
                .new_sample(move |s| {
                    let sample = s.pull_sample().map_err(|_| gst::FlowError::Eos)?;
                    let buf = sample.buffer().ok_or(gst::FlowError::Error)?;
                    let base = pl.upgrade().and_then(|p| p.base_time()).unwrap_or(gst::ClockTime::ZERO);
                    let ts = buf
                        .pts()
                        .map(|p| clock.to_us(base + p))
                        .unwrap_or_else(|| clock.to_us(gst::SystemClock::obtain().time()));
                    let map = buf.map_readable().map_err(|_| gst::FlowError::Error)?;
                    on_packet(map.as_slice().to_vec(), ts);
                    Ok(gst::FlowSuccess::Ok)
                })
                .build(),
        );
        pipeline.set_state(gst::State::Playing).context("starting audio capture")?;
        Ok(Self { pipeline })
    }

    pub fn error(&self) -> Option<String> {
        bus_error(&self.pipeline)
    }
}

impl Drop for AudioCapture {
    fn drop(&mut self) {
        let _ = self.pipeline.set_state(gst::State::Null);
    }
}

/// Decodes Opus packets and plays them as they arrive. A gap in sequence
/// numbers is passed to the decoder as a discontinuity, so Opus conceals
/// the lost 5 ms instead of the playout clock slipping.
pub struct AudioPlayer {
    pipeline: gst::Pipeline,
    src: gst_app::AppSrc,
    /// (first sequence number, next expected).
    seq: std::sync::Mutex<Option<(u32, u32)>>,
}

/// Playout buffer of the audio sink, µs: the jitter allowance.
pub const PLAYOUT_US: u64 = 20_000;

impl AudioPlayer {
    pub fn start(sink: &AudioSink, channels: u32) -> Result<Self> {
        init()?;
        let out = match sink {
            AudioSink::Default => format!("autoaudiosink sync=false buffer-time={PLAYOUT_US} latency-time=5000"),
            AudioSink::PipeWire { target } => format!("pipewiresink target-object={target} sync=false"),
            AudioSink::File { path } => format!("wavenc ! filesink location={path}"),
        };
        let desc = format!(
            "appsrc name=src is-live=true format=time do-timestamp=false \
             ! opusdec plc=true ! audioconvert ! audioresample \
             ! audio/x-raw,rate={RATE},channels={channels} ! {out}"
        );
        let pipeline = gst::parse::launch(&desc)
            .with_context(|| format!("building {desc}"))?
            .downcast::<gst::Pipeline>()
            .map_err(|_| anyhow!("not a pipeline"))?;
        let src = pipeline.by_name("src").expect("named").downcast::<gst_app::AppSrc>().expect("appsrc");
        src.set_caps(Some(
            &gst::Caps::builder("audio/x-opus")
                .field("rate", RATE as i32)
                .field("channels", channels as i32)
                .field("channel-mapping-family", 0i32)
                .build(),
        ));
        pipeline.set_state(gst::State::Playing).context("starting audio playback")?;
        Ok(Self { pipeline, src, seq: std::sync::Mutex::new(None) })
    }

    /// Packets are stamped by sequence number (5 ms each), so a lost packet
    /// shows as a timestamp gap and Opus conceals it.
    pub fn push(&self, seq: u32, opus: &[u8]) -> Result<()> {
        let mut st = self.seq.lock().expect("lock");
        let (first, next) = *st.get_or_insert((seq, seq));
        if seq.wrapping_sub(next) > u32::MAX / 2 {
            return Ok(()); // late duplicate or reordered: drop
        }
        let gap = seq != next;
        *st = Some((first, seq.wrapping_add(1)));
        if gap {
            // Tell the decoder about the missing span; with plc=true Opus
            // fills it with concealment instead of silence-free skipping.
            let at = gst::ClockTime::from_mseconds(5 * next.wrapping_sub(first) as u64);
            let dur = gst::ClockTime::from_mseconds(5 * seq.wrapping_sub(next) as u64);
            self.src.send_event(gst::event::Gap::builder(at).duration(dur).build());
        }
        let mut buf = gst::Buffer::from_slice(opus.to_vec());
        {
            let b = buf.get_mut().expect("unique");
            b.set_pts(gst::ClockTime::from_mseconds(5 * seq.wrapping_sub(first) as u64));
            b.set_duration(gst::ClockTime::from_mseconds(5));
            if gap {
                b.set_flags(gst::BufferFlags::DISCONT);
            }
        }
        self.src.push_buffer(buf).map_err(|e| anyhow!("audio playback refused a packet: {e:?}"))?;
        Ok(())
    }

    /// Ends playback cleanly (finishes a WAV file).
    pub fn finish(&self) {
        let _ = self.src.end_of_stream();
        if let Some(bus) = self.pipeline.bus() {
            let _ = bus
                .timed_pop_filtered(gst::ClockTime::from_seconds(2), &[gst::MessageType::Eos, gst::MessageType::Error]);
        }
    }

    pub fn error(&self) -> Option<String> {
        bus_error(&self.pipeline)
    }
}

impl Drop for AudioPlayer {
    fn drop(&mut self) {
        let _ = self.pipeline.set_state(gst::State::Null);
    }
}

fn bus_error(p: &gst::Pipeline) -> Option<String> {
    let bus = p.bus()?;
    while let Some(msg) = bus.pop_filtered(&[gst::MessageType::Error]) {
        if let gst::MessageView::Error(e) = msg.view() {
            return Some(format!("{} ({:?})", e.error(), e.debug()));
        }
    }
    None
}
