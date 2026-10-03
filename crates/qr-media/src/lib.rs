//! GStreamer media for QuickRemote: encoder and decoder discovery, the
//! low-latency encode pipeline (host), the decode pipeline (client) and
//! Opus audio.

pub mod audio;
pub mod decode;
pub mod encode;
pub mod probe;

use std::sync::OnceLock;

use anyhow::Result;
use gstreamer as gst;
use gstreamer::glib;
use gstreamer::prelude::*;
use qr_proto::Codec;

/// Initializes GStreamer once.
pub fn init() -> Result<()> {
    static INIT: OnceLock<std::result::Result<(), String>> = OnceLock::new();
    INIT.get_or_init(|| gst::init().map_err(|e| e.to_string()))
        .clone()
        .map_err(|e| anyhow::anyhow!("GStreamer init failed: {e}"))
}

/// Caps of an encoded stream as the parsers and decoders want it: Annex B
/// access units (H.264/H.265) or OBU temporal units (AV1).
pub fn encoded_caps(codec: Codec) -> gst::Caps {
    match codec {
        Codec::H264 => gst::Caps::builder("video/x-h264")
            .field("stream-format", "byte-stream")
            .field("alignment", "au")
            .build(),
        Codec::H265 => gst::Caps::builder("video/x-h265")
            .field("stream-format", "byte-stream")
            .field("alignment", "au")
            .build(),
        Codec::Av1 => gst::Caps::builder("video/x-av1")
            .field("stream-format", "obu-stream")
            .field("alignment", "tu")
            .build(),
    }
}

pub fn parser(codec: Codec) -> &'static str {
    match codec {
        Codec::H264 => "h264parse",
        Codec::H265 => "h265parse",
        Codec::Av1 => "av1parse",
    }
}

/// Converts GStreamer clock times to `qr_net`-style monotonic microseconds.
///
/// The pipeline clock is the system monotonic clock, as is `Instant`; the
/// offset between the two bases is sampled once.
#[derive(Clone, Copy, Debug)]
pub struct ClockMap {
    /// `our_us − gst_ns / 1000` at the moment of sampling.
    offset_us: i64,
}

impl ClockMap {
    /// `now_us` is the caller's clock (e.g. `qr_net::clock::now_us`).
    pub fn new(now_us: impl Fn() -> u64) -> Self {
        let clock = gst::SystemClock::obtain();
        let a = now_us();
        let g = clock.time().nseconds() / 1000;
        let b = now_us();
        Self {
            offset_us: ((a + b) / 2) as i64 - g as i64,
        }
    }

    /// A buffer's absolute capture time: `base_time + pts`.
    pub fn to_us(&self, gst_abs: gst::ClockTime) -> u64 {
        (gst_abs.nseconds() as i64 / 1000 + self.offset_us).max(0) as u64
    }
}

/// Sets a property from its string form when the element has it; returns
/// whether it existed. Unlike `set_property_from_str`, an unparsable value
/// (an enum nick another GStreamer version lacks) only logs a warning.
pub fn set_prop(e: &impl IsA<gst::Object>, name: &str, value: &str) -> bool {
    use glib::translate::ToGlibPtr;
    if e.find_property(name).is_none() {
        return false;
    }
    let obj: &gst::Object = e.upcast_ref();
    let (Ok(n), Ok(v)) = (std::ffi::CString::new(name), std::ffi::CString::new(value)) else {
        return false;
    };
    // SAFETY: `obj` is a live GObject for the duration of the call, and
    // both strings are NUL-terminated and outlive it.
    unsafe {
        gst::ffi::gst_util_set_object_arg(
            ToGlibPtr::<*mut gst::ffi::GstObject>::to_glib_none(obj).0
                as *mut glib::gobject_ffi::GObject,
            n.as_ptr(),
            v.as_ptr(),
        );
    }
    true
}
