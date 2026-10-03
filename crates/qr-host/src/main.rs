//! QuickRemote host: shares this machine's screen (or a virtual display)
//! with paired clients.

mod clipboard;
mod input;
mod media;
mod outbox;
mod ratectl;
mod session;
mod stream;

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
use qr_proto::msg::{ErrorCode, Profile};
use qr_proto::{ALPN, Msg};

#[derive(Parser, Debug)]
#[command(name = "qr-host", about = "Share this machine with QuickRemote clients")]
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
    /// What to stream: `test` (a test pattern; `test:<pattern>` picks a
    /// videotestsrc pattern).
    #[arg(long, default_value = "test")]
    source: String,
    /// Picture size for test and virtual-display sources.
    #[arg(long, default_value = "1920x1080")]
    size: String,
    /// Frame rate cap.
    #[arg(long, default_value_t = 60)]
    fps: u32,
    /// Fixed starting bitrate in kbit/s (0: from the profile).
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
    /// Rate-control profile (`auto`, `lan`, `internet`).
    #[arg(long, default_value = "auto")]
    profile: String,
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
    /// Audio to send: `auto`, `none`, `test:<wave>`, `pipewire:<target>`,
    /// `pulse[:<device>]`, `default`.
    #[arg(long, default_value = "auto")]
    audio: String,
    /// Where the client's microphone goes: `none`, `file:<wav>`,
    /// `pipewire:<target>`.
    #[arg(long, default_value = "none")]
    mic_out: String,
    /// Where files from clients are saved (default ~/Downloads/QuickRemote).
    #[arg(long)]
    files_dir: Option<PathBuf>,
    /// Files put here are sent to the connected client, then removed
    /// (default ~/QuickRemote/Outbox).
    #[arg(long)]
    outbox: Option<PathBuf>,
    /// Input backend: `auto` (from the source), `none`, `xtest:<display>`.
    #[arg(long, default_value = "auto")]
    input: String,
}

impl Args {
    fn profile(&self) -> Option<Profile> {
        match self.profile.as_str() {
            "lan" => Some(Profile::Lan),
            "internet" => Some(Profile::Internet),
            _ => None,
        }
    }

    fn size(&self) -> (u32, u32) {
        self.size.split_once('x').and_then(|(w, h)| Some((w.parse().ok()?, h.parse().ok()?))).unwrap_or((1920, 1080))
    }
}

/// The session currently in control.
struct Active {
    client: qr_net::Fingerprint,
    conn: quinn::Connection,
    done: tokio::sync::watch::Receiver<()>,
}

pub struct HostCtx {
    pub args: Args,
    pub encoders: Vec<EncoderInfo>,
    pub input: input::Input,
    /// Relay ports allocated for this host: sessions from them are relayed.
    pub relays: Arc<Mutex<Vec<SocketAddr>>>,
}

impl HostCtx {
    pub fn files_dir(&self) -> PathBuf {
        self.args.files_dir.clone().unwrap_or_else(|| dirs_home().join("Downloads").join("QuickRemote"))
    }

    pub fn outbox_dir(&self) -> PathBuf {
        self.args.outbox.clone().unwrap_or_else(|| dirs_home().join("QuickRemote").join("Outbox"))
    }

    /// The mode a stream runs at: the configured size for now (virtual
    /// displays follow the client's request once they exist).
    pub fn display_mode(&self, _w: u16, _h: u16, _refresh: u16) -> (u32, u32, u32) {
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
    let args = Args::parse();
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
        tracing::info!(element = e.element, codec = e.codec.name(), hw = e.hw, "encoder");
    }

    let injector = input::open(&args.input, &args.source).context("opening the input backend")?;
    tracing::info!(backend = injector.name(), "input");
    let ctx = Arc::new(HostCtx { args, encoders, input: input::Input::new(injector), relays });
    let active: Arc<tokio::sync::Mutex<Option<Active>>> = Arc::default();
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
                if let Some(old) = a.as_ref() {
                    if old.client == hs.client_fp {
                        tracing::info!(%peer, "client reconnected; closing its previous session");
                        old.conn.close(1u32.into(), b"replaced");
                        let mut d = old.done.clone();
                        drop(a);
                        let _ = tokio::time::timeout(std::time::Duration::from_secs(3), d.changed()).await;
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
                *a = Some(Active { client: hs.client_fp, conn: conn.clone(), done: rx });
                tx
            };
            if let Err(e) = session::run(ctx, conn.clone(), hs).await {
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
