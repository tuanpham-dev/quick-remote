//! Client decode pipeline: appsrc (encoded access units) → parser →
//! decoder → raw frames in system memory for the renderer.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result, anyhow};
use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_app as gst_app;
use gstreamer_video as gst_video;
use qr_proto::Codec;

use crate::{encoded_caps, init, parser, set_prop};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PixelFormat {
    Nv12,
    I420,
    Y444,
}

/// One decoded picture, planes tightly described by `strides`/`offsets`.
#[derive(Debug, Clone)]
pub struct DecodedFrame {
    pub frame_id: u32,
    pub width: u32,
    pub height: u32,
    pub format: PixelFormat,
    pub data: Vec<u8>,
    pub offsets: [usize; 3],
    pub strides: [usize; 3],
    /// Time from push to picture, µs (latency, including any frame the
    /// decoder holds back until the next one arrives).
    pub decode_us: u64,
    /// Decoding work, µs: like `decode_us`, but a picture held until the
    /// next push counts from that push. A decoder that falls behind still
    /// shows its whole backlog here.
    pub work_us: u64,
}

pub struct Decoder {
    pipeline: gst::Pipeline,
    src: gst_app::AppSrc,
    pushed: Arc<Mutex<VecDeque<(u32, std::time::Instant)>>>,
    pub element: String,
    codec: Codec,
}

impl Decoder {
    /// Starts a decoder; `on_frame` runs on a streaming thread.
    pub fn start(codec: Codec, element: &str, on_frame: impl Fn(DecodedFrame) + Send + Sync + 'static) -> Result<Self> {
        init()?;
        let desc = format!(
            "appsrc name=src is-live=true format=time do-timestamp=false max-bytes=0 \
             ! {parse} ! {element} name=dec ! videoconvert n-threads=2 \
             ! video/x-raw,format={{NV12,I420,Y444}} \
             ! appsink name=out sync=false max-buffers=4 emit-signals=true",
            parse = parser(codec),
        );
        let pipeline = gst::parse::launch(&desc)
            .with_context(|| format!("building {desc}"))?
            .downcast::<gst::Pipeline>()
            .map_err(|_| anyhow!("not a pipeline"))?;
        let dec = pipeline.by_name("dec").expect("named");
        // Output each picture as soon as it is decoded: our streams have no
        // B-frames, but an SPS without reorder hints makes a strict decoder
        // hold frames for possible reordering (one frame or more of delay).
        set_prop(&dec, "compliance", "flexible");
        // libav: frame threading adds a frame of delay per thread.
        set_prop(&dec, "thread-type", "slice");
        let src = pipeline.by_name("src").expect("named").downcast::<gst_app::AppSrc>().expect("appsrc");
        src.set_caps(Some(&encoded_caps(codec)));
        let sink = pipeline.by_name("out").expect("named").downcast::<gst_app::AppSink>().expect("appsink");
        let pushed: Arc<Mutex<VecDeque<(u32, std::time::Instant)>>> = Arc::default();
        let p2 = pushed.clone();
        sink.set_callbacks(
            gst_app::AppSinkCallbacks::builder()
                .new_sample(move |s| {
                    let sample = s.pull_sample().map_err(|_| gst::FlowError::Eos)?;
                    let caps = sample.caps().ok_or(gst::FlowError::NotNegotiated)?;
                    let info = gst_video::VideoInfo::from_caps(caps).map_err(|_| gst::FlowError::NotNegotiated)?;
                    let buf = sample.buffer().ok_or(gst::FlowError::Error)?;
                    let (frame_id, decode_us, work_us) = {
                        let mut q = p2.lock().expect("lock");
                        match q.pop_front() {
                            Some((id, t)) => {
                                let d = t.elapsed().as_micros() as u64;
                                // One frame queued behind this one: a hold,
                                // released by that push. More: a backlog.
                                let work = match (q.len(), q.back()) {
                                    (1, Some((_, last))) => last.elapsed().as_micros() as u64,
                                    _ => d,
                                };
                                (id, d, work)
                            }
                            None => (0, 0, 0),
                        }
                    };
                    let format = match info.format() {
                        gst_video::VideoFormat::Nv12 => PixelFormat::Nv12,
                        gst_video::VideoFormat::I420 => PixelFormat::I420,
                        gst_video::VideoFormat::Y444 => PixelFormat::Y444,
                        _ => return Err(gst::FlowError::NotNegotiated),
                    };
                    let map = buf.map_readable().map_err(|_| gst::FlowError::Error)?;
                    let mut offsets = [0usize; 3];
                    let mut strides = [0usize; 3];
                    for i in 0..info.n_planes() as usize {
                        offsets[i] = info.offset()[i];
                        strides[i] = info.stride()[i] as usize;
                    }
                    on_frame(DecodedFrame {
                        frame_id,
                        width: info.width(),
                        height: info.height(),
                        format,
                        data: map.as_slice().to_vec(),
                        offsets,
                        strides,
                        decode_us,
                        work_us,
                    });
                    Ok(gst::FlowSuccess::Ok)
                })
                .build(),
        );
        pipeline.set_state(gst::State::Playing).context("starting the decode pipeline")?;

        Ok(Self { pipeline, src, pushed, element: element.to_string(), codec })
    }

    /// Feeds one access unit. Frames come out in order (no B-frames), so the
    /// frame id is matched by order.
    pub fn push(&self, frame_id: u32, au: &[u8], keyframe: bool) -> Result<()> {
        // Keyframes carry the SPS: make sure it leaves the decoder a free
        // DPB slot (see `h264`), else every picture waits for the next.
        // openh264dec also needs a Baseline label to output at once.
        let baseline = self.element == "openh264dec";
        let fixed = (keyframe && self.codec == Codec::H264).then(|| crate::h264::fix_au(au, baseline)).flatten();
        let mut buf = gst::Buffer::from_slice(fixed.unwrap_or_else(|| au.to_vec()));
        {
            let b = buf.get_mut().expect("unique");
            b.set_pts(gst::ClockTime::from_useconds(frame_id as u64 * 1000));
            if !keyframe {
                b.set_flags(gst::BufferFlags::DELTA_UNIT);
            }
        }
        {
            let mut q = self.pushed.lock().expect("lock");
            if q.len() >= 16 {
                q.pop_front();
            }
            q.push_back((frame_id, std::time::Instant::now()));
        }
        self.src.push_buffer(buf).map_err(|e| anyhow!("decoder refused a frame: {e:?}"))?;
        Ok(())
    }

    /// Forgets frames in flight (after a loss, before the next keyframe).
    pub fn flush_pending(&self) {
        self.pushed.lock().expect("lock").clear();
    }

    pub fn error(&self) -> Option<String> {
        let bus = self.pipeline.bus()?;
        while let Some(msg) = bus.pop_filtered(&[gst::MessageType::Error]) {
            if let gst::MessageView::Error(e) = msg.view() {
                return Some(format!("{} ({:?})", e.error(), e.debug()));
            }
        }
        None
    }
}

impl Drop for Decoder {
    fn drop(&mut self) {
        let _ = self.pipeline.set_state(gst::State::Null);
    }
}
