//! QuickRemote host: shares this machine's screen (or a virtual display)
//! with paired clients.

mod clipboard;
mod cursor;
mod display;
#[cfg(target_os = "linux")]
mod gpuconv;
mod input;
#[cfg(target_os = "linux")]
mod kwin;
mod media;
#[cfg(target_os = "linux")]
mod mutter;
mod outbox;
#[cfg(target_os = "linux")]
mod portal;
mod ratectl;
#[cfg(target_os = "linux")]
mod screencopy;
mod session;
mod stream;
#[cfg(target_os = "linux")]
mod supervisor;
#[cfg(target_os = "linux")]
#[cfg(target_os = "linux")]
mod uinput;
#[cfg(target_os = "linux")]
mod vdisplay;
mod video;
mod web;
#[cfg(windows)]
mod winservice;
#[cfg(target_os = "linux")]
mod wlclip;
#[cfg(target_os = "linux")]
mod wlinput;
#[cfg(target_os = "linux")]
mod wlkeymap;
mod ws;
#[cfg(target_os = "linux")]
mod xdesktop;
// The Linux-only sources' stand-ins elsewhere.
#[cfg(not(target_os = "linux"))]
mod nolinux;
#[cfg(not(target_os = "linux"))]
use nolinux::{kwin, mutter, screencopy, vdisplay, xdesktop};

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use clap::Parser;
use qr_media::probe::{self, EncoderInfo};
use qr_net::discovery::Advertiser;
use qr_net::keystore::KeyStore;
use qr_net::pairing::{PinLimiter, new_pin};
use qr_net::session::{HostConfig, host_handshake};
use qr_net::{Identity, endpoint, identity};
use qr_proto::msg::ErrorCode;
use qr_proto::{ALPN, Msg};

#[derive(Parser, Debug)]
#[command(name = "qr-host", version, about = "Share this machine with QuickRemote clients")]
pub struct Args {
    /// UDP address to accept connections on.
    #[arg(long, default_value = "0.0.0.0:9450")]
    listen: SocketAddr,
    /// Name shown to clients (default: the hostname).
    #[arg(long)]
    name: Option<String>,
    /// Directory for the identity and paired clients
    /// (default ~/.config/quickremote/host).
    #[arg(long)]
    config_dir: Option<PathBuf>,
    /// What to stream: `auto` (this session's desktop: Wayland, else X11,
    /// else a virtual display), `wayland[:socket]` (wlroots, KDE or GNOME,
    /// detected), `mutter`, `kwin`, `x11:<display>`, `vdisplay[:command]`,
    /// `d3d11` (Windows), or `test[:<pattern>]`.
    #[arg(long, default_value = "auto")]
    source: String,
    /// Picture size for test and virtual-display sources.
    #[arg(long, default_value = "1920x1080")]
    size: String,
    /// Frame rate cap.
    #[arg(long, default_value_t = 60)]
    fps: u32,
    /// Fixed starting bitrate in kbit/s (0: the default for the size).
    #[arg(long, default_value_t = 0)]
    bitrate: u32,
    /// Upper bitrate limit in kbit/s.
    #[arg(long, default_value_t = 150_000)]
    max_kbps: u32,
    /// Encoder element (`auto`, or e.g. `vah264lpenc`, `x264enc`).
    #[arg(long, default_value = "auto")]
    encoder: String,
    /// Codec (`auto`, `h264`, `h265`, `av1`).
    #[arg(long, default_value = "auto")]
    codec: String,
    /// No longer used (one rate controller serves every path); accepted so
    /// existing scripts keep working.
    #[arg(long, hide = true)]
    profile: Option<String>,
    /// Serve the web client here (TCP for the page, UDP for WebTransport).
    #[arg(long)]
    web_listen: Option<SocketAddr>,
    /// Serve viewers over a plain WebSocket here (`/ws`, plus `/status`),
    /// for browsers that reach this host only over TCP (the Perch Remote
    /// Desktop extension). Loopback addresses only: there is no pairing on
    /// this transport. Several viewers share one session (see
    /// docs/protocol.md).
    #[arg(long)]
    ws_listen: Option<SocketAddr>,
    /// With `--source vdisplay`: run this X11 desktop command (for example
    /// `xfce4-session`) in a rootful Xwayland on the virtual display. The X
    /// screen and the desktop's scale follow the display's size and scale.
    #[arg(long)]
    desktop: Option<String>,
    /// XKB layout for the virtual display's keyboard (keys are sent by
    /// physical position), e.g. `us`, `de`.
    #[arg(long)]
    keyboard_layout: Option<String>,
    /// Font DPI told to the managed desktop (`--desktop`).
    #[arg(long, default_value_t = 96)]
    dpi: u32,
    /// Rendezvous server (`host:port`) for access from the internet.
    #[arg(long)]
    rendezvous: Option<String>,
    /// Use this PIN instead of a random one (tests).
    #[arg(long)]
    pin: Option<String>,
    /// Don't advertise on the LAN.
    #[arg(long)]
    no_mdns: bool,
    /// Print the working encoders and exit.
    #[arg(long)]
    probe: bool,
    /// System service mode (root): keep a host running in the active
    /// session (the user's desktop, or the login screen). The other options
    /// are passed on to it.
    #[arg(long)]
    supervise: bool,
    /// Windows: run as the service (the service manager passes this).
    #[arg(long)]
    service: bool,
    /// Windows: register the service (starts with Windows, runs the host in
    /// the active session as SYSTEM); the other options are passed on.
    #[arg(long)]
    install_service: bool,
    /// Windows: remove the service.
    #[arg(long)]
    uninstall_service: bool,
    /// Audio to send: `auto` (what the desktop plays), `none`,
    /// `test:<wave>`, `pipewire:<target>`, `pulse[:<device>]`, `default`
    /// (the default input).
    #[arg(long, default_value = "auto")]
    audio: String,
    /// Where the client's microphone goes: `auto` (the virtual display's
    /// microphone, else nowhere), `none`, `file:<wav>`, `pipewire:<target>`.
    #[arg(long, default_value = "auto")]
    mic_out: String,
    /// Where files from clients are saved (default ~/Downloads/QuickRemote).
    #[arg(long)]
    files_dir: Option<PathBuf>,
    /// Files put here are sent to the connected client, then removed
    /// (default ~/QuickRemote/Outbox).
    #[arg(long)]
    outbox: Option<PathBuf>,
    /// Render the virtual display in software (pixman) instead of on the GPU.
    #[arg(long)]
    vd_software: bool,
    /// Number of virtual monitors, side by side, all at `--size`.
    #[arg(long, default_value_t = 1)]
    vd_outputs: u8,
    /// Input backend: `auto` (from the source), `none`, `xtest:<display>`.
    #[arg(long, default_value = "auto")]
    input: String,
}

impl Args {
    fn size(&self) -> (u32, u32) {
        self.size.split_once('x').and_then(|(w, h)| Some((w.parse().ok()?, h.parse().ok()?))).unwrap_or((1920, 1080))
    }
}

/// The session currently in control.
struct Active {
    client: qr_net::Fingerprint,
    conn: quinn::Connection,
    done: tokio::sync::watch::Receiver<()>,
    /// Tells the session it was replaced, so it says so before closing.
    kick: Option<tokio::sync::oneshot::Sender<()>>,
}

pub struct HostCtx {
    pub args: Args,
    pub encoders: Vec<EncoderInfo>,
    pub input: input::Input,
    /// Relay ports allocated for this host: sessions from them are relayed.
    pub relays: Arc<Mutex<Vec<SocketAddr>>>,
    /// Browser sessions piped through the web gateway.
    pub web_peers: Arc<web::WebPeers>,
    /// The virtual display, for `--source vdisplay`.
    pub vd: Option<Mutex<vdisplay::VirtualDisplay>>,
    /// The GNOME desktop's remote-desktop session, for `--source mutter`.
    pub mutter: Option<mutter::MutterSession>,
    /// The KDE desktop's screencast and fake input, for `--source kwin`.
    pub kwin: Option<kwin::KwinSession>,
    /// Clients' virtual monitors (KDE, GNOME), kept a minute past their
    /// session.
    pub displays: display::Registry,
    /// A wlroots desktop's Wayland socket (screencopy and virtual input),
    /// for `--source wayland`.
    pub wl_socket: Option<PathBuf>,
    /// The managed X11 desktop on the virtual display (`--desktop`).
    pub xdesk: Option<Mutex<xdesktop::XDesktop>>,
    /// Pointer shapes, where the capture leaves the pointer out (X11
    /// displays, the managed desktop).
    pub cursor: Option<Arc<cursor::CursorService>>,
    /// Whether the pointer is drawn into the picture (no shapes).
    pub cursor_in_picture: bool,
}

impl HostCtx {
    /// Environment for a program to start on the managed desktop now
    /// (its display and scale); empty without one.
    pub fn desktop_env(&self) -> Vec<(String, String)> {
        self.xdesk.as_ref().map(|x| x.lock().expect("lock").env()).unwrap_or_default()
    }
}

impl HostCtx {
    pub fn files_dir(&self) -> PathBuf {
        self.args.files_dir.clone().unwrap_or_else(|| dirs_home().join("Downloads").join("QuickRemote"))
    }

    pub fn outbox_dir(&self) -> PathBuf {
        self.args.outbox.clone().unwrap_or_else(|| dirs_home().join("QuickRemote").join("Outbox"))
    }

    /// The mode a stream starts at: a virtual display takes the client's
    /// request (up to 4K120); real desktops keep their own size.
    pub fn display_mode(&self, w: u16, h: u16, refresh: u16) -> (u32, u32, u32) {
        if (self.mutter.is_some() || self.kwin.is_some() || self.wl_socket.is_some())
            && let Some(m) = self.monitors().into_iter().next()
        {
            return (m.width as u32, m.height as u32, self.args.fps);
        }
        if self.vd.is_some() {
            let (dw, dh) = self.args.size();
            let (w, h) =
                if w >= 320 && h >= 200 { (w.min(3840) as u32 & !1, h.min(2160) as u32 & !1) } else { (dw, dh) };
            let fps = if refresh > 0 { (refresh as u32).min(120) } else { self.args.fps };
            return (w, h, fps);
        }
        if let Some(display) = self.args.source.strip_prefix("x11:")
            && let Ok((w, h)) = x11_size(display)
        {
            return (w, h, self.args.fps);
        }
        let (w, h) = self.args.size();
        (w, h, self.args.fps)
    }
}

/// Size of an X display's first screen.
fn x11_size(display: &str) -> Result<(u32, u32)> {
    use x11rb::connection::Connection;
    let (conn, screen) = x11rb::connect(Some(display))?;
    let s = &conn.setup().roots[screen];
    Ok((s.width_in_pixels as u32, s.height_in_pixels as u32))
}

/// `auto`: the desktop this host was started in.
fn resolve_source(s: &str) -> String {
    if s != "auto" {
        return s.to_string();
    }
    if cfg!(windows) {
        return "d3d11".into();
    }
    if std::env::var_os("WAYLAND_DISPLAY").is_some() {
        return "wayland".into();
    }
    if let Ok(d) = std::env::var("DISPLAY") {
        return format!("x11:{d}");
    }
    // No desktop (a server): a virtual one.
    "vdisplay".into()
}

/// A Wayland socket path: `name`, else `WAYLAND_DISPLAY`, under
/// `XDG_RUNTIME_DIR` unless absolute.
fn wayland_socket(name: Option<&str>) -> Result<PathBuf> {
    let name = name.map(str::to_string).or_else(|| std::env::var("WAYLAND_DISPLAY").ok());
    let name = name.context("no Wayland display: set WAYLAND_DISPLAY or use --source wayland:<socket>")?;
    Ok(if name.starts_with('/') {
        PathBuf::from(name)
    } else {
        PathBuf::from(std::env::var("XDG_RUNTIME_DIR").context("XDG_RUNTIME_DIR is not set")?).join(name)
    })
}

/// SIGTERM (systemd, `kill`) or SIGINT (Ctrl+C).
async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        let (Ok(mut term), Ok(mut int)) = (signal(SignalKind::terminate()), signal(SignalKind::interrupt())) else {
            return std::future::pending().await;
        };
        tokio::select! {
            _ = term.recv() => {}
            _ = int.recv() => {}
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

fn dirs_home() -> PathBuf {
    std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("."))
}

fn hostname() -> String {
    std::fs::read_to_string("/etc/hostname").map(|s| s.trim().to_string()).unwrap_or_else(|_| "host".into())
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();
    let mut args = Args::parse();
    if args.profile.is_some() {
        tracing::warn!("the --profile option has no effect: one rate controller serves every connection");
    }
    // Service modes pass everything else on to the host they run, except
    // --source (they choose it).
    let forward = || {
        let mut forward = Vec::new();
        let mut it = std::env::args().skip(1);
        while let Some(a) = it.next() {
            match a.as_str() {
                "--supervise" | "--service" | "--install-service" | "--uninstall-service" => {}
                "--source" => {
                    it.next();
                }
                _ if a.starts_with("--source=") => {}
                _ => forward.push(a),
            }
        }
        forward
    };
    #[cfg(target_os = "linux")]
    if args.supervise {
        return supervisor::run(forward()).await;
    }
    #[cfg(windows)]
    {
        if args.install_service {
            return winservice::install(&forward());
        }
        if args.uninstall_service {
            return winservice::uninstall();
        }
        if args.service {
            let f = forward();
            return tokio::task::spawn_blocking(move || winservice::run(f)).await?;
        }
    }
    #[cfg(not(windows))]
    if args.service || args.install_service || args.uninstall_service {
        anyhow::bail!("--service, --install-service and --uninstall-service are for Windows; on Linux use --supervise");
    }
    args.source = resolve_source(&args.source);
    let args_ws_listen = args.ws_listen;
    if let Some(w) = args_ws_listen
        && !w.ip().is_loopback()
    {
        eprintln!("--ws-listen must be a loopback address (127.0.0.1 or ::1): the WebSocket transport has no pairing");
        std::process::exit(2);
    }
    qr_net::init_crypto();
    qr_net::trace::from_env();
    qr_media::init()?;

    let encoders = tokio::task::spawn_blocking(probe::encoders).await??;
    if args.probe {
        for e in &encoders {
            println!("{:14} {:5} {} 4:4:4={}", e.element, e.codec.name(), if e.hw { "hw" } else { "sw" }, e.chroma444);
        }
        return Ok(());
    }
    anyhow::ensure!(!encoders.is_empty(), "no working video encoder found (install x264 or a GPU driver)");

    let name = args.name.clone().unwrap_or_else(hostname);
    let dir = args.config_dir.clone().unwrap_or_else(|| identity::default_dir("host"));
    let id = Identity::load_or_create(&dir, &name).context("loading the host identity")?;
    let keystore = Arc::new(Mutex::new(KeyStore::open(&dir.join("paired.json"))?));
    let pin = Arc::new(Mutex::new(args.pin.clone().unwrap_or_else(new_pin)));
    let limiter = Arc::new(Mutex::new(PinLimiter::default()));

    let (ep, raw) = endpoint::endpoint_with_raw(args.listen, &id, true, &[ALPN])?;
    let addr = ep.local_addr()?;
    let relays: Arc<Mutex<Vec<SocketAddr>>> = Arc::default();
    if let Some(server) = &args.rendezvous {
        let server: SocketAddr =
            tokio::net::lookup_host(server).await?.next().with_context(|| format!("resolving {server}"))?;
        let (reg_tx, mut reg_rx) = tokio::sync::watch::channel(None);
        tokio::spawn(qr_net::rendezvous::host_loop(ep.clone(), raw, server, id.clone(), relays.clone(), reg_tx));
        tokio::spawn(async move {
            while reg_rx.changed().await.is_ok() {
                if let Some(observed) = reg_rx.borrow().clone() {
                    println!("REGISTERED {observed}");
                }
            }
        });
    }
    let _mdns = if args.no_mdns {
        None
    } else {
        match Advertiser::start(&name, &id.host_id(), std::env::consts::OS, addr.port(), 0) {
            Ok(a) => Some(a),
            Err(e) => {
                tracing::warn!("LAN discovery is off: {e}");
                None
            }
        }
    };
    println!("QuickRemote host \"{name}\"");
    println!("HOST-ID {}", id.host_id());
    println!("LISTEN {addr}");
    println!("PIN {}", pin.lock().expect("lock"));
    for e in &encoders {
        tracing::info!(
            element = e.element,
            codec = e.codec.name(),
            hw = e.hw,
            max = format!("{}x{}", e.max_width, e.max_height),
            "encoder"
        );
    }

    #[cfg(target_os = "linux")]
    if let Some(l) = &args.keyboard_layout {
        wlinput::set_layout(l);
    }
    if args.desktop.is_some() && !args.source.starts_with("vdisplay") {
        anyhow::bail!("--desktop runs on the virtual display: use it with --source vdisplay");
    }
    let vd = match args.source.strip_prefix("vdisplay") {
        Some(rest) => {
            let (w, h) = args.size();
            let desktop = rest.strip_prefix(':').filter(|d| !d.is_empty());
            let d = vdisplay::VirtualDisplay::start(
                w,
                h,
                args.fps,
                desktop,
                !args.vd_software,
                args.keyboard_layout.as_deref(),
            )
            .context("starting the virtual display")?;
            for _ in 1..args.vd_outputs {
                d.add_output(w, h, args.fps).context("adding a virtual monitor")?;
            }
            tracing::info!(runtime = %d.runtime.display(), outputs = args.vd_outputs, renderer = if d.gpu { "GPU (gles2)" } else { "software (pixman)" }, "virtual display up");
            Some(Mutex::new(d))
        }
        None => None,
    };
    // The managed X11 desktop, and the pointer shapes: forwarded where the
    // capture leaves the pointer out (X11 displays, the managed desktop),
    // drawn into the picture everywhere else.
    let xdesk = match (&vd, &args.desktop) {
        (Some(vd), Some(cmd)) => Some(Mutex::new(
            xdesktop::XDesktop::start(&vd.lock().expect("lock"), cmd, args.keyboard_layout.as_deref(), args.dpi)
                .context("starting the managed desktop")?,
        )),
        _ => None,
    };
    let cursor_display = match (&xdesk, args.source.strip_prefix("x11:")) {
        (Some(x), _) => Some(x.lock().expect("lock").display.clone()),
        (None, Some(d)) => Some(d.to_string()),
        _ => None,
    };
    let cursor = cursor_display.and_then(|d| match cursor::CursorService::start(Some(&d), xdesk.is_some()) {
        Ok(c) => Some(Arc::new(c)),
        Err(e) => {
            tracing::warn!("cursor shapes stay in the picture: {e:#}");
            None
        }
    });
    let cursor_in_picture = cursor.is_none();
    // A Wayland desktop: wlroots compositors through screencopy and the
    // virtual input protocols, GNOME through mutter's remote-desktop API,
    // KDE through KWin's screencast and fake input.
    let mut wl_socket = None;
    let mut kwin_socket = None;
    let mut use_mutter = args.source == "mutter";
    if args.source == "kwin" {
        kwin_socket = Some(wayland_socket(None)?);
    }
    if let Some(rest) = args.source.strip_prefix("wayland") {
        let socket = wayland_socket(rest.strip_prefix(':'))?;
        if screencopy::supported(&socket) {
            tracing::info!(socket = %socket.display(), "wlroots desktop");
            wl_socket = Some(socket);
        } else if kwin::supported(&socket) {
            kwin_socket = Some(socket);
        } else {
            use_mutter = true;
        }
    }
    let mutter = if use_mutter {
        Some(mutter::MutterSession::start().await.context(
            "this desktop offers neither wlr-screencopy, KWin's screencast nor GNOME's remote desktop \
             (on KDE, install deploy/kde/quickremote-host.desktop)",
        )?)
    } else {
        None
    };
    let kwin = match &kwin_socket {
        Some(s) => {
            tracing::info!(socket = %s.display(), "KDE desktop");
            Some(kwin::KwinSession::start(s, args.fps).context("starting KWin's screencast")?)
        }
        None => None,
    };
    let vd_socket = vd.as_ref().map(|d| d.lock().expect("lock").wayland_socket()).or(wl_socket.clone());
    let injector = match &mutter {
        // GNOME: input through the remote-desktop session (pads and pen
        // through uinput where allowed).
        Some(m) if args.input == "auto" => {
            let main: Box<dyn input::Injector> = Box::new(m.injector()?);
            input::with_devices(main, "mutter+uinput")
        }
        _ => match &kwin {
            Some(k) if args.input == "auto" => match k.injector() {
                Ok(i) => input::with_devices(Box::new(i), "kwin+uinput"),
                Err(e) => {
                    tracing::warn!("{e:#}");
                    Box::new(input::NoInput)
                }
            },
            _ => input::open(&args.input, &args.source, vd_socket.as_deref()).context("opening the input backend")?,
        },
    };
    // The managed X desktop: pointer, buttons, wheel and text through
    // XTest on its own display.
    let injector = match (&xdesk, args.input.as_str()) {
        (Some(x), "auto") => {
            let x = x.lock().expect("lock");
            input::with_x_desktop(injector, &x.display, x.scale_handle())
        }
        _ => injector,
    };
    tracing::info!(backend = injector.name(), "input");
    let web_peers: Arc<web::WebPeers> = Arc::default();
    if let Some(wl) = args.web_listen {
        let mut names = vec!["localhost".to_string(), wl.ip().to_string()];
        names.extend(
            qr_net::rendezvous::local_candidates(0)
                .iter()
                .filter_map(|a| a.parse::<SocketAddr>().ok())
                .map(|a| a.ip().to_string()),
        );
        let cert = web::wt_cert(names.clone())?;
        let page_cert = web::page_cert(&dir, names.clone())?;
        let hash = base64::Engine::encode(&base64::engine::general_purpose::URL_SAFE_NO_PAD, cert.hash);
        let host_addr =
            SocketAddr::new(if addr.ip().is_unspecified() { "127.0.0.1".parse()? } else { addr.ip() }, addr.port());
        let gw = Arc::new(web::Gateway {
            id: web::gateway_identity(&dir, &keystore)?,
            cert: std::sync::RwLock::new(cert),
            names,
            page_cert,
            host_addr,
            web_keys: Mutex::new(KeyStore::open(&dir.join("web-paired.json"))?),
            pin: pin.clone(),
            fixed_pin: args.pin.clone(),
            limiter: limiter.clone(),
            peers: web_peers.clone(),
        });
        let scheme = if wl.ip().is_loopback() { "http" } else { "https" };
        let shown = if wl.ip().is_unspecified() {
            qr_net::rendezvous::local_candidates(wl.port()).first().cloned().unwrap_or(wl.to_string())
        } else {
            wl.to_string()
        };
        println!("WEB-URL {scheme}://{shown}/#c={hash}");
        tokio::spawn(async move {
            if let Err(e) = web::run(gw, wl).await {
                tracing::error!("web client server: {e:#}");
            }
        });
    }
    let ctx = Arc::new(HostCtx {
        args,
        encoders,
        input: input::Input::new(injector),
        relays,
        web_peers,
        vd,
        mutter,
        kwin,
        displays: Default::default(),
        wl_socket,
        xdesk,
        cursor,
        cursor_in_picture,
    });
    if let Some(wsl) = args_ws_listen {
        let gw_id = web::gateway_identity(&dir, &keystore)?;
        let hub = ws::Hub::new(ctx.clone(), gw_id, addr);
        tokio::spawn(async move {
            if let Err(e) = ws::run(hub, wsl).await {
                tracing::error!("WebSocket viewers: {e:#}");
                std::process::exit(2);
            }
        });
    }
    let active: Arc<tokio::sync::Mutex<Option<Active>>> = Arc::default();
    // A managed display or desktop whose process dies (sway, Xwayland, the
    // session) leaves nothing to stream: say what died and stop, so that
    // whatever started this host (Perch's panel) can start it again.
    if ctx.vd.is_some() || ctx.xdesk.is_some() {
        let ctx = ctx.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(std::time::Duration::from_secs(1));
            loop {
                tick.tick().await;
                let gone = ctx
                    .xdesk
                    .as_ref()
                    .and_then(|x| x.lock().expect("lock").exited().map(|e| format!("managed desktop: {e}")))
                    .or_else(|| {
                        ctx.vd
                            .as_ref()
                            .and_then(|v| v.lock().expect("lock").exited().map(|e| format!("virtual display: {e}")))
                    });
                if let Some(e) = gone {
                    tracing::error!("{e}; stopping");
                    if let Some(x) = &ctx.xdesk {
                        x.lock().expect("lock").shutdown();
                    }
                    if let Some(vd) = &ctx.vd {
                        vd.lock().expect("lock").shutdown();
                    }
                    std::process::exit(3);
                }
            }
        });
    }
    // Stopped with SIGTERM or Ctrl+C: end what this host started (the
    // virtual display and its desktop) before exiting.
    {
        let ctx = ctx.clone();
        tokio::spawn(async move {
            shutdown_signal().await;
            tracing::info!("stopping");
            if let Some(x) = &ctx.xdesk {
                x.lock().expect("lock").shutdown();
            }
            if let Some(vd) = &ctx.vd {
                vd.lock().expect("lock").shutdown();
            }
            std::process::exit(0);
        });
    }
    while let Some(incoming) = ep.accept().await {
        let (ctx, id, keystore, pin, limiter, active, name) =
            (ctx.clone(), id.clone(), keystore.clone(), pin.clone(), limiter.clone(), active.clone(), name.clone());
        tokio::spawn(async move {
            let conn = match incoming.await {
                Ok(c) => c,
                Err(e) => {
                    tracing::debug!("handshake failed: {e}");
                    return;
                }
            };
            let peer = conn.remote_address();
            let rv = ctx.args.rendezvous.clone().unwrap_or_default();
            let cfg = HostConfig { id: &id, name: &name, rendezvous: &rv };
            let current_pin = pin.lock().expect("lock").clone();
            let mut hs = match host_handshake(&conn, &cfg, &keystore, &current_pin, &limiter).await {
                Ok(hs) => hs,
                Err(e) => {
                    tracing::info!(%peer, "refused: {e:#}");
                    return;
                }
            };
            if hs.paired_now {
                let fresh = ctx.args.pin.clone().unwrap_or_else(new_pin);
                tracing::info!(%peer, client = %hs.client_name, "paired");
                println!("PIN {fresh}");
                *pin.lock().expect("lock") = fresh;
            }
            // One controlling client at a time. The same client coming back
            // (after a network drop or a restart) replaces its old session.
            let done = {
                let mut a = active.lock().await;
                if let Some(old) = a.as_mut() {
                    if old.client == hs.client_fp {
                        tracing::info!(%peer, "client reconnected; closing its previous session");
                        // The old session tells its client (it may be another
                        // tab still open) and ends; the connection closes
                        // after it, or after a second regardless.
                        let conn_old = old.conn.clone();
                        if let Some(k) = old.kick.take() {
                            let _ = k.send(());
                        }
                        let mut d = old.done.clone();
                        drop(a);
                        if tokio::time::timeout(std::time::Duration::from_secs(1), d.changed()).await.is_err() {
                            conn_old.close(1u32.into(), b"replaced");
                            let _ = tokio::time::timeout(std::time::Duration::from_secs(2), d.changed()).await;
                        }
                        a = active.lock().await;
                    } else {
                        let text = "Another client is connected to this host.".to_string();
                        let _ = hs.control_tx.send(&Msg::Error { code: ErrorCode::Busy as u16, text }).await;
                        let _ = hs.control_tx.0.finish();
                        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                        return;
                    }
                }
                let (tx, rx) = tokio::sync::watch::channel(());
                let (kick, kicked) = tokio::sync::oneshot::channel();
                *a = Some(Active { client: hs.client_fp, conn: conn.clone(), done: rx, kick: Some(kick) });
                (tx, kicked)
            };
            let (done, kicked) = done;
            if let Err(e) = session::run(ctx, conn.clone(), hs, kicked).await {
                tracing::info!(%peer, "session ended: {e:#}");
            }
            {
                let mut a = active.lock().await;
                if a.as_ref().is_some_and(|x| x.conn.stable_id() == conn.stable_id()) {
                    *a = None;
                }
            }
            let _ = done.send(());
            conn.close(0u32.into(), b"bye");
        });
    }
    Ok(())
}
