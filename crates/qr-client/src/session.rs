//! Client session core, shared by the window and headless modes: connect,
//! authenticate, receive and decode video, keep the clock in sync, and send
//! feedback.

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use bytes::Bytes;
use qr_fec::{Event, Reassembler};
use qr_media::audio::{AudioCapture, AudioPlayer, AudioSink, AudioSource, PLAYOUT_US};
use qr_media::decode::{DecodedFrame, Decoder};
use qr_media::probe::DecoderInfo;
use qr_net::clock::{ClockSync, now_us};
use qr_net::keystore::KeyStore;
use qr_net::session::{FramedSend, client_handshake};
use qr_net::{Fingerprint, Identity, endpoint};
use qr_proto::datagram::{AudioPacket, kind};
use qr_proto::msg::{Feedback, HostStats};
use qr_proto::{ALPN, Codec, InputEvent, Msg, PROTO_MAJOR, PROTO_MINOR};
use tokio::sync::Mutex as AsyncMutex;

/// Prefix of an end reason meaning the host gave this session to the same
/// client connecting elsewhere: no reconnecting.
pub const REPLACED: &str = "replaced: ";

/// What the overlay and `--stats-json` show, refreshed every second.
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct Stats {
    pub fps: f32,
    pub kbps: u32,
    pub codec: String,
    pub decoder: String,
    pub encoder: String,
    pub width: u32,
    pub height: u32,
    pub path: String,
    pub profile: String,
    pub rtt_ms: f32,
    pub loss_pct: f32,
    pub fec_repaired: u32,
    pub frames_lost: u32,
    pub parity_pct: u8,
    /// Mean latency split, ms: capture→encoded (host), host send→client
    /// frame complete (network), decode, and the end-to-end total.
    pub capture_ms: f32,
    pub encode_ms: f32,
    pub network_ms: f32,
    pub decode_ms: f32,
    pub present_ms: f32,
    pub total_ms: f32,
    /// Host capture → client playout of audio, ms (arrival + playout buffer).
    pub audio_ms: f32,
    /// The main stream's monitor.
    pub monitor: u8,
    /// Extra streams by id.
    pub streams: std::collections::BTreeMap<u8, StreamStats>,
}

/// A stream besides the main one (a second window), in `Stats::streams`.
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct StreamStats {
    pub monitor: u8,
    pub width: u32,
    pub height: u32,
    pub fps: f32,
}

/// Where rumble from the host goes: (pad, strong, weak, ms).
pub type RumbleTx = Arc<Mutex<Option<std::sync::mpsc::Sender<(u8, u16, u16, u16)>>>>;

/// Decoded frames of streams other than 0: (stream, frame).
pub type ExtraSink = Arc<dyn Fn(u8, DecodedFrame) + Send + Sync>;

/// Per-frame timeline, client clock µs (`None` until known).
#[derive(Debug, Clone, Copy)]
pub struct FrameTimes {
    pub capture: i64,
    pub encode_start: i64,
    pub encode_end: i64,
    pub send: i64,
    pub complete: i64,
}

#[derive(Default)]
struct Window {
    frames: u32,
    bytes: u64,
    shards_rx: u32,
    shards_lost: u32,
    sum_capture: f64,
    sum_encode: f64,
    sum_network: f64,
    sum_decode: f64,
    sum_present: f64,
    sum_total: f64,
    lat_n: u32,
}

pub struct Shared {
    pub clock: ClockSync,
    pub stats: Stats,
    win: Window,
    /// Frame id → timeline, until decoded.
    pending: std::collections::HashMap<u32, FrameTimes>,
    /// Totals from decoded frames for latency tools: (total µs, parts).
    pub samples: Vec<LatencySample>,
    pub keep_samples: bool,
    /// Feedback accumulation (since last report).
    fb: Feedback,
    delays: Vec<(i64, i64)>,
    pub host: Option<HostStats>,
    audio_lat: (f64, u32),
    /// Frames decoded per extra stream in this stats window.
    extra_frames: std::collections::HashMap<u8, u32>,
    /// The host's monitors.
    pub monitors: Vec<qr_proto::Monitor>,
    /// What the host's desktop can do and applied (protocol 1.1+).
    pub display: Option<DisplayState>,
}

/// The host's latest `DisplayInfo` for stream 0.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct DisplayState {
    pub kind: u8,
    pub width: u16,
    pub height: u16,
    pub scale_pct: u16,
    pub note: String,
}

#[derive(Debug, Clone, Copy, serde::Serialize)]
pub struct LatencySample {
    /// Client time the frame was decoded, µs.
    pub at_us: i64,
    pub frame: u32,
    /// The latency fields are valid (clock synced).
    pub synced: bool,
    pub capture_us: i64,
    pub encode_us: i64,
    pub network_us: i64,
    pub decode_us: i64,
    pub present_us: i64,
    pub total_us: i64,
}

pub struct Connected {
    pub endpoint: quinn::Endpoint,
    pub conn: quinn::Connection,
    pub control: Arc<AsyncMutex<FramedSend>>,
    pub shared: Arc<Mutex<Shared>>,
    pub host_name: String,
    pub host_id: String,
    pub tasks: Vec<tokio::task::JoinHandle<()>>,
    pub ended: tokio::sync::watch::Receiver<Option<String>>,
    pub input: Input,
    pub files: Arc<qr_net::files::Files>,
    /// Sends a control message from any thread.
    pub ctl: qr_net::files::Control,
    /// The host's protocol minor version (display requests, pause: 1+).
    pub host_minor: u16,
    /// Rumble from the host for the pad thread: (pad, strong, weak, ms).
    pub rumble: RumbleTx,
    /// Where extra streams' pictures go (none: they are only counted).
    pub extra_sink: Arc<Mutex<Option<ExtraSink>>>,
}

/// Sends input: keys, buttons and text on the reliable input stream;
/// pointer motion as datagrams carrying running totals.
#[derive(Clone)]
pub struct Input {
    tx: tokio::sync::mpsc::UnboundedSender<InputEvent>,
    conn: quinn::Connection,
    motion: Arc<Mutex<Motion>>,
}

impl Input {
    pub fn send(&self, ev: InputEvent) {
        let _ = self.tx.send(ev);
    }

    /// Relative motion (pointer locked), in raw device units.
    pub fn rel(&self, dx: f64, dy: f64) {
        let mut m = self.motion.lock().expect("lock");
        m.frac_x += dx;
        m.frac_y += dy;
        let (ix, iy) = (m.frac_x.trunc(), m.frac_y.trunc());
        m.frac_x -= ix;
        m.frac_y -= iy;
        if ix == 0.0 && iy == 0.0 {
            return;
        }
        m.d.rel_x += ix as i64;
        m.d.rel_y += iy as i64;
        self.flush(&mut m);
    }

    /// Absolute position in stream pixels.
    pub fn abs(&self, stream: u8, x: u16, y: u16) {
        let mut m = self.motion.lock().expect("lock");
        m.d.has_abs = true;
        m.d.stream = stream;
        m.d.abs_x = x;
        m.d.abs_y = y;
        self.flush(&mut m);
    }

    fn flush(&self, m: &mut Motion) {
        m.d.seq = m.d.seq.wrapping_add(1);
        m.d.t_client_us = now_us();
        let _ = self.conn.send_datagram(Bytes::from(m.d.encode()));
    }
}

struct Motion {
    d: qr_proto::datagram::Motion,
    frac_x: f64,
    frac_y: f64,
}

impl Drop for Connected {
    fn drop(&mut self) {
        for t in &self.tasks {
            t.abort();
        }
    }
}

/// Where the host is.
#[derive(Debug, Clone)]
pub enum Target {
    Addr(SocketAddr),
    /// By host ID through a rendezvous server (direct after hole punching,
    /// or relayed).
    Rendezvous {
        server: SocketAddr,
        host_id: String,
    },
}

pub struct ConnectOpts {
    pub target: Target,
    pub expect_fp: Option<Fingerprint>,
    pub pin: Option<String>,
    pub name: String,
    pub want: (u16, u16, u16),
    pub decoders: Vec<DecoderInfo>,
    pub decoder_override: Option<String>,
    pub keep_samples: bool,
    /// Where host audio plays (`None`: muted).
    pub audio_out: Option<AudioSink>,
    /// Microphone to send to the host (`None`: off).
    pub mic: Option<AudioSource>,
    /// Where files from the host are saved.
    pub recv_dir: std::path::PathBuf,
    /// X display whose clipboard is synced with the host (`None`: off).
    pub clipboard: Option<String>,
}

/// Called with every decoded frame (window mode renders it).
pub type FrameSink = Arc<dyn Fn(DecodedFrame, FrameTimes) + Send + Sync>;

pub async fn connect(id: &Identity, keystore: &Mutex<KeyStore>, o: ConnectOpts, sink: FrameSink) -> Result<Connected> {
    let v6 = match &o.target {
        Target::Addr(a) => a.is_ipv6(),
        Target::Rendezvous { server, .. } => server.is_ipv6(),
    };
    let bind: SocketAddr = if v6 { "[::]:0".parse()? } else { "0.0.0.0:0".parse()? };
    let (ep, raw) = endpoint::endpoint_with_raw(bind, id, false, &[ALPN])?;
    let (conn, via) = match &o.target {
        Target::Addr(a) => (ep.connect(*a, "quickremote")?.await.with_context(|| format!("connecting to {a}"))?, None),
        Target::Rendezvous { server, host_id } => {
            let (c, p) = qr_net::rendezvous::client_connect(&ep, &raw, *server, id, host_id).await?;
            (c, Some(p))
        }
    };
    let caps = qr_media::probe::decoder_caps(&o.decoders);
    let hello = Msg::Hello {
        major: PROTO_MAJOR,
        minor: PROTO_MINOR,
        name: o.name.clone(),
        os: std::env::consts::OS.into(),
        want_width: o.want.0,
        want_height: o.want.1,
        want_refresh: o.want.2,
        decoders: caps,
    };
    let s = client_handshake(&conn, id.fingerprint, hello, keystore, o.pin.as_deref(), o.expect_fp).await?;
    let host_minor = s.host_minor;
    let mut rx = s.control_rx;
    let control = Arc::new(AsyncMutex::new(s.control_tx));
    let shared = Arc::new(Mutex::new(Shared {
        clock: ClockSync::default(),
        stats: Stats::default(),
        win: Window::default(),
        pending: Default::default(),
        samples: Vec::new(),
        keep_samples: o.keep_samples,
        fb: Feedback::default(),
        delays: Vec::new(),
        host: None,
        audio_lat: (0.0, 0),
        extra_frames: Default::default(),
        monitors: Vec::new(),
        display: None,
    }));

    // Wait for the session to start and the first stream to be announced.
    let mut codec = None;
    let mut chroma444 = false;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while codec.is_none() {
        let m = tokio::time::timeout_at(deadline, rx.recv()).await.context("waiting for the stream")??;
        match m {
            Msg::MonitorList { monitors } => {
                let names: Vec<String> =
                    monitors.iter().map(|m| format!("{}:{}:{}x{}", m.id, m.name, m.width, m.height)).collect();
                tracing::info!(monitors = %names.join(" "), "host monitors");
                shared.lock().expect("lock").monitors = monitors;
            }
            Msg::SessionStart { profile, path, .. } => {
                let mut sh = shared.lock().expect("lock");
                sh.stats.profile = if profile == 1 { "LAN" } else { "Internet" }.into();
                sh.stats.path = match via {
                    Some(qr_net::rendezvous::Path::Relayed) => "Relayed",
                    Some(qr_net::rendezvous::Path::Direct) => "Direct",
                    None => match path {
                        1 => "LAN",
                        2 => "Direct",
                        _ => "Relayed",
                    },
                }
                .into();
            }
            Msg::StreamStarted { codec: c, width, height, monitor, chroma444: c444, .. } => {
                chroma444 = c444;
                let mut sh = shared.lock().expect("lock");
                sh.stats.monitor = monitor;
                sh.stats.width = width as u32;
                sh.stats.height = height as u32;
                codec = Some(Codec::from_u8(c)?);
            }
            Msg::Error { text, .. } => bail!(text),
            _ => {}
        }
    }
    let codec = codec.expect("loop exits with a codec");

    let (end_tx, ended) = tokio::sync::watch::channel(None::<String>);
    let mut tasks = Vec::new();

    // Input stream.
    let (in_tx, mut in_rx) = tokio::sync::mpsc::unbounded_channel::<InputEvent>();
    {
        let mut st = conn.open_uni().await?;
        st.write_all(&[qr_proto::uni::INPUT]).await?;
        let mut st = FramedSend(st);
        tasks.push(tokio::spawn(async move {
            while let Some(ev) = in_rx.recv().await {
                if st.send_raw(&ev.encode()).await.is_err() {
                    return;
                }
            }
        }));
    }
    let input = Input {
        tx: in_tx,
        conn: conn.clone(),
        motion: Arc::new(Mutex::new(Motion {
            d: qr_proto::datagram::Motion {
                seq: 0,
                rel_x: 0,
                rel_y: 0,
                has_abs: false,
                stream: 0,
                abs_x: 0,
                abs_y: 0,
                t_client_us: 0,
            },
            frac_x: 0.0,
            frac_y: 0.0,
        })),
    };

    // Decoder: completed frames in, pictures out to the sink with timings.
    // Rebuilt when a restarted stream changes codec or chroma (4:4:4).
    let make_main = {
        let (decoders, ovr, shared) = (o.decoders.clone(), o.decoder_override.clone(), shared.clone());
        move |codec: Codec, chroma444: bool| -> Result<Decoder> {
            let el = pick_decoder(&decoders, ovr.as_deref(), codec, chroma444)?;
            {
                let mut sh = shared.lock().expect("lock");
                sh.stats.codec = codec.name().into();
                sh.stats.decoder = el.clone();
            }
            let (sh2, sink) = (shared.clone(), sink.clone());
            Decoder::start(codec, &el, move |f| {
                let done = now_us() as i64;
                let times = {
                    let mut sh = sh2.lock().expect("lock");
                    let Some(t) = sh.pending.remove(&f.frame_id) else {
                        return;
                    };
                    sh.record(f.frame_id, &t, done, f.decode_us as i64, f.work_us);
                    t
                };
                sink(f, times);
            })
        }
    };
    let decoder = make_main(codec, chroma444)?;

    // Extra streams (second windows) get their own decoders.
    let extra_sink: Arc<Mutex<Option<ExtraSink>>> = Arc::default();
    // (stream, (codec, 4:4:4)) as streams start; `None` when one stops.
    let (extra_tx, extra_rx) = tokio::sync::mpsc::unbounded_channel::<(u8, Option<(Codec, bool)>)>();
    let make_extra = {
        let (decoders, ovr, sink, shared) =
            (o.decoders.clone(), o.decoder_override.clone(), extra_sink.clone(), shared.clone());
        move |stream: u8, codec: Codec, chroma444: bool| -> Result<Decoder> {
            let el = pick_decoder(&decoders, ovr.as_deref(), codec, chroma444)?;
            let (sink, shared) = (sink.clone(), shared.clone());
            Decoder::start(codec, &el, move |f| {
                *shared.lock().expect("lock").extra_frames.entry(stream).or_default() += 1;
                if let Some(s) = sink.lock().expect("lock").as_ref() {
                    s(stream, f);
                }
            })
        }
    };

    let player = match &o.audio_out {
        Some(sink) => match AudioPlayer::start(sink, 2) {
            Ok(p) => Some(Arc::new(p)),
            Err(e) => {
                tracing::warn!("audio playback is off: {e:#}");
                None
            }
        },
        None => None,
    };
    let mic = match &o.mic {
        Some(src) => {
            let conn = conn.clone();
            let seq = std::sync::atomic::AtomicU32::new(0);
            let clock = qr_media::ClockMap::new(now_us);
            match AudioCapture::start(src, 1, 32_000, clock, move |data, ts_us| {
                let p =
                    AudioPacket { mic: true, seq: seq.fetch_add(1, std::sync::atomic::Ordering::Relaxed), ts_us, data };
                let _ = conn.send_datagram(Bytes::from(p.encode()));
            }) {
                Ok(m) => Some(m),
                Err(e) => {
                    tracing::warn!("microphone is off: {e:#}");
                    None
                }
            }
        }
        None => None,
    };

    // Media receive loop.
    {
        let (conn, shared, control, end_tx, player) =
            (conn.clone(), shared.clone(), control.clone(), end_tx.clone(), player.clone());
        tasks.push(tokio::spawn(async move {
            let _mic = mic;
            let main = (decoder, (codec, chroma444));
            let r = recv_loop(&conn, &shared, &control, main, make_main, player.as_deref(), extra_rx, make_extra).await;
            if let Some(p) = player {
                p.finish();
            }
            let _ = end_tx.send(Some(r.err().map_or("closed".into(), |e| e.to_string())));
        }));
    }
    // Control messages from other tasks and threads go through one writer.
    let (ctl_tx, mut ctl_rx) = tokio::sync::mpsc::unbounded_channel::<Msg>();
    {
        let control = control.clone();
        tasks.push(tokio::spawn(async move {
            while let Some(m) = ctl_rx.recv().await {
                if control.lock().await.send(&m).await.is_err() {
                    return;
                }
            }
        }));
    }
    let ctl: qr_net::files::Control = Arc::new(move |m| {
        let _ = ctl_tx.send(m);
    });
    let files = qr_net::files::Files::new(conn.clone(), o.recv_dir.clone(), 1);
    // Streams from the host (files).
    {
        let (conn, files, ctl) = (conn.clone(), files.clone(), ctl.clone());
        tasks.push(tokio::spawn(async move {
            while let Ok(mut st) = conn.accept_uni().await {
                let mut kind = [0u8; 1];
                if st.read_exact(&mut kind).await.is_err() || kind[0] != qr_proto::uni::FILE {
                    continue;
                }
                let (files, ctl) = (files.clone(), ctl.clone());
                tokio::spawn(async move {
                    if let Err(e) = files.receive(st, &ctl).await {
                        tracing::info!("file transfer failed: {e:#}");
                    }
                });
            }
        }));
    }

    let clip_tx = match &o.clipboard {
        Some(display) => match qr_clip::spawn(display) {
            Ok((tx, rx)) => {
                let ctl = ctl.clone();
                std::thread::spawn(move || {
                    use qr_proto::msg::ClipKind;
                    while let Ok(c) = rx.recv() {
                        ctl(match c {
                            qr_clip::Content::Text(t) => {
                                Msg::Clipboard { kind: ClipKind::Text as u8, data: t.into_bytes() }
                            }
                            qr_clip::Content::Png(p) => Msg::Clipboard { kind: ClipKind::Png as u8, data: p },
                        });
                    }
                });
                Some(tx)
            }
            Err(e) => {
                tracing::warn!("clipboard is off: {e:#}");
                None
            }
        },
        None => None,
    };

    let rumble: RumbleTx = Arc::default();
    let rumble2 = rumble.clone();
    // Control receive loop.
    {
        let (shared, end_tx, files, ctl) = (shared.clone(), end_tx.clone(), files.clone(), ctl.clone());
        let extra_tx = extra_tx.clone();
        tasks.push(tokio::spawn(async move {
            loop {
                let m = rx.recv().await;
                if let Ok(m) = &m
                    && files.on_control(m, &ctl)
                {
                    continue;
                }
                if let Ok(Msg::Rumble { idx, strong, weak, ms }) = &m {
                    tracing::info!(idx, strong, weak, ms, "rumble from the host");
                    if let Some(tx) = rumble2.lock().expect("lock").as_ref() {
                        let _ = tx.send((*idx, *strong, *weak, *ms));
                    }
                    continue;
                }
                if let Ok(Msg::Clipboard { kind, data }) = &m {
                    if let Some(tx) = &clip_tx {
                        let c = if *kind == qr_proto::msg::ClipKind::Png as u8 {
                            qr_clip::Content::Png(data.clone())
                        } else {
                            qr_clip::Content::Text(String::from_utf8_lossy(data).into_owned())
                        };
                        let _ = tx.send(qr_clip::Command::Set(c));
                    }
                    continue;
                }
                match m {
                    Ok(Msg::Pong { t_client, t_host, .. }) => {
                        shared.lock().expect("lock").clock.sample(t_client, t_host, now_us());
                    }
                    Ok(Msg::HostStats(h)) => {
                        let mut sh = shared.lock().expect("lock");
                        sh.stats.encoder = h.encoder.clone();
                        sh.stats.parity_pct = h.parity_pct;
                        sh.host = Some(h);
                    }
                    Ok(Msg::StreamStarted { stream, width, height, monitor, fps, codec, chroma444, .. })
                        if stream != 0 =>
                    {
                        tracing::info!(stream, monitor, width, height, fps, "extra stream started");
                        let mut sh = shared.lock().expect("lock");
                        let st = sh.stats.streams.entry(stream).or_default();
                        (st.monitor, st.width, st.height) = (monitor, width as u32, height as u32);
                        drop(sh);
                        let _ = extra_tx.send((stream, Codec::from_u8(codec).ok().map(|c| (c, chroma444))));
                    }
                    Ok(Msg::MonitorList { monitors }) => {
                        let names: Vec<String> =
                            monitors.iter().map(|m| format!("{}:{}:{}x{}", m.id, m.name, m.width, m.height)).collect();
                        tracing::info!(monitors = %names.join(" "), "host monitors");
                        shared.lock().expect("lock").monitors = monitors;
                    }
                    Ok(Msg::StreamStop { stream }) => {
                        shared.lock().expect("lock").stats.streams.remove(&stream);
                        let _ = extra_tx.send((stream, None));
                    }
                    Ok(Msg::StreamStarted { width, height, monitor, fps, codec, chroma444, .. }) => {
                        let _ = extra_tx.send((0, Codec::from_u8(codec).ok().map(|c| (c, chroma444))));
                        // A restarted stream (monitor switch, resize): it
                        // opens with a keyframe that carries the new size.
                        tracing::info!(monitor, width, height, fps, "stream restarted");
                        let mut sh = shared.lock().expect("lock");
                        sh.stats.monitor = monitor;
                        sh.stats.width = width as u32;
                        sh.stats.height = height as u32;
                    }
                    Ok(Msg::DisplayInfo { stream, kind, width, height, scale_pct, note }) => {
                        tracing::info!(stream, kind, width, height, scale_pct, note, "display info");
                        if stream == 0 {
                            shared.lock().expect("lock").display =
                                Some(DisplayState { kind, width, height, scale_pct, note });
                        }
                    }
                    Ok(Msg::Error { code, text }) => {
                        let replaced = code == qr_proto::msg::ErrorCode::Replaced as u16;
                        let _ = end_tx.send(Some(if replaced { format!("{REPLACED}{text}") } else { text }));
                        return;
                    }
                    Ok(_) => {}
                    Err(e) => {
                        let _ = end_tx.send(Some(e.to_string()));
                        return;
                    }
                }
            }
        }));
    }
    // Pings (200 ms), feedback (50 ms), stats (1 s).
    {
        let (shared, control, conn) = (shared.clone(), control.clone(), conn.clone());
        tasks.push(tokio::spawn(async move {
            let mut seq = 0u32;
            let mut tick = tokio::time::interval(Duration::from_millis(50));
            let mut last_stats = Instant::now();
            loop {
                tick.tick().await;
                seq = seq.wrapping_add(1);
                let mut msgs = Vec::new();
                if seq.is_multiple_of(4) {
                    msgs.push(Msg::Ping { seq, t_client: now_us() });
                }
                {
                    let mut sh = shared.lock().expect("lock");
                    msgs.push(Msg::Feedback(sh.take_feedback(seq)));
                    if last_stats.elapsed() >= Duration::from_secs(1) {
                        let secs = last_stats.elapsed().as_secs_f32();
                        last_stats = Instant::now();
                        sh.roll_stats(secs, conn.rtt());
                    }
                }
                let mut c = control.lock().await;
                for m in msgs {
                    if c.send(&m).await.is_err() {
                        return;
                    }
                }
            }
        }));
    }
    Ok(Connected {
        endpoint: ep,
        conn,
        control,
        shared,
        host_name: s.host_name,
        host_id: s.host_id,
        tasks,
        ended,
        input,
        files,
        ctl,
        rumble,
        extra_sink,
        host_minor,
    })
}

// One task's whole context; a struct would only rename the arguments.
#[allow(clippy::too_many_arguments)]
async fn recv_loop(
    conn: &quinn::Connection,
    shared: &Mutex<Shared>,
    control: &AsyncMutex<FramedSend>,
    (mut decoder, mut kind): (Decoder, (Codec, bool)),
    make_main: impl Fn(Codec, bool) -> Result<Decoder>,
    player: Option<&AudioPlayer>,
    mut extra_rx: tokio::sync::mpsc::UnboundedReceiver<(u8, Option<(Codec, bool)>)>,
    make_extra: impl Fn(u8, Codec, bool) -> Result<Decoder>,
) -> Result<()> {
    let frame_ms: f64 = 1000.0 / 60.0;
    let deadline = Duration::from_millis((2.0 * frame_ms).max(20.0) as u64);
    let mut reasm = Reassembler::new(0, deadline);
    // After a loss, deltas are useless until the next keyframe.
    let mut need_key = false;
    // Streams besides 0: (reassembler, decoder, need_key, codec and chroma).
    let mut extras: std::collections::HashMap<u8, (Reassembler, Decoder, bool, (Codec, bool))> = Default::default();
    let mut tick = tokio::time::interval(Duration::from_millis(5));
    loop {
        let events = tokio::select! {
            d = conn.read_datagram() => {
                let d: Bytes = d?;
                match d.first() {
                    Some(&kind::VIDEO) if d.get(1).is_some_and(|&s| s != 0) => {
                        if let Some((r, dec, nk, _)) = extras.get_mut(&d[1]) {
                            let ev = r.push(&d, qr_net::clock::base().elapsed()).unwrap_or_default();
                            extra_events(d[1], ev, dec, nk, control).await?;
                        }
                        Vec::new()
                    }
                    Some(&kind::VIDEO) => reasm.push(&d, qr_net::clock::base().elapsed()).unwrap_or_default(),
                    Some(&kind::AUDIO) => {
                        if let (Some(p), Ok(a)) = (player, AudioPacket::decode(&d)) {
                            p.push(a.seq, &a.data)?;
                            let mut sh = shared.lock().expect("lock");
                            if sh.clock.ready() {
                                let lat = now_us() as i64 - sh.clock.to_client(a.ts_us) + PLAYOUT_US as i64;
                                sh.audio_lat.0 += lat as f64;
                                sh.audio_lat.1 += 1;
                            }
                        }
                        Vec::new()
                    }
                    _ => Vec::new(),
                }
            }
            Some((id, started)) = extra_rx.recv() => {
                // A restart keeps the decoder unless the codec or chroma
                // changed; the frame numbering carries on either way.
                match started {
                    Some(k) if id == 0 => {
                        if k != kind {
                            tracing::info!(codec = k.0.name(), chroma444 = k.1, "new decoder for the restarted stream");
                            decoder = make_main(k.0, k.1)?;
                            kind = k;
                            need_key = true;
                        }
                    }
                    Some(k) if extras.get(&id).is_none_or(|e| e.3 != k) => match make_extra(id, k.0, k.1) {
                        Ok(dec) => {
                            let reasm = extras.remove(&id).map_or_else(|| Reassembler::new(id, deadline), |e| e.0);
                            extras.insert(id, (reasm, dec, true, k));
                        }
                        Err(e) => tracing::warn!("stream {id}: {e:#}"),
                    },
                    Some(_) => {}
                    None => {
                        extras.remove(&id);
                    }
                }
                Vec::new()
            }
            _ = tick.tick() => {
                let now = qr_net::clock::base().elapsed();
                for (id, (r, dec, nk, _)) in extras.iter_mut() {
                    let ev = r.poll(now);
                    extra_events(*id, ev, dec, nk, control).await?;
                    let st = r.take_stats();
                    if st != Default::default() {
                        add_stats(&mut shared.lock().expect("lock"), &st);
                    }
                }
                reasm.poll(now)
            }
        };
        for ev in events {
            match ev {
                Event::Frame(f) => {
                    let key = f.meta.flags & qr_proto::datagram::fflags::KEYFRAME != 0;
                    if need_key && !key {
                        continue;
                    }
                    need_key = false;
                    {
                        let mut sh = shared.lock().expect("lock");
                        let complete = f.completed.as_micros() as i64;
                        let off = |t: u64| sh.clock.to_client(t);
                        let t = FrameTimes {
                            capture: off(f.meta.capture_us),
                            encode_start: off(f.meta.encode_start_us),
                            encode_end: off(f.meta.encode_end_us),
                            send: off(f.meta.send_us),
                            complete,
                        };
                        // Raw transit (offset-free) for rate control.
                        sh.delays.push((complete, complete - f.meta.send_us as i64));
                        if sh.pending.len() > 64 {
                            sh.pending.clear();
                        }
                        sh.pending.insert(f.id, t);
                        sh.win.bytes += f.au.len() as u64;
                    }
                    decoder.push(f.id, &f.au, key)?;
                }
                Event::Lost { id } => {
                    need_key = true;
                    decoder.flush_pending();
                    control.lock().await.send(&Msg::FrameLost { stream: 0, frame: id }).await?;
                }
            }
        }
        let st = reasm.take_stats();
        if st != Default::default() {
            add_stats(&mut shared.lock().expect("lock"), &st);
        }
        if let Some(e) = decoder.error() {
            bail!("decoder failed: {e}");
        }
    }
}

/// Decodes an extra stream's frames (no latency bookkeeping).
async fn extra_events(
    stream: u8,
    events: Vec<Event>,
    decoder: &Decoder,
    need_key: &mut bool,
    control: &AsyncMutex<FramedSend>,
) -> Result<()> {
    for ev in events {
        match ev {
            Event::Frame(f) => {
                let key = f.meta.flags & qr_proto::datagram::fflags::KEYFRAME != 0;
                if *need_key && !key {
                    continue;
                }
                *need_key = false;
                decoder.push(f.id, &f.au, key)?;
            }
            Event::Lost { id } => {
                *need_key = true;
                decoder.flush_pending();
                control.lock().await.send(&Msg::FrameLost { stream, frame: id }).await?;
            }
        }
    }
    Ok(())
}

/// Adds reassembly statistics to the feedback and the stats window.
fn add_stats(sh: &mut Shared, st: &qr_fec::Stats) {
    sh.fb.bytes_rx += st.bytes_rx as u32;
    sh.fb.shards_rx += st.shards_rx;
    sh.fb.shards_lost += st.shards_lost;
    sh.fb.shards_repaired += st.shards_repaired;
    sh.fb.frames_rx += st.frames_rx as u16;
    sh.fb.frames_lost += st.frames_lost as u16;
    sh.win.shards_rx += st.shards_rx;
    sh.win.shards_lost += st.shards_lost;
    sh.stats.fec_repaired += st.shards_repaired;
    sh.stats.frames_lost += st.frames_lost;
}

impl Shared {
    fn record(&mut self, frame: u32, t: &FrameTimes, decoded: i64, decode_us: i64, work_us: u64) {
        // The slowest decode in the feedback window tells the host whether
        // this client keeps up (a picture held for the next frame is no
        // sign of a slow decoder: that would hold it longer at lower rates).
        self.fb.decode_us = self.fb.decode_us.max(work_us.min(u32::MAX as u64) as u32);
        let w = &mut self.win;
        w.frames += 1;
        if !self.clock.ready() {
            if self.keep_samples {
                self.samples.push(LatencySample {
                    at_us: decoded,
                    frame,
                    synced: false,
                    capture_us: 0,
                    encode_us: 0,
                    network_us: 0,
                    decode_us,
                    present_us: 0,
                    total_us: 0,
                });
            }
            return;
        }
        let s = LatencySample {
            at_us: decoded,
            frame,
            synced: true,
            capture_us: t.encode_start - t.capture,
            encode_us: t.encode_end - t.encode_start,
            network_us: t.complete - t.send,
            decode_us,
            present_us: 0,
            total_us: decoded - t.capture,
        };
        w.sum_capture += s.capture_us as f64;
        w.sum_encode += s.encode_us as f64;
        w.sum_network += s.network_us as f64;
        w.sum_decode += s.decode_us as f64;
        w.sum_total += s.total_us as f64;
        w.lat_n += 1;
        if self.keep_samples {
            self.samples.push(s);
        }
    }

    /// Adds the present step for the last frame (window mode).
    pub fn presented(&mut self, t: &FrameTimes, present_done: i64) {
        if self.clock.ready() {
            self.win.sum_present += (present_done - t.capture) as f64;
        }
    }

    fn take_feedback(&mut self, seq: u32) -> Feedback {
        let mut fb = std::mem::take(&mut self.fb);
        fb.seq = seq;
        fb.window_ms = 50;
        if !self.delays.is_empty() {
            let d: Vec<(i64, i64)> = std::mem::take(&mut self.delays);
            fb.delay_min_us = d.iter().map(|x| x.1).min().unwrap_or(0) as i32;
            fb.delay_max_us = d.iter().map(|x| x.1).max().unwrap_or(0) as i32;
            fb.delay_slope = slope(&d);
        }
        fb
    }

    fn roll_stats(&mut self, secs: f32, rtt: Duration) {
        let w = std::mem::take(&mut self.win);
        let s = &mut self.stats;
        s.fps = w.frames as f32 / secs;
        s.kbps = (w.bytes as f32 * 8.0 / secs / 1000.0) as u32;
        s.rtt_ms = rtt.as_secs_f32() * 1000.0;
        if self.audio_lat.1 > 0 {
            s.audio_ms = (self.audio_lat.0 / self.audio_lat.1 as f64 / 1000.0) as f32;
            self.audio_lat = (0.0, 0);
        }
        for (id, st) in s.streams.iter_mut() {
            st.fps = self.extra_frames.remove(id).unwrap_or(0) as f32 / secs;
        }
        self.extra_frames.clear();
        let shards = (w.shards_rx + w.shards_lost).max(1) as f32;
        s.loss_pct = w.shards_lost as f32 / shards * 100.0;
        if w.lat_n > 0 {
            let n = w.lat_n as f64;
            s.capture_ms = (w.sum_capture / n / 1000.0) as f32;
            s.encode_ms = (w.sum_encode / n / 1000.0) as f32;
            s.network_ms = (w.sum_network / n / 1000.0) as f32;
            s.decode_ms = (w.sum_decode / n / 1000.0) as f32;
            s.total_ms = (w.sum_total / n / 1000.0) as f32;
            if w.sum_present > 0.0 {
                s.present_ms = ((w.sum_present - w.sum_total) / n / 1000.0) as f32;
                s.total_ms = (w.sum_present / n / 1000.0) as f32;
            }
        }
    }
}

/// Least-squares slope of (t µs, delay µs) in µs per second.
fn slope(d: &[(i64, i64)]) -> i32 {
    if d.len() < 3 {
        return 0;
    }
    let n = d.len() as f64;
    let t0 = d[0].0 as f64;
    let (mut sx, mut sy, mut sxx, mut sxy) = (0.0, 0.0, 0.0, 0.0);
    for &(t, y) in d {
        let x = (t as f64 - t0) / 1e6;
        let y = y as f64;
        sx += x;
        sy += y;
        sxx += x * x;
        sxy += x * y;
    }
    let den = n * sxx - sx * sx;
    if den.abs() < 1e-12 { 0 } else { ((n * sxy - sx * sy) / den) as i32 }
}

/// Ends the session on purpose: `Bye`, then a close that reaches the host
/// before the process exits.
pub async fn bye(c: &Connected) {
    {
        let mut ctl = c.control.lock().await;
        let _ = ctl.send(&Msg::Bye).await;
        let _ = ctl.0.finish();
        let _ = tokio::time::timeout(Duration::from_millis(300), ctl.0.stopped()).await;
    }
    c.conn.close(0u32.into(), b"bye");
    let _ = tokio::time::timeout(Duration::from_secs(1), c.endpoint.wait_idle()).await;
}

/// The decoder element for `codec`: the override if given; for 4:4:4, one
/// that decodes it (often software: hardware decoders rarely do).
fn pick_decoder(decoders: &[DecoderInfo], ovr: Option<&str>, codec: Codec, chroma444: bool) -> Result<String> {
    if let Some(o) = ovr
        && !chroma444
    {
        return Ok(o.to_string());
    }
    decoders
        .iter()
        .filter(|d| d.codec == codec)
        .find(|d| d.chroma444 || !chroma444)
        .or_else(|| decoders.iter().find(|d| d.codec == codec))
        .map(|d| d.element.to_string())
        .ok_or_else(|| anyhow!("no decoder for {}", codec.name()))
}
