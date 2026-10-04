//! The Android session: like the desktop client's, minus GStreamer. Frames
//! and audio wait in bounded queues for the Kotlin decoder threads.

use std::collections::{HashMap, VecDeque};
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use bytes::Bytes;
use qr_fec::{Event, Reassembler};
use qr_net::clock::{ClockSync, now_us};
use qr_net::keystore::KeyStore;
use qr_net::session::{FramedSend, client_handshake};
use qr_net::{Identity, endpoint};
use qr_proto::datagram::{AudioPacket, Motion, PadState, fflags, kind};
use qr_proto::msg::Feedback;
use qr_proto::{ALPN, Codec, CodecCap, InputEvent, Msg, PROTO_MAJOR, PROTO_MINOR};
use tokio::sync::mpsc;

pub fn runtime() -> &'static tokio::runtime::Runtime {
    static RT: OnceLock<tokio::runtime::Runtime> = OnceLock::new();
    RT.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread().worker_threads(3).enable_all().build().expect("runtime")
    })
}

pub fn handles() -> &'static Mutex<HashMap<i64, Arc<Handle>>> {
    static H: OnceLock<Mutex<HashMap<i64, Arc<Handle>>>> = OnceLock::new();
    H.get_or_init(Mutex::default)
}

pub struct VideoFrame {
    pub id: u32,
    pub keyframe: bool,
    pub codec: u8,
    pub width: u16,
    pub height: u16,
    pub capture_local_us: i64,
    pub au: Vec<u8>,
}

/// A queue a Kotlin thread blocks on.
struct Queue<T> {
    q: Mutex<VecDeque<T>>,
    cv: Condvar,
    cap: usize,
}

impl<T> Queue<T> {
    fn new(cap: usize) -> Self {
        Self { q: Mutex::new(VecDeque::new()), cv: Condvar::new(), cap }
    }

    /// Pushes; returns false when an old item had to be dropped.
    fn push(&self, v: T) -> bool {
        let mut q = self.q.lock().expect("lock");
        let dropped = q.len() >= self.cap;
        if dropped {
            q.pop_front();
        }
        q.push_back(v);
        self.cv.notify_one();
        !dropped
    }

    fn pop(&self, timeout: Duration) -> Option<T> {
        let q = self.q.lock().expect("lock");
        let (mut q, _) = self.cv.wait_timeout_while(q, timeout, |q| q.is_empty()).expect("lock");
        q.pop_front()
    }
}

pub struct Handle {
    conn: quinn::Connection,
    ctl: mpsc::UnboundedSender<Msg>,
    input: mpsc::UnboundedSender<InputEvent>,
    video: Queue<VideoFrame>,
    audio: Queue<Vec<u8>>,
    events: Mutex<VecDeque<String>>,
    ended: AtomicBool,
    motion: Mutex<(Motion, f64, f64)>,
    pads: Mutex<HashMap<u8, u32>>,
    clock: Mutex<ClockSync>,
    /// `monotonic_us() − now_us()`: converts our clock to CLOCK_MONOTONIC.
    mono_offset: AtomicI64,
    fb: Mutex<Feedback>,
    delays: Mutex<Vec<(i64, i64)>>,
    decode_us: AtomicI64,
    tasks: Mutex<Vec<tokio::task::JoinHandle<()>>>,
}

impl Handle {
    pub fn next_video(&self, timeout_ms: u64) -> Option<VideoFrame> {
        self.video.pop(Duration::from_millis(timeout_ms))
    }

    pub fn next_audio(&self, timeout_ms: u64) -> Option<Vec<u8>> {
        self.audio.pop(Duration::from_millis(timeout_ms))
    }

    pub fn ended(&self) -> bool {
        self.ended.load(Ordering::Relaxed)
    }

    pub fn poll_event(&self) -> Option<String> {
        self.events.lock().expect("lock").pop_front()
    }

    fn event(&self, v: serde_json::Value) {
        let mut e = self.events.lock().expect("lock");
        if e.len() < 256 {
            e.push_back(v.to_string());
        }
    }

    pub fn control(&self, m: Msg) {
        let _ = self.ctl.send(m);
    }

    pub fn input(&self, e: InputEvent) {
        let _ = self.input.send(e);
    }

    pub fn request_keyframe(&self) {
        self.control(Msg::KeyframeReq { stream: 0 });
    }

    pub fn decoded(&self, _id: u32, decode_us: u64) {
        self.decode_us.store(decode_us as i64, Ordering::Relaxed);
    }

    /// Relative motion (raw units) and/or an absolute position in stream
    /// pixels; relative motion travels as running totals.
    pub fn motion(&self, dx: f64, dy: f64, abs: Option<(u16, u16)>) {
        let mut g = self.motion.lock().expect("lock");
        let (m, fx, fy) = &mut *g;
        *fx += dx;
        *fy += dy;
        let (ix, iy) = (fx.trunc(), fy.trunc());
        *fx -= ix;
        *fy -= iy;
        m.rel_x += ix as i64;
        m.rel_y += iy as i64;
        if let Some((x, y)) = abs {
            m.has_abs = true;
            m.abs_x = x;
            m.abs_y = y;
        }
        m.seq = m.seq.wrapping_add(1);
        m.t_client_us = now_us();
        let _ = self.conn.send_datagram(Bytes::from(m.encode()));
    }

    pub fn pad(&self, idx: u8, buttons: u32, axes: [i16; 6]) {
        let mut pads = self.pads.lock().expect("lock");
        let seq = pads.entry(idx).or_insert_with(|| {
            let _ = self.ctl.send(Msg::PadAttach { idx, kind: 1 });
            0
        });
        *seq = seq.wrapping_add(1);
        let p = PadState { idx, seq: *seq, buttons, axes };
        let _ = self.conn.send_datagram(Bytes::from(p.encode()));
    }

    pub fn close(&self) {
        let _ = self.ctl.send(Msg::Bye);
        self.ended.store(true, Ordering::Relaxed);
        let conn = self.conn.clone();
        runtime().spawn(async move {
            tokio::time::sleep(Duration::from_millis(200)).await;
            conn.close(0u32.into(), b"bye");
        });
        for t in self.tasks.lock().expect("lock").drain(..) {
            t.abort();
        }
    }
}

pub fn list_hosts(ms: u64) -> String {
    runtime().block_on(async {
        let Ok((_b, mut rx)) = qr_net::discovery::Browser::start() else { return "[]".into() };
        let mut out = Vec::new();
        let deadline = tokio::time::Instant::now() + Duration::from_millis(ms);
        while let Ok(Some(d)) = tokio::time::timeout_at(deadline, rx.recv()).await {
            if let qr_net::discovery::Discovery::Up(h) = d
                && !out.iter().any(|x: &serde_json::Value| x["id"] == h.host_id)
            {
                out.push(serde_json::json!({
                    "name": h.name, "id": h.host_id, "os": h.os,
                    "addr": h.addrs.first().map(|a| a.to_string()).unwrap_or_default(),
                }));
            }
        }
        serde_json::Value::Array(out).to_string()
    })
}

fn caps(codecs: u32) -> Vec<CodecCap> {
    [(Codec::Av1, 4), (Codec::H265, 2), (Codec::H264, 1)]
        .into_iter()
        .filter(|(_, bit)| codecs & bit != 0)
        .map(|(codec, _)| CodecCap { codec, hw: true, max_width: 4096, max_height: 2304, chroma444: false })
        .collect()
}

pub fn connect(
    dir: String,
    target: String,
    pin: Option<String>,
    name: String,
    rendezvous: Option<String>,
    codecs: u32,
    want: (u16, u16),
) -> Result<i64> {
    runtime().block_on(async move {
        let dir = std::path::PathBuf::from(dir);
        let id = Identity::load_or_create(&dir, &name)?;
        let keystore = Mutex::new(KeyStore::open(&dir.join("paired.json"))?);
        let peer = keystore.lock().expect("lock").by_host_id(&target).cloned();
        let expect_fp = peer.as_ref().and_then(|p| {
            let b: Vec<u8> = (0..p.fingerprint.len())
                .step_by(2)
                .filter_map(|i| p.fingerprint.get(i..i + 2).and_then(|h| u8::from_str_radix(h, 16).ok()))
                .collect();
            b.try_into().ok()
        });
        let (ep, raw) = endpoint::endpoint_with_raw("0.0.0.0:0".parse()?, &id, false, &[ALPN])?;
        let conn = if let Ok(addr) = target.parse::<SocketAddr>() {
            ep.connect(addr, "quickremote")?.await?
        } else if target.starts_with("qr-") {
            // LAN first, then the rendezvous server, then the last address.
            let lan = qr_net::discovery::Browser::start().ok();
            let mut found = None;
            if let Some((_b, mut rx)) = lan {
                let deadline = tokio::time::Instant::now() + Duration::from_millis(1500);
                while let Ok(Some(d)) = tokio::time::timeout_at(deadline, rx.recv()).await {
                    if let qr_net::discovery::Discovery::Up(h) = d
                        && h.host_id == target
                    {
                        found = h.addrs.first().copied();
                        break;
                    }
                }
            }
            let server = rendezvous.or_else(|| peer.as_ref().map(|p| p.rendezvous.clone()).filter(|s| !s.is_empty()));
            match (found, server) {
                (Some(a), _) => ep.connect(a, "quickremote")?.await?,
                (None, Some(s)) => {
                    let s = if s.contains(':') { s } else { format!("{s}:4500") };
                    let server =
                        tokio::net::lookup_host(&s).await?.next().ok_or_else(|| anyhow!("no address for {s}"))?;
                    qr_net::rendezvous::client_connect(&ep, &raw, server, &id, &target).await?.0
                }
                (None, None) => {
                    let a: SocketAddr = peer
                        .as_ref()
                        .map(|p| p.last_addr.clone())
                        .unwrap_or_default()
                        .parse()
                        .context("the host isn't on this network")?;
                    ep.connect(a, "quickremote")?.await?
                }
            }
        } else {
            let a =
                tokio::net::lookup_host(if target.contains(':') { target.clone() } else { format!("{target}:9450") })
                    .await?
                    .next()
                    .ok_or_else(|| anyhow!("no address for {target}"))?;
            ep.connect(a, "quickremote")?.await?
        };
        let hello = Msg::Hello {
            major: PROTO_MAJOR,
            minor: PROTO_MINOR,
            name: name.clone(),
            os: "android".into(),
            want_width: want.0,
            want_height: want.1,
            want_refresh: 0,
            decoders: caps(codecs),
        };
        let s = client_handshake(&conn, id.fingerprint, hello, &keystore, pin.as_deref(), expect_fp).await?;
        start(conn, s.control_tx, s.control_rx, ep).await
    })
}

async fn start(
    conn: quinn::Connection,
    control_tx: FramedSend,
    mut control_rx: qr_net::session::FramedRecv,
    ep: quinn::Endpoint,
) -> Result<i64> {
    let (ctl, mut ctl_rx) = mpsc::unbounded_channel::<Msg>();
    let (input, mut input_rx) = mpsc::unbounded_channel::<InputEvent>();
    let h = Arc::new(Handle {
        conn: conn.clone(),
        ctl,
        input,
        video: Queue::new(8),
        audio: Queue::new(32),
        events: Mutex::default(),
        ended: AtomicBool::new(false),
        motion: Mutex::new((
            Motion { seq: 0, rel_x: 0, rel_y: 0, has_abs: false, stream: 0, abs_x: 0, abs_y: 0, t_client_us: 0 },
            0.0,
            0.0,
        )),
        pads: Mutex::default(),
        clock: Mutex::default(),
        mono_offset: AtomicI64::new(qr_net::trace::monotonic_us() as i64 - now_us() as i64),
        fb: Mutex::default(),
        delays: Mutex::default(),
        decode_us: AtomicI64::new(0),
        tasks: Mutex::default(),
    });
    let mut tasks = Vec::new();
    // Control writer.
    {
        let mut tx = control_tx;
        tasks.push(tokio::spawn(async move {
            while let Some(m) = ctl_rx.recv().await {
                if tx.send(&m).await.is_err() {
                    return;
                }
            }
        }));
    }
    // Input stream.
    {
        let mut st = conn.open_uni().await?;
        st.write_all(&[qr_proto::uni::INPUT]).await?;
        let mut st = FramedSend(st);
        tasks.push(tokio::spawn(async move {
            while let Some(e) = input_rx.recv().await {
                if st.send_raw(&e.encode()).await.is_err() {
                    return;
                }
            }
        }));
    }
    // Control reader → events.
    {
        let h = h.clone();
        tasks.push(tokio::spawn(async move {
            loop {
                match control_rx.recv().await {
                    Ok(Msg::Pong { t_client, t_host, .. }) => h.clock.lock().expect("lock").sample(t_client, t_host, now_us()),
                    Ok(Msg::SessionStart { profile, path, .. }) => {
                        h.event(serde_json::json!({"t": "session", "profile": profile, "path": path}))
                    }
                    Ok(Msg::StreamStarted { codec, width, height, fps, .. }) => {
                        h.event(serde_json::json!({"t": "stream", "codec": codec, "width": width, "height": height, "fps": fps}))
                    }
                    Ok(Msg::HostStats(s)) => h.event(serde_json::json!({"t": "hoststats", "kbps": s.bitrate_kbps, "fps": s.fps, "encoder": s.encoder})),
                    Ok(Msg::Rumble { idx, strong, weak, ms }) => {
                        h.event(serde_json::json!({"t": "rumble", "idx": idx, "strong": strong, "weak": weak, "ms": ms}))
                    }
                    Ok(Msg::Clipboard { kind: 1, data }) => {
                        h.event(serde_json::json!({"t": "clipboard", "text": String::from_utf8_lossy(&data)}))
                    }
                    Ok(Msg::FileOffer { id, .. }) => {
                        // Files from the host aren't supported on Android yet.
                        h.control(Msg::FileCancel { id, reason: "not supported on this client".into() })
                    }
                    Ok(Msg::Error { text, .. }) => {
                        h.event(serde_json::json!({"t": "error", "text": text}));
                        break;
                    }
                    Ok(_) => {}
                    Err(e) => {
                        h.event(serde_json::json!({"t": "ended", "text": e.to_string()}));
                        break;
                    }
                }
            }
            h.ended.store(true, Ordering::Relaxed);
        }));
    }
    // Datagrams: video reassembly and audio.
    {
        let h = h.clone();
        tasks.push(tokio::spawn(async move {
            let base = qr_net::clock::base();
            let mut reasm = Reassembler::new(0, Duration::from_millis(34));
            let mut need_key = true;
            let mut tick = tokio::time::interval(Duration::from_millis(5));
            loop {
                let events = tokio::select! {
                    d = h.conn.read_datagram() => {
                        let Ok(d) = d else { break };
                        match d.first() {
                            Some(&kind::VIDEO) => reasm.push(&d, base.elapsed()).unwrap_or_default(),
                            Some(&kind::AUDIO) => {
                                if let Ok(a) = AudioPacket::decode(&d) {
                                    h.audio.push(a.data);
                                }
                                Vec::new()
                            }
                            _ => Vec::new(),
                        }
                    }
                    _ = tick.tick() => reasm.poll(base.elapsed()),
                };
                for ev in events {
                    match ev {
                        Event::Frame(f) => {
                            let key = f.meta.flags & fflags::KEYFRAME != 0;
                            if need_key && !key {
                                continue;
                            }
                            need_key = false;
                            let complete = f.completed.as_micros() as i64;
                            h.delays.lock().expect("lock").push((complete, complete - f.meta.send_us as i64));
                            let local = h.clock.lock().expect("lock").to_client(f.meta.capture_us);
                            let frame = VideoFrame {
                                id: f.id,
                                keyframe: key,
                                codec: f.meta.codec,
                                width: f.meta.width,
                                height: f.meta.height,
                                capture_local_us: local + h.mono_offset.load(Ordering::Relaxed),
                                au: f.au,
                            };
                            // A dropped frame breaks the reference chain.
                            if !h.video.push(frame) {
                                need_key = true;
                                h.request_keyframe();
                            }
                        }
                        Event::Lost { id } => {
                            need_key = true;
                            h.control(Msg::FrameLost { stream: 0, frame: id });
                        }
                    }
                }
                let st = reasm.take_stats();
                let mut fb = h.fb.lock().expect("lock");
                fb.bytes_rx += st.bytes_rx as u32;
                fb.shards_rx += st.shards_rx;
                fb.shards_lost += st.shards_lost;
                fb.shards_repaired += st.shards_repaired;
                fb.frames_rx += st.frames_rx as u16;
                fb.frames_lost += st.frames_lost as u16;
            }
            h.ended.store(true, Ordering::Relaxed);
            h.event(serde_json::json!({"t": "ended", "text": "connection closed"}));
        }));
    }
    // Pings and feedback.
    {
        let h = h.clone();
        tasks.push(tokio::spawn(async move {
            let mut seq = 0u32;
            let mut tick = tokio::time::interval(Duration::from_millis(50));
            loop {
                tick.tick().await;
                seq = seq.wrapping_add(1);
                if seq % 4 == 0 {
                    h.control(Msg::Ping { seq, t_client: now_us() });
                }
                let mut fb = std::mem::take(&mut *h.fb.lock().expect("lock"));
                fb.seq = seq;
                fb.window_ms = 50;
                fb.decode_us = h.decode_us.load(Ordering::Relaxed) as u32;
                let d = std::mem::take(&mut *h.delays.lock().expect("lock"));
                if !d.is_empty() {
                    fb.delay_min_us = d.iter().map(|x| x.1).min().unwrap_or(0) as i32;
                    fb.delay_max_us = d.iter().map(|x| x.1).max().unwrap_or(0) as i32;
                }
                h.control(Msg::Feedback(fb));
            }
        }));
    }
    *h.tasks.lock().expect("lock") = tasks;
    // Keep the endpoint alive as long as the session.
    let keep = h.clone();
    runtime().spawn(async move {
        while !keep.ended() {
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
        drop(ep);
    });
    static NEXT: AtomicI64 = AtomicI64::new(1);
    let id = NEXT.fetch_add(1, Ordering::Relaxed);
    handles().lock().expect("lock").insert(id, h);
    if id < 0 {
        bail!("handle overflow");
    }
    Ok(id)
}
