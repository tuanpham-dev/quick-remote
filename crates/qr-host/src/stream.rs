//! One video stream: encoder → FEC packetizer → QUIC datagrams, with rate
//! control applied to the encoder.

use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::Result;
use bytes::Bytes;
use qr_fec::Packetizer;
use qr_media::encode::{EncodeConfig, EncodedFrame, Encoder};
use qr_proto::datagram::{FrameMeta, fflags, vflags};
use qr_proto::msg::Feedback;
use tokio::sync::mpsc;

use crate::ratectl::RateCtl;

/// Datagram send-buffer fill above which capture is held (frames skipped
/// before encoding) rather than queued.
const HOLD_FILL: f64 = 0.5;

#[derive(Default)]
pub struct StreamCounters {
    pub bytes_sent: AtomicU64,
    pub frames_sent: AtomicU32,
    pub capture_to_encoded_us: AtomicU64,
    pub encode_us: AtomicU64,
}

pub struct VideoStream {
    pub id: u8,
    pub encoder: Arc<Encoder>,
    pub rate: Mutex<RateCtl>,
    pub counters: Arc<StreamCounters>,
    pub width: u32,
    pub height: u32,
    last_keyframe_req: Mutex<Option<Instant>>,
    /// Parity ratio and loss estimate ×1000, read by the sender per frame.
    parity: Arc<AtomicU32>,
    loss: Arc<AtomicU32>,
    sender: tokio::task::JoinHandle<()>,
}

impl VideoStream {
    pub fn start(
        id: u8,
        cfg: &EncodeConfig,
        rate: RateCtl,
        conn: quinn::Connection,
    ) -> Result<Arc<Self>> {
        let (tx, rx) = mpsc::channel::<EncodedFrame>(4);
        let clock = qr_media::ClockMap::new(qr_net::clock::now_us);
        let encoder = Arc::new(Encoder::start(cfg, clock, move |f| {
            // A full channel means the sender is behind: drop this frame's
            // bits rather than queue latency (the sender requests a
            // keyframe if the chain breaks).
            if tx.try_send(f).is_err() {
                tracing::debug!("encoded frame dropped: sender busy");
            }
        })?);
        encoder.set_bitrate(rate.target_kbps);
        encoder.set_max_fps(rate.fps);
        let counters = Arc::new(StreamCounters::default());
        let parity = Arc::new(AtomicU32::new((rate.parity * 1000.0) as u32));
        let loss = Arc::new(AtomicU32::new((rate.loss_ewma * 1000.0) as u32));
        let sender = tokio::spawn(send_loop(
            id,
            rx,
            conn,
            encoder.clone(),
            counters.clone(),
            parity.clone(),
            loss.clone(),
        ));
        let s = Arc::new(Self {
            id,
            encoder,
            rate: Mutex::new(rate),
            counters,
            width: cfg.width,
            height: cfg.height,
            last_keyframe_req: Mutex::new(None),
            parity,
            loss,
            sender,
        });
        Ok(s)
    }

    /// Applies client feedback to the rate controller and the encoder.
    pub fn feedback(&self, fb: &Feedback) {
        let mut r = self.rate.lock().expect("lock");
        if r.on_feedback(fb, Instant::now()) {
            self.encoder.set_bitrate(r.target_kbps);
            self.encoder.set_max_fps(r.fps);
            self.parity
                .store((r.parity * 1000.0) as u32, Ordering::Relaxed);
            self.loss
                .store((r.loss_ewma * 1000.0) as u32, Ordering::Relaxed);
            tracing::debug!(kbps = r.target_kbps, fps = r.fps, parity = r.parity, "rate");
        }
    }

    /// A frame could not be rebuilt on the client: send a keyframe, at most
    /// once per `min_gap` (about one RTT) so a burst of losses costs one.
    pub fn frame_lost(&self, min_gap: Duration) {
        let mut last = self.last_keyframe_req.lock().expect("lock");
        let now = Instant::now();
        if last.is_some_and(|t| now.duration_since(t) < min_gap) {
            return;
        }
        *last = Some(now);
        self.encoder.force_keyframe();
    }

    pub fn parity(&self) -> f32 {
        self.parity.load(Ordering::Relaxed) as f32 / 1000.0
    }
}

impl Drop for VideoStream {
    fn drop(&mut self) {
        self.sender.abort();
    }
}

async fn send_loop(
    stream: u8,
    mut rx: mpsc::Receiver<EncodedFrame>,
    conn: quinn::Connection,
    encoder: Arc<Encoder>,
    counters: Arc<StreamCounters>,
    parity_permille: Arc<AtomicU32>,
    loss_permille: Arc<AtomicU32>,
) {
    let mut frame_id: u32 = 0;
    let mut pk = Packetizer::new(stream, 1200, 0.1);
    let buffer = qr_net::endpoint::BUFFER as f64;
    let mut recheck = tokio::time::interval(Duration::from_millis(5));
    loop {
        let f = tokio::select! {
            f = rx.recv() => match f {
                Some(f) => f,
                None => return,
            },
            // While capture is held no frames arrive here, so the queue
            // has to be re-checked on a timer or the hold never lifts.
            _ = recheck.tick() => {
                let fill = 1.0 - conn.datagram_send_buffer_space() as f64 / buffer;
                encoder.hold(fill > HOLD_FILL);
                continue;
            }
        };
        let Some(max) = conn.max_datagram_size() else {
            tracing::warn!("peer does not accept datagrams");
            return;
        };
        pk.max_datagram = max;
        pk.parity = parity_permille.load(Ordering::Relaxed) as f32 / 1000.0;
        pk.loss = loss_permille.load(Ordering::Relaxed) as f32 / 1000.0;
        let meta = FrameMeta {
            codec: f.codec as u8,
            flags: if f.keyframe { fflags::KEYFRAME } else { 0 },
            width: f.width as u16,
            height: f.height as u16,
            len: 0,
            capture_us: f.capture_us,
            encode_start_us: f.encode_start_us,
            encode_end_us: f.encode_end_us,
            send_us: qr_net::clock::now_us(),
        };
        let dgrams = pk.frame(
            frame_id,
            if f.keyframe { vflags::KEYFRAME } else { 0 },
            &meta,
            &f.au,
        );
        frame_id = frame_id.wrapping_add(1);
        let mut bytes = 0u64;
        for d in dgrams {
            bytes += d.len() as u64;
            if let Err(e) = conn.send_datagram(Bytes::from(d)) {
                tracing::info!("video send stopped: {e}");
                return;
            }
        }
        counters.bytes_sent.fetch_add(bytes, Ordering::Relaxed);
        counters.frames_sent.fetch_add(1, Ordering::Relaxed);
        counters.capture_to_encoded_us.fetch_add(
            f.encode_end_us.saturating_sub(f.capture_us),
            Ordering::Relaxed,
        );
        counters.encode_us.fetch_add(
            f.encode_end_us.saturating_sub(f.encode_start_us),
            Ordering::Relaxed,
        );
        // Hold capture while the datagram queue is more than half full.
        let fill = 1.0 - conn.datagram_send_buffer_space() as f64 / buffer;
        encoder.hold(fill > HOLD_FILL);
    }
}
