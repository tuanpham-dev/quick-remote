//! One client session: codec choice, streams, control messages, stats.

use std::net::IpAddr;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use anyhow::{Result, bail};
use qr_media::encode::Source;
use qr_media::probe;
use qr_net::session::{FramedSend, HostSession};
use qr_proto::msg::{ErrorCode, HostStats, Monitor, PathKind, Profile, mode};
use qr_proto::{CodecCap, Msg};
use tokio::sync::Mutex as AsyncMutex;

use crate::HostCtx;
use crate::video::Video;

fn is_private(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v) => v.is_private() || v.is_loopback() || v.is_link_local(),
        IpAddr::V6(v) => {
            v.is_loopback() || (v.segments()[0] & 0xfe00) == 0xfc00 || (v.segments()[0] & 0xffc0) == 0xfe80
        }
    }
}

/// LAN when the peer has a private address and the round trip is short.
pub fn pick_profile(forced: Option<Profile>, conn: &quinn::Connection, web: &crate::web::WebPeers) -> Profile {
    if let Some(p) = forced {
        return p;
    }
    // A browser behind the web gateway: judge by its own path.
    let (ip, rtt) = web
        .0
        .lock()
        .expect("lock")
        .get(&conn.remote_address().port())
        .filter(|_| conn.remote_address().ip().is_loopback())
        .map(|p| (p.addr.ip(), p.rtt))
        .unwrap_or((conn.remote_address().ip(), conn.rtt()));
    if is_private(ip) && rtt < Duration::from_millis(5) { Profile::Lan } else { Profile::Internet }
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

    let profile = pick_profile(ctx.args.profile(), &conn, &ctx.web_peers);
    let mode = ctx.display_mode(want_width, want_height, want_refresh);
    tracing::info!(client = %client_name, ?profile, encoder = enc.element, ?mode, "session start");
    send(
        &control_tx,
        &Msg::SessionStart {
            profile: profile as u8,
            path: if ctx.relays.lock().expect("lock").contains(&conn.remote_address()) {
                PathKind::Relayed
            } else if profile == Profile::Lan {
                PathKind::Lan
            } else {
                PathKind::Direct
            } as u8,
            modes: mode::AUDIO | mode::CLIPBOARD,
        },
    )
    .await?;
    send(&control_tx, &Msg::MonitorList { monitors: ctx.monitors() }).await?;

    // Control messages from background tasks (files, outbox, video) go
    // through a channel to one writer.
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

    // Browser sessions: video datagrams must fit WebTransport's.
    let max_datagram = ctx
        .web_peers
        .0
        .lock()
        .expect("lock")
        .get(&conn.remote_address().port())
        .filter(|_| conn.remote_address().ip().is_loopback())
        .map(|p| p.max_datagram);
    let enc444 = probe::choose_444(&allowed_encoders(&ctx), &decoders);
    let video = Arc::new(Video::new(ctx.clone(), conn.clone(), enc.clone(), enc444, profile, max_datagram, ctl.clone()));
    video.start(0, 0, mode).await?;
    let _audio = match crate::media::start_audio(&ctx, &conn) {
        Ok(a) => a,
        Err(e) => {
            tracing::warn!("audio is off: {e:#}");
            None
        }
    };
    ctx.input.reset();
    let dgram_task = tokio::spawn(crate::media::datagram_loop(ctx.clone(), conn.clone()));
    // The client reports every 50 ms. Half a second of silence is a stalled
    // link (QUIC rides out a few seconds): let go of held keys and buttons
    // rather than leave them auto-repeating until it comes back.
    let heard = Arc::new(std::sync::Mutex::new(std::time::Instant::now()));
    let watchdog = {
        let (ctx, heard) = (ctx.clone(), heard.clone());
        tokio::spawn(async move {
            let mut stalled = false;
            let mut tick = tokio::time::interval(Duration::from_millis(100));
            loop {
                tick.tick().await;
                let silent = heard.lock().expect("lock").elapsed() > Duration::from_millis(500);
                if silent && !stalled {
                    tracing::info!("the client went quiet: releasing held input");
                    ctx.input.release_all();
                }
                stalled = silent;
            }
        })
    };
    let files = qr_net::files::Files::new(conn.clone(), ctx.files_dir(), 2);
    let uni_task = tokio::spawn(uni_streams(ctx.clone(), conn.clone(), files.clone(), ctl.clone()));
    let outbox_task = tokio::spawn(crate::outbox::run(ctx.outbox_dir(), files.clone(), ctl.clone()));
    let clipboard = crate::clipboard::start(&ctx, ctl.clone());

    // Stats once a second.
    let stats_task = {
        let (video, tx, enc_name, conn2) = (video.clone(), control_tx.clone(), enc.element.to_string(), conn.clone());
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(1));
            tick.tick().await;
            loop {
                tick.tick().await;
                let Some(stream) = video.stream(0) else { continue };
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

    // Rumble from games back to the client's pads.
    let rumble_task = {
        let (ctx, ctl) = (ctx.clone(), ctl.clone());
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_millis(20));
            loop {
                tick.tick().await;
                for (idx, strong, weak) in ctx.input.injector.lock().expect("lock").take_rumble() {
                    ctl(Msg::Rumble { idx, strong, weak, ms: 200 });
                }
            }
        })
    };
    let result =
        control_loop(&ctx, &conn, &mut control_rx, &control_tx, &video, &files, &ctl, clipboard.as_ref(), &heard).await;
    watchdog.abort();
    ctl_writer.abort();
    rumble_task.abort();
    outbox_task.abort();
    stats_task.abort();
    dgram_task.abort();
    uni_task.abort();
    ctx.input.release_all();
    tracing::info!(client = %client_name, "session end: {}", result.as_ref().map_or_else(|e| e.to_string(), |_| "bye".into()));
    result
}

#[allow(clippy::too_many_arguments)]
async fn control_loop(
    ctx: &HostCtx,
    conn: &quinn::Connection,
    rx: &mut qr_net::session::FramedRecv,
    tx: &AsyncMutex<FramedSend>,
    video: &Arc<Video>,
    files: &Arc<qr_net::files::Files>,
    ctl: &qr_net::files::Control,
    clipboard: Option<&crate::clipboard::Clipboard>,
    heard: &std::sync::Mutex<std::time::Instant>,
) -> Result<()> {
    loop {
        let m = rx.recv().await?;
        *heard.lock().expect("lock") = std::time::Instant::now();
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
            // Feedback describes the link: every stream adapts to it.
            Msg::Feedback(fb) => {
                for s in video.all() {
                    s.feedback(&fb);
                }
            }
            Msg::PadDetach { idx } => ctx.input.pad_detach(idx),
            Msg::FrameLost { stream, .. } | Msg::KeyframeReq { stream } => {
                if let Some(s) = video.stream(stream) {
                    s.frame_lost(conn.rtt().max(Duration::from_millis(30)));
                }
            }
            Msg::SetMode { modes } => {
                let sharp = modes & qr_proto::msg::mode::SHARP_TEXT != 0;
                video.set_sharp(sharp).await;
                tracing::info!(sharp, "sharp-text mode");
            }
            // Switches a stream to another monitor, or opens one more stream
            // (a second window).
            Msg::SelectMonitor { stream, monitor } => {
                let fps = video.current(stream).or(video.current(0)).map_or(ctx.args.fps, |(_, m)| m.2);
                let Some(m) = ctx.monitors().into_iter().find(|m| m.id == monitor) else {
                    tracing::warn!(monitor, "no such monitor");
                    continue;
                };
                if let Err(e) = video.start(stream, monitor, (m.width as u32, m.height as u32, fps)).await {
                    tracing::warn!("starting stream {stream} on monitor {monitor} failed: {e:#}");
                }
            }
            Msg::StreamStop { stream } => video.stop(stream).await,
            // A virtual display follows the client's window; a real desktop
            // keeps its size and the client scales the picture.
            Msg::Resize { stream, width, height, refresh } if ctx.vd.is_some() && width >= 320 && height >= 200 => {
                let Some((_, (_, _, fps))) = video.current(stream) else { continue };
                let fps = if refresh > 0 { refresh as u32 } else { fps };
                let mode = (width.min(7680) as u32 & !1, height.min(4320) as u32 & !1, fps);
                video.request_resize(stream, mode);
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

/// The encoders `--encoder` / `--codec` allow.
fn allowed_encoders(ctx: &HostCtx) -> Vec<probe::EncoderInfo> {
    ctx.encoders
        .iter()
        .filter(|e| ctx.args.encoder == "auto" || ctx.args.encoder == e.element || e.chroma444)
        .filter(|e| ctx.args.codec == "auto" || ctx.args.codec == e.codec.name())
        .cloned()
        .collect()
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
    /// The Wayland socket captured through screencopy: the virtual
    /// display's, or a wlroots desktop's.
    pub fn screencopy_socket(&self) -> Option<std::path::PathBuf> {
        match &self.vd {
            Some(vd) => Some(vd.lock().expect("lock").wayland_socket()),
            None => self.wl_socket.clone(),
        }
    }

    /// The monitors a client can pick from.
    pub fn monitors(&self) -> Vec<Monitor> {
        if let Some(m) = &self.mutter {
            return m.monitors();
        }
        if let Some(socket) = &self.wl_socket {
            match crate::screencopy::monitors(socket, self.args.fps) {
                Ok(m) if !m.is_empty() => return m,
                Ok(_) => tracing::warn!("the Wayland desktop has no outputs"),
                Err(e) => tracing::warn!("listing the Wayland outputs: {e:#}"),
            }
        }
        if let Some(vd) = &self.vd {
            match vd.lock().expect("lock").monitors() {
                Ok(m) if !m.is_empty() => return m,
                Ok(_) => tracing::warn!("the virtual display has no outputs"),
                Err(e) => tracing::warn!("listing virtual outputs: {e:#}"),
            }
        }
        // An X display: its RandR monitors.
        if let Some(display) = self.args.source.strip_prefix("x11:") {
            match x11_monitors(display, self.args.fps) {
                Ok(m) if !m.is_empty() => return m,
                Ok(_) => {}
                Err(e) => tracing::warn!("listing the X monitors: {e:#}"),
            }
        }
        let (w, h, fps) = self.display_mode(0, 0, 0);
        vec![Monitor {
            id: 0,
            name: "screen".into(),
            width: w as u16,
            height: h as u16,
            refresh_mhz: fps * 1000,
            x: 0,
            y: 0,
            primary: true,
        }]
    }

    /// Capture source for a monitor of a non-virtual display.
    pub fn source_for(&self, mon: &Monitor) -> Source {
        if let Some(node) = self.mutter.as_ref().and_then(|m| m.node(mon.id)) {
            return Source::PipeWire { fd: None, node };
        }
        match self.source() {
            // One monitor of several: its rectangle of the screen.
            Source::X11 { display, .. } if self.monitors().len() > 1 => Source::X11 {
                display,
                rect: Some((mon.x, mon.y, mon.width as u32, mon.height as u32)),
            },
            s => s,
        }
    }

    pub fn source(&self) -> Source {
        match self.args.source.as_str() {
            "test" => Source::Test { pattern: "ball".into() },
            s if s.starts_with("x11:") => Source::X11 { display: s[4..].into(), rect: None },
            s if s.starts_with("d3d11") => {
                Source::D3d11 { monitor: s.split(':').nth(1).and_then(|m| m.parse().ok()).unwrap_or(0) }
            }
            other => Source::Test { pattern: other.trim_start_matches("test:").into() },
        }
    }
}

/// An X display's RandR monitors, or its Xinerama heads.
fn x11_monitors(display: &str, fps: u32) -> Result<Vec<Monitor>> {
    use x11rb::connection::Connection;
    use x11rb::protocol::randr::ConnectionExt as _;
    use x11rb::protocol::xproto::ConnectionExt as _;
    let (conn, screen) = x11rb::connect(Some(display))?;
    let root = conn.setup().roots[screen].root;
    let reply = conn.randr_get_monitors(root, true)?.reply()?;
    let mut out = Vec::new();
    for (i, m) in reply.monitors.iter().enumerate() {
        let name = conn
            .get_atom_name(m.name)?
            .reply()
            .map(|r| String::from_utf8_lossy(&r.name).into_owned())
            .unwrap_or_else(|_| format!("monitor-{i}"));
        out.push(Monitor {
            id: i as u8,
            name,
            width: m.width,
            height: m.height,
            refresh_mhz: fps * 1000,
            x: m.x as i32,
            y: m.y as i32,
            primary: m.primary,
        });
    }
    // One RandR monitor but several Xinerama heads (Xinerama setups, nested
    // servers): the heads are the monitors.
    if out.len() <= 1 {
        use x11rb::connection::RequestConnection as _;
        use x11rb::protocol::xinerama::ConnectionExt as _;
        if conn.extension_information(x11rb::protocol::xinerama::X11_EXTENSION_NAME)?.is_some()
            && conn.xinerama_is_active()?.reply()?.state != 0
        {
            let heads = conn.xinerama_query_screens()?.reply()?.screen_info;
            if heads.len() > 1 {
                return Ok(heads
                    .iter()
                    .enumerate()
                    .map(|(i, h)| Monitor {
                        id: i as u8,
                        name: format!("head-{i}"),
                        width: h.width,
                        height: h.height,
                        refresh_mhz: fps * 1000,
                        x: h.x_org as i32,
                        y: h.y_org as i32,
                        primary: i == 0,
                    })
                    .collect());
            }
        }
    }
    Ok(out)
}
