//! The web client's Rust core, compiled to WebAssembly: PIN pairing (the
//! same SPAKE2 as native peers) and FEC reassembly of video datagrams.

use std::time::Duration;

use qr_fec::{Event, Reassembler};
use qr_pair::{Keys, Pairing, Transcript};
use wasm_bindgen::prelude::*;

/// Client side of PIN pairing.
#[wasm_bindgen]
pub struct WebPairing {
    state: Option<Pairing>,
    msg: Vec<u8>,
    keys: Option<Keys>,
}

#[wasm_bindgen]
impl WebPairing {
    #[wasm_bindgen(constructor)]
    pub fn new(pin: &str) -> WebPairing {
        let (state, msg) = Pairing::client(pin);
        WebPairing { state: Some(state), msg, keys: None }
    }

    /// The SPAKE2 message to send to the host.
    pub fn message(&self) -> Vec<u8> {
        self.msg.clone()
    }

    /// Finishes with the host's SPAKE2 message and returns this side's
    /// confirmation MAC. `client_fp` is SHA-256 of the browser's public key
    /// (SPKI), `host_fp` SHA-256 of the host's web certificate.
    pub fn finish(&mut self, host_msg: &[u8], client_fp: &[u8], host_fp: &[u8]) -> Result<Vec<u8>, JsError> {
        let t = Transcript {
            client_fp: client_fp.try_into().map_err(|_| JsError::new("bad client fingerprint"))?,
            host_fp: host_fp.try_into().map_err(|_| JsError::new("bad host fingerprint"))?,
            exporter: [0; 32],
        };
        let keys =
            self.state.take().and_then(|s| s.finish(host_msg, &t)).ok_or_else(|| JsError::new("pairing failed"))?;
        let mac = keys.client_mac().to_vec();
        self.keys = Some(keys);
        Ok(mac)
    }

    /// Checks the host's confirmation MAC.
    pub fn verify(&self, host_mac: &[u8]) -> bool {
        let Ok(m) = <[u8; 32]>::try_from(host_mac) else { return false };
        self.keys.as_ref().is_some_and(|k| k.check(false, &m))
    }
}

/// A complete frame (or a loss), handed to JavaScript.
#[wasm_bindgen]
pub struct WebFrame {
    pub id: u32,
    pub lost: bool,
    pub keyframe: bool,
    pub codec: u8,
    pub width: u16,
    pub height: u16,
    pub capture_us: f64,
    pub encode_start_us: f64,
    pub encode_end_us: f64,
    pub send_us: f64,
    pub repaired: u32,
    au: Vec<u8>,
}

#[wasm_bindgen]
impl WebFrame {
    /// The encoded access unit.
    #[wasm_bindgen(getter)]
    pub fn au(&self) -> Vec<u8> {
        self.au.clone()
    }
}

/// Rebuilds video frames from datagrams. Times are the page's clock in ms
/// (`performance.now()`).
#[wasm_bindgen]
pub struct WebReassembler {
    r: Reassembler,
    out: std::collections::VecDeque<Event>,
}

fn t(ms: f64) -> Duration {
    Duration::from_micros((ms.max(0.0) * 1000.0) as u64)
}

#[wasm_bindgen]
impl WebReassembler {
    /// `deadline_ms`: how long a later frame waits before an incomplete one
    /// is declared lost.
    #[wasm_bindgen(constructor)]
    pub fn new(deadline_ms: u32) -> WebReassembler {
        WebReassembler { r: Reassembler::new(0, Duration::from_millis(deadline_ms as u64)), out: Default::default() }
    }

    /// Drops what is in flight (a restarted stream); frame numbering goes on.
    pub fn reset(&mut self) {
        // The host numbers frames on across a stream's restarts: keep the
        // position (resetting to 0 dropped every later frame).
        let next = self.r.next_expected();
        self.r.reset(next);
        self.out.clear();
    }

    /// Feeds one video datagram; returns how many events are ready.
    pub fn push(&mut self, d: &[u8], now_ms: f64) -> usize {
        if let Ok(ev) = self.r.push(d, t(now_ms)) {
            self.out.extend(ev);
        }
        self.out.len()
    }

    /// Declares overdue frames lost; call every few ms.
    pub fn poll(&mut self, now_ms: f64) -> usize {
        let ev = self.r.poll(t(now_ms));
        self.out.extend(ev);
        self.out.len()
    }

    /// The next ready event, oldest first.
    // `next` is the JavaScript API's name (not an iterator).
    #[allow(clippy::should_implement_trait)]
    pub fn next(&mut self) -> Option<WebFrame> {
        Some(match self.out.pop_front()? {
            Event::Frame(f) => WebFrame {
                id: f.id,
                lost: false,
                keyframe: f.meta.flags & qr_proto::datagram::fflags::KEYFRAME != 0,
                codec: f.meta.codec,
                width: f.meta.width,
                height: f.meta.height,
                capture_us: f.meta.capture_us as f64,
                encode_start_us: f.meta.encode_start_us as f64,
                encode_end_us: f.meta.encode_end_us as f64,
                send_us: f.meta.send_us as f64,
                repaired: f.repaired,
                au: f.au,
            },
            Event::Lost { id } => WebFrame {
                id,
                lost: true,
                keyframe: false,
                codec: 0,
                width: 0,
                height: 0,
                capture_us: 0.0,
                encode_start_us: 0.0,
                encode_end_us: 0.0,
                send_us: 0.0,
                repaired: 0,
                au: vec![],
            },
        })
    }

    /// Counters since the last call: bytes, shards received, shards lost,
    /// shards repaired, frames received, frames lost.
    pub fn take_stats(&mut self) -> Vec<f64> {
        let s = self.r.take_stats();
        vec![
            s.bytes_rx as f64,
            s.shards_rx as f64,
            s.shards_lost as f64,
            s.shards_repaired as f64,
            s.frames_rx as f64,
            s.frames_lost as f64,
        ]
    }
}

/// SHA-256 of `data`.
#[wasm_bindgen]
pub fn sha256(data: &[u8]) -> Vec<u8> {
    use sha2::Digest;
    sha2::Sha256::digest(data).to_vec()
}
