//! Which encoders and decoders actually work on this machine.
//!
//! An element existing in the registry is not enough (a VA element may be
//! registered while the render node is unreadable), so each candidate runs
//! a tiny pipeline and must produce output.

use std::time::Duration;

use anyhow::Result;
use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_app as gst_app;
use qr_proto::{Codec, CodecCap};

use crate::{encoded_caps, init, parser};

/// How an encoder family is configured (see `encode::configure`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Family {
    Va,
    Nv,
    Qsv,
    Amf,
    MediaFoundation,
    X264,
    X265,
    OpenH264,
    SvtAv1,
}

#[derive(Debug, Clone)]
pub struct EncoderInfo {
    pub element: &'static str,
    pub codec: Codec,
    pub family: Family,
    pub hw: bool,
    pub chroma444: bool,
    /// The largest picture it takes (from its input caps; Intel's VA
    /// encoders stop at 4096×4096 on older GPUs).
    pub max_width: u32,
    pub max_height: u32,
}

/// The largest width and height an element's input caps allow.
fn caps_max(el: &str) -> (u32, u32) {
    let mut max = (0u32, 0u32);
    let Some(f) = gst::ElementFactory::find(el) else { return (16384, 16384) };
    for t in f.static_pad_templates() {
        if t.direction() != gst::PadDirection::Sink {
            continue;
        }
        for st in t.caps().iter() {
            let get = |k: &str| match st.value(k) {
                Ok(v) => v
                    .get::<gst::IntRange<i32>>()
                    .map(|r| r.max() as u32)
                    .or_else(|_| v.get::<i32>().map(|v| v as u32))
                    .ok(),
                Err(_) => None,
            };
            if let (Some(w), Some(h)) = (get("width"), get("height")) {
                max = (max.0.max(w), max.1.max(h));
            }
        }
    }
    if max.0 == 0 || max.1 == 0 { (16384, 16384) } else { (max.0.min(16384), max.1.min(16384)) }
}

/// Candidates, best first within each codec.
pub const ENCODERS: &[(&str, Codec, Family, bool)] = &[
    ("nvav1enc", Codec::Av1, Family::Nv, true),
    ("vaav1enc", Codec::Av1, Family::Va, true),
    ("vaav1lpenc", Codec::Av1, Family::Va, true),
    ("qsvav1enc", Codec::Av1, Family::Qsv, true),
    ("amfav1enc", Codec::Av1, Family::Amf, true),
    ("nvh265enc", Codec::H265, Family::Nv, true),
    ("vah265enc", Codec::H265, Family::Va, true),
    ("vah265lpenc", Codec::H265, Family::Va, true),
    ("qsvh265enc", Codec::H265, Family::Qsv, true),
    ("amfh265enc", Codec::H265, Family::Amf, true),
    ("mfh265enc", Codec::H265, Family::MediaFoundation, true),
    ("nvh264enc", Codec::H264, Family::Nv, true),
    ("vah264enc", Codec::H264, Family::Va, true),
    ("vah264lpenc", Codec::H264, Family::Va, true),
    ("qsvh264enc", Codec::H264, Family::Qsv, true),
    ("amfh264enc", Codec::H264, Family::Amf, true),
    ("mfh264enc", Codec::H264, Family::MediaFoundation, true),
    // Software, fastest first: at 1080p60 on a 6-core i5, x264 encodes in
    // ~4 ms, x265 and openh264 in ~10 ms, SVT-AV1 not in real time.
    ("x264enc", Codec::H264, Family::X264, false),
    ("x265enc", Codec::H265, Family::X265, false),
    // openh264's SPS declares a DPB of 1, so decoders hold each picture
    // for one frame (see `encode::configure`, x264); last resort only.
    ("openh264enc", Codec::H264, Family::OpenH264, false),
    ("svtav1enc", Codec::Av1, Family::SvtAv1, false),
];

/// Decoder candidates, best first within each codec.
pub const DECODERS: &[(&str, Codec, bool)] = &[
    ("nvav1dec", Codec::Av1, true),
    ("vaav1dec", Codec::Av1, true),
    ("d3d11av1dec", Codec::Av1, true),
    ("qsvav1dec", Codec::Av1, true),
    ("nvh265dec", Codec::H265, true),
    ("vah265dec", Codec::H265, true),
    ("d3d11h265dec", Codec::H265, true),
    ("qsvh265dec", Codec::H265, true),
    ("nvh264dec", Codec::H264, true),
    ("vah264dec", Codec::H264, true),
    ("d3d11h264dec", Codec::H264, true),
    ("qsvh264dec", Codec::H264, true),
    ("dav1ddec", Codec::Av1, false),
    ("av1dec", Codec::Av1, false),
    ("avdec_h265", Codec::H265, false),
    ("avdec_h264", Codec::H264, false),
    ("openh264dec", Codec::H264, false),
];

const PROBE_W: u32 = 256;
const PROBE_H: u32 = 144;

fn run(desc: &str, appsink_name: &str, timeout: Duration) -> Option<Vec<gst::Sample>> {
    let pipeline = gst::parse::launch(desc).ok()?.downcast::<gst::Pipeline>().ok()?;
    let sink = pipeline.by_name(appsink_name)?.downcast::<gst_app::AppSink>().ok()?;
    pipeline.set_state(gst::State::Playing).ok()?;
    let mut out = Vec::new();
    let deadline = std::time::Instant::now() + timeout;
    while std::time::Instant::now() < deadline {
        match sink.try_pull_sample(gst::ClockTime::from_mseconds(100)) {
            Some(s) => out.push(s),
            None if sink.is_eos() => break,
            None => {}
        }
        if let Some(msg) = pipeline.bus().and_then(|b| b.pop_filtered(&[gst::MessageType::Error])) {
            tracing::debug!(%desc, ?msg, "probe pipeline error");
            break;
        }
    }
    let _ = pipeline.set_state(gst::State::Null);
    (!out.is_empty()).then_some(out)
}

fn encode_desc(el: &str, codec: Codec, family: Family, format: &str) -> String {
    let caps = encoded_caps(codec).to_string();
    let props = match family {
        Family::X264 => " tune=zerolatency speed-preset=ultrafast",
        Family::X265 => " tune=zerolatency speed-preset=ultrafast",
        Family::SvtAv1 => " preset=12",
        _ => "",
    };
    format!(
        "videotestsrc num-buffers=4 ! video/x-raw,format={format},width={PROBE_W},height={PROBE_H},framerate=30/1 \
         ! videoconvert ! video/x-raw,format={format} ! {el}{props} ! {} ! {caps} ! appsink name=out sync=false",
        parser(codec)
    )
}

/// Encoders that open and produce output, best first.
pub fn encoders() -> Result<Vec<EncoderInfo>> {
    init()?;
    let mut out = Vec::new();
    for &(el, codec, family, hw) in ENCODERS {
        if gst::ElementFactory::find(el).is_none() {
            continue;
        }
        let fmt = if hw { "NV12" } else { "I420" };
        if run(&encode_desc(el, codec, family, fmt), "out", Duration::from_secs(5)).is_none() {
            tracing::debug!(el, "encoder registered but does not work here");
            continue;
        }
        let chroma444 = run(&encode_desc(el, codec, family, "Y444"), "out", Duration::from_secs(5))
            .and_then(|s| s.first().and_then(|s| s.caps().map(|c| c.to_string())))
            .is_some_and(|c| c.contains("4:4:4") || c.contains("444"));
        let (max_width, max_height) = caps_max(el);
        out.push(EncoderInfo { element: el, codec, family, hw, chroma444, max_width, max_height });
    }
    Ok(out)
}

#[derive(Debug, Clone)]
pub struct DecoderInfo {
    pub element: &'static str,
    pub codec: Codec,
    pub hw: bool,
    /// Decodes 4:4:4 streams (sharp-text mode).
    pub chroma444: bool,
}

/// Decoders that decode a stream from a working software encoder.
pub fn decoders() -> Result<Vec<DecoderInfo>> {
    init()?;
    let mut out = Vec::new();
    for &(el, codec, hw) in DECODERS {
        if gst::ElementFactory::find(el).is_none() {
            continue;
        }
        let Some(enc) =
            ENCODERS.iter().rev().find(|e| e.1 == codec && !e.3 && gst::ElementFactory::find(e.0).is_some())
        else {
            continue;
        };
        let desc = format!(
            "{} ! {el} ! videoconvert ! video/x-raw,format=NV12 ! appsink name=out sync=false",
            encode_desc(enc.0, codec, enc.2, "I420").replace(" ! appsink name=out sync=false", "")
        );
        if run(&desc, "out", Duration::from_secs(5)).is_some() {
            // 4:4:4: only x264/x265 encode it here; a decoder that can't
            // handle the profile errors out.
            let enc444 = match codec {
                Codec::H264 => Some(("x264enc", Family::X264)),
                Codec::H265 => Some(("x265enc", Family::X265)),
                Codec::Av1 => None,
            };
            let chroma444 = enc444.filter(|(e, _)| gst::ElementFactory::find(e).is_some()).is_some_and(|(e, f)| {
                let desc = format!(
                    "{} ! {el} ! videoconvert ! video/x-raw,format=Y444 ! appsink name=out sync=false",
                    encode_desc(e, codec, f, "Y444").replace(" ! appsink name=out sync=false", "")
                );
                run(&desc, "out", Duration::from_secs(5)).is_some()
            });
            out.push(DecoderInfo { element: el, codec, hw, chroma444 });
        } else {
            tracing::debug!(el, "decoder registered but does not work here");
        }
    }
    Ok(out)
}

/// What a client advertises in `Hello`: per codec, the best decoder.
pub fn decoder_caps(decs: &[DecoderInfo]) -> Vec<CodecCap> {
    let mut caps: Vec<CodecCap> = Vec::new();
    for d in decs {
        if caps.iter().any(|c| c.codec == d.codec) {
            continue;
        }
        let chroma444 = decs.iter().any(|x| x.codec == d.codec && x.chroma444);
        caps.push(CodecCap { codec: d.codec, hw: d.hw, max_width: 4096, max_height: 2304, chroma444 });
    }
    caps
}

/// The encoder for 4:4:4 (sharp text): one that encodes 4:4:4 in a codec
/// the client decodes in 4:4:4, hardware first, else in `encs` order.
pub fn choose_444(encs: &[EncoderInfo], client: &[CodecCap]) -> Option<EncoderInfo> {
    let ok = |e: &&EncoderInfo| e.chroma444 && client.iter().any(|c| c.codec == e.codec && c.chroma444);
    encs.iter().filter(ok).find(|e| e.hw).or_else(|| encs.iter().find(ok)).cloned()
}

/// Picks the encoder for a client: among codecs both sides handle, prefer
/// hardware decode on the client, then hardware encode on the host, then
/// (for hardware encoders) the more efficient codec. Software encoders
/// keep `encs` order, which is by speed, since a slow encoder costs more
/// latency than the bits it saves.
pub fn choose(encs: &[EncoderInfo], client: &[CodecCap]) -> Option<(EncoderInfo, CodecCap)> {
    let mut best: Option<(u32, &EncoderInfo, &CodecCap)> = None;
    for e in encs {
        let Some(c) = client.iter().find(|c| c.codec == e.codec) else {
            continue;
        };
        let eff = if e.hw { e.codec.efficiency() as u32 } else { 0 };
        let score = (c.hw as u32) * 100 + (e.hw as u32) * 10 + eff;
        if best.is_none_or(|b| score > b.0) {
            best = Some((score, e, c));
        }
    }
    best.map(|(_, e, c)| (e.clone(), c.clone()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn enc(element: &'static str, codec: Codec, hw: bool) -> EncoderInfo {
        EncoderInfo { element, codec, family: Family::X264, hw, chroma444: false, max_width: 4096, max_height: 4096 }
    }

    fn cap(codec: Codec, hw: bool) -> CodecCap {
        CodecCap { codec, hw, max_width: 4096, max_height: 2304, chroma444: false }
    }

    #[test]
    fn prefers_client_hw_then_host_hw_then_efficiency() {
        let encs = [
            enc("vah264lpenc", Codec::H264, true),
            enc("x265enc", Codec::H265, false),
            enc("x264enc", Codec::H264, false),
        ];
        // Client decodes H.265 in software only, H.264 in hardware: H.264.
        let (e, _) = choose(&encs, &[cap(Codec::H265, false), cap(Codec::H264, true)]).unwrap();
        assert_eq!(e.element, "vah264lpenc");
        // Both in hardware: host hardware wins over efficiency.
        let (e, _) = choose(&encs, &[cap(Codec::H265, true), cap(Codec::H264, true)]).unwrap();
        assert_eq!(e.element, "vah264lpenc");
        // No common codec.
        assert!(choose(&encs, &[cap(Codec::Av1, true)]).is_none());
    }

    #[test]
    fn chooses_a_444_encoder_the_client_decodes() {
        let mut x264 = enc("x264enc", Codec::H264, false);
        x264.chroma444 = true;
        let mut x265 = enc("x265enc", Codec::H265, false);
        x265.chroma444 = true;
        let va = enc("vah264lpenc", Codec::H264, true);
        let mut h264 = cap(Codec::H264, true);
        h264.chroma444 = true;
        let encs = [va.clone(), x265.clone(), x264.clone()];
        assert_eq!(choose_444(&encs, &[h264.clone()]).map(|e| e.element), Some("x264enc"));
        assert!(choose_444(&encs, &[cap(Codec::H264, true)]).is_none(), "the client can't decode 4:4:4");
        assert!(choose_444(&[va], &[h264]).is_none(), "no 4:4:4 encoder");
    }
}
