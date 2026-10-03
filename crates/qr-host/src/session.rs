//! One client session: codec choice, streams, control messages, stats.

use std::net::IpAddr;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use anyhow::{Result, bail};
use qr_media::encode::{EncodeConfig, Source};
use qr_media::probe;
use qr_net::session::{FramedSend, HostSession};
use qr_proto::msg::{ErrorCode, HostStats, Monitor, PathKind, Profile, mode};
use qr_proto::{CodecCap, Msg};
use tokio::sync::Mutex as AsyncMutex;

use crate::HostCtx;
use crate::ratectl::{Limits, RateCtl};
use crate::stream::VideoStream;

fn is_private(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v) => v.is_private() || v.is_loopback() || v.is_link_local(),
        IpAddr::V6(v) => {
            v.is_loopback() || (v.segments()[0] & 0xfe00) == 0xfc00 || (v.segments()[0] & 0xffc0) == 0xfe80
        }
    }
}

/// LAN when the peer has a private address and the round trip is short.
pub fn pick_profile(forced: Option<Profile>, conn: &quinn::Connection) -> Profile {
    if let Some(p) = forced {
        return p;
    }
    if is_private(conn.remote_address().ip()) && conn.rtt() < Duration::from_millis(5) {
        Profile::Lan
    } else {
        Profile::Internet
    }
}

pub async fn run(ctx: Arc<HostCtx>, conn: quinn::Connection, hs: HostSession) -> Result<()> {
    let HostSession { control_tx, mut control_rx, hello, client_name, .. } = hs;
    let control_tx = Arc::new(AsyncMutex::new(control_tx));
    let Msg::Hello { want_width, want_height, want_refresh, decoders, .. } = hello else { unreachable!() };

    let Some((enc, _cap)) = choose_encoder(&ctx, &decoders) else {
        let ours: Vec<&str> = ctx.encoders.iter().map(|e| e.codec.name()).collect();
        let theirs: Vec<&str> = decoders.iter().map(|c| c.codec.name()).collect();
        let text = format!("No common codec: the host encodes {ours:?}, the client decodes {theirs:?}.");
        send(&control_tx, &Msg::Error { code: ErrorCode::NoCommonCodec as u16, text: text.clone() }).await?;
        bail!(text);
    };

    let profile = pick_profile(ctx.args.profile(), &conn);
    let (width, height, fps) = ctx.display_mode(want_width, want_height, want_refresh);
    tracing::info!(client = %client_name, ?profile, encoder = enc.element, width, height, fps, "session start");
    send(
        &control_tx,
        &Msg::SessionStart {
            profile: profile as u8,
            path: if profile == Profile::Lan { PathKind::Lan } else { PathKind::Direct } as u8,
            modes: mode::AUDIO | mode::CLIPBOARD,
        },
    )
    .await?;
    send(
        &control_tx,
        &Msg::MonitorList {
            monitors: vec![Monitor {
                id: 0,
                name: "screen".into(),
                width: width as u16,
                height: height as u16,
                refresh_mhz: fps * 1000,
                x: 0,
                y: 0,
                primary: true,
            }],
        },
    )
    .await?;

    let limits = Limits { width, height, max_fps: fps, min_kbps: 300, max_kbps: ctx.args.max_kbps };
    let mut rate = RateCtl::new(profile, limits, Instant::now());
    if ctx.args.bitrate > 0 {
        rate.target_kbps = ctx.args.bitrate;
    }
    let cfg = EncodeConfig {
        encoder: enc.clone(),
        source: ctx.source(),
        width,
        height,
        fps,
        bitrate_kbps: rate.target_kbps,
        chroma444: false,
    };
    let stream = VideoStream::start(0, &cfg, rate, conn.clone())?;
    let _audio = match crate::media::start_audio(&ctx, &conn) {
        Ok(a) => a,
        Err(e) => {
            tracing::warn!("audio is off: {e:#}");
            None
        }
    };
    ctx.input.reset();
    let dgram_task = tokio::spawn(crate::media::datagram_loop(ctx.clone(), conn.clone()));
    // Control messages from background tasks (files, outbox) go through a
    // channel to one writer.
    let (ctl_tx, mut ctl_rx) = tokio::sync::mpsc::unbounded_channel::<Msg>();
    let ctl_writer = {
        let tx = control_tx.clone();
        tokio::spawn(async move {
            while let Some(m) = ctl_rx.recv().await {
                if send(&tx, &m).await.is_err() {
                    return;
                }
            }
        })
    };
    let ctl: qr_net::files::Control = Arc::new(move |m| {
        let _ = ctl_tx.send(m);
    });
    let files = qr_net::files::Files::new(conn.clone(), ctx.files_dir(), 2);
    let uni_task = tokio::spawn(uni_streams(ctx.clone(), conn.clone(), files.clone(), ctl.clone()));
    let outbox_task = tokio::spawn(crate::outbox::run(ctx.outbox_dir(), files.clone(), ctl.clone()));
    let clipboard = crate::clipboard::start(&ctx, ctl.clone());
    send(
        &control_tx,
        &Msg::StreamStarted {
            stream: 0,
            monitor: 0,
            codec: enc.codec as u8,
            chroma444: false,
            width: width as u16,
            height: height as u16,
            fps: fps as u16,
        },
    )
    .await?;

    // Stats once a second.
    let stats_task = {
        let (stream, tx, enc_name, conn2) = (stream.clone(), control_tx.clone(), enc.element.to_string(), conn.clone());
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(1));
            tick.tick().await;
            loop {
                tick.tick().await;
                let c = &stream.counters;
                let frames = c.frames_sent.swap(0, Ordering::Relaxed).max(1);
                let r = stream.rate.lock().expect("lock").fps;
                let es = stream.encoder.stats();
                let m = HostStats {
                    stream: 0,
                    bitrate_kbps: (c.bytes_sent.swap(0, Ordering::Relaxed) * 8 / 1000) as u32,
                    fps: frames as u16,
                    parity_pct: (stream.parity() * 100.0) as u8,
                    profile: profile as u8,
                    encoder: enc_name.clone(),
                    capture_us: (c.capture_to_encoded_us.swap(0, Ordering::Relaxed) / frames as u64) as u32,
                    encode_us: (c.encode_us.swap(0, Ordering::Relaxed) / frames as u64) as u32,
                    dropped_by_backpressure: es.skipped,
                };
                tracing::debug!(fps_cap = r, ?m, "host stats");
                let qs = conn2.stats();
                tracing::debug!(
                    cwnd = qs.path.cwnd,
                    lost = qs.path.lost_packets,
                    sent = qs.path.sent_packets,
                    congestion_events = qs.path.congestion_events,
                    datagrams = qs.frame_tx.datagram,
                    "quic stats"
                );
                if let Some(e) = stream.encoder.error() {
                    tracing::error!("capture/encode failed: {e}");
                    let _ = send(&tx, &Msg::Error { code: ErrorCode::CaptureFailed as u16, text: e }).await;
                    break;
                }
                if send(&tx, &Msg::HostStats(m)).await.is_err() {
                    break;
                }
            }
        })
    };

    let result = control_loop(&conn, &mut control_rx, &control_tx, &stream, &files, &ctl, clipboard.as_ref()).await;
    ctl_writer.abort();
    outbox_task.abort();
    stats_task.abort();
    dgram_task.abort();
    uni_task.abort();
    ctx.input.release_all();
    tracing::info!(client = %client_name, "session end: {}", result.as_ref().map_or_else(|e| e.to_string(), |_| "bye".into()));
    result
}

async fn control_loop(
    conn: &quinn::Connection,
    rx: &mut qr_net::session::FramedRecv,
    tx: &AsyncMutex<FramedSend>,
    stream: &Arc<VideoStream>,
    files: &Arc<qr_net::files::Files>,
    ctl: &qr_net::files::Control,
    clipboard: Option<&crate::clipboard::Clipboard>,
) -> Result<()> {
    loop {
        let m = rx.recv().await?;
        if files.on_control(&m, ctl) {
            continue;
        }
        if let Msg::Clipboard { kind, data } = &m {
            if let Some(c) = clipboard {
                crate::clipboard::from_client(c, *kind, data);
            }
            continue;
        }
        match m {
            Msg::Ping { seq, t_client } => {
                send(tx, &Msg::Pong { seq, t_client, t_host: qr_net::clock::now_us() }).await?;
            }
            Msg::Feedback(fb) => stream.feedback(&fb),
            Msg::FrameLost { .. } | Msg::KeyframeReq { .. } => {
                stream.frame_lost(conn.rtt().max(Duration::from_millis(30)));
            }
            Msg::Bye => return Ok(()),
            other => tracing::debug!(?other, "unhandled control message"),
        }
    }
}

/// Accepts the client's unidirectional streams: input and files.
async fn uni_streams(
    ctx: Arc<HostCtx>,
    conn: quinn::Connection,
    files: Arc<qr_net::files::Files>,
    ctl: qr_net::files::Control,
) {
    while let Ok(mut st) = conn.accept_uni().await {
        let mut kind = [0u8; 1];
        if st.read_exact(&mut kind).await.is_err() {
            continue;
        }
        match kind[0] {
            qr_proto::uni::INPUT => {
                let ctx = ctx.clone();
                tokio::spawn(async move {
                    let mut rx = qr_net::session::FramedRecv(st);
                    while let Ok(Some(body)) = rx.recv_raw().await {
                        match qr_proto::InputEvent::decode(&body) {
                            Ok(ev) => ctx.input.event(&ev),
                            Err(e) => tracing::debug!("bad input event: {e}"),
                        }
                    }
                    // The stream ended (or broke): nothing stays pressed.
                    ctx.input.release_all();
                });
            }
            qr_proto::uni::FILE => {
                let (files, ctl) = (files.clone(), ctl.clone());
                tokio::spawn(async move {
                    if let Err(e) = files.receive(st, &ctl).await {
                        tracing::info!("file transfer failed: {e:#}");
                    }
                });
            }
            other => tracing::debug!(other, "unknown stream type"),
        }
    }
}

async fn send(tx: &AsyncMutex<FramedSend>, m: &Msg) -> Result<()> {
    tx.lock().await.send(m).await
}

fn choose_encoder(ctx: &HostCtx, decoders: &[CodecCap]) -> Option<(probe::EncoderInfo, CodecCap)> {
    let allowed: Vec<probe::EncoderInfo> = ctx
        .encoders
        .iter()
        .filter(|e| ctx.args.encoder == "auto" || ctx.args.encoder == e.element)
        .filter(|e| ctx.args.codec == "auto" || ctx.args.codec == e.codec.name())
        .cloned()
        .collect();
    probe::choose(&allowed, decoders)
}

impl HostCtx {
    pub fn source(&self) -> Source {
        match self.args.source.as_str() {
            "test" => Source::Test { pattern: "ball".into() },
            s if s.starts_with("x11:") => Source::X11 { display: s[4..].into(), rect: None },
            other => Source::Test { pattern: other.trim_start_matches("test:").into() },
        }
    }
}
