//! QuickRemote client.

mod app;
mod keymap;
mod pads;
mod render;
mod script;
mod session;

use std::net::{SocketAddr, ToSocketAddrs};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use clap::Parser;
use qr_net::discovery::{Browser, Discovery};
use qr_net::keystore::KeyStore;
use qr_net::{Identity, identity};

#[derive(Parser, Debug)]
#[command(name = "qr-client", about = "Connect to a QuickRemote host")]
struct Args {
    /// Host address (`ip:port`) or host ID (`qr-xxxx-xxxx-xxxxx`).
    #[arg(long)]
    connect: Option<String>,
    /// Rendezvous server (`host[:port]`) for reaching a host by ID over the
    /// internet.
    #[arg(long)]
    rendezvous: Option<String>,
    /// PIN shown on the host (first connection only).
    #[arg(long)]
    pin: Option<String>,
    /// List hosts on the LAN and paired hosts, then exit.
    #[arg(long)]
    list: bool,
    /// How long `--list` listens, seconds.
    #[arg(long, default_value_t = 3.0)]
    list_secs: f32,
    /// No window: receive and decode only (tests, measurements).
    #[arg(long)]
    headless: bool,
    /// Headless: stop after this many decoded frames.
    #[arg(long)]
    frames: Option<u64>,
    /// Headless: stop after this many seconds.
    #[arg(long)]
    duration: Option<f32>,
    /// Write one JSON stats line per second to this file (`-` = stdout).
    #[arg(long)]
    stats_json: Option<String>,
    /// Write every frame's latency split as JSON lines to this file.
    #[arg(long)]
    latency_log: Option<PathBuf>,
    /// Name shown on the host.
    #[arg(long)]
    name: Option<String>,
    /// Identity and paired-host directory (default ~/.config/quickremote/client).
    #[arg(long)]
    config_dir: Option<PathBuf>,
    /// Decoder element to use instead of the best available.
    #[arg(long)]
    decoder: Option<String>,
    /// Headless: send the input commands in this file (see script.rs).
    #[arg(long)]
    script: Option<String>,
    /// Write `<event> <CLOCK_MONOTONIC µs>` for each input event sent.
    #[arg(long)]
    input_trace: Option<String>,
    /// Headless: send these files to the host.
    #[arg(long)]
    send_file: Vec<PathBuf>,
    /// Headless (tests): cancel uploads after this many bytes.
    #[arg(long)]
    cancel_after: Option<u64>,
    /// Where files from the host are saved (default ~/Downloads).
    #[arg(long)]
    recv_dir: Option<PathBuf>,
    /// X display whose clipboard is synced (default: $DISPLAY).
    #[arg(long)]
    clipboard_display: Option<String>,
    /// Don't sync the clipboard.
    #[arg(long)]
    no_clipboard: bool,
    /// Headless (tests): send a scripted gamepad 0.
    #[arg(long)]
    fake_pad: bool,
    /// Turn sharp-text mode on at the start.
    #[arg(long)]
    sharp: bool,
    /// Headless: at exit, write the last decoded picture's planes to this
    /// file and its layout to `<file>.json`.
    #[arg(long)]
    dump_frame: Option<PathBuf>,
    /// Host audio output: `default`, `none`, `file:<wav>`.
    #[arg(long, default_value = "default")]
    audio_out: String,
    /// Send a microphone: `default`, `file:<wav>`, `test:<wave>` (off when absent).
    #[arg(long)]
    mic: Option<String>,
    /// Requested virtual-display size, `WxH@Hz` (0 = host decides).
    #[arg(long)]
    size: Option<String>,
}

fn hostname() -> String {
    std::fs::read_to_string("/etc/hostname").map(|s| s.trim().to_string()).unwrap_or_else(|_| "client".into())
}

/// Starts the gamepad thread for a session.
pub fn start_pads(c: &session::Connected) {
    let (tx, rx) = std::sync::mpsc::channel();
    *c.rumble.lock().expect("lock") = Some(tx);
    let (conn, ctl) = (c.conn.clone(), c.ctl.clone());
    std::thread::Builder::new().name("qr-pads".into()).spawn(move || pads::run(conn, ctl, rx)).ok();
}

fn recv_dir(args: &Args) -> PathBuf {
    args.recv_dir.clone().unwrap_or_else(|| {
        std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from(".")).join("Downloads")
    })
}

fn audio_out(s: &str) -> Option<qr_media::audio::AudioSink> {
    use qr_media::audio::AudioSink;
    match s.split_once(':').unwrap_or((s, "")) {
        ("none", _) => None,
        ("file", p) => Some(AudioSink::File { path: p.into() }),
        _ => Some(AudioSink::Default),
    }
}

fn mic_source(s: &str) -> qr_media::audio::AudioSource {
    use qr_media::audio::AudioSource;
    match s.split_once(':').unwrap_or((s, "")) {
        ("file", p) => AudioSource::File { path: p.into() },
        ("test", w) => AudioSource::Test { wave: if w.is_empty() { "sine".into() } else { w.into() } },
        _ => AudioSource::Default,
    }
}

fn parse_size(s: &str) -> (u16, u16, u16) {
    let (wh, hz) = s.split_once('@').unwrap_or((s, "0"));
    let (w, h) = wh.split_once('x').unwrap_or(("0", "0"));
    (w.parse().unwrap_or(0), h.parse().unwrap_or(0), hz.parse().unwrap_or(0))
}

async fn list(keystore: &KeyStore, secs: f32) -> Result<()> {
    let (_b, mut rx) = Browser::start()?;
    let deadline = tokio::time::Instant::now() + Duration::from_secs_f32(secs);
    let mut seen = std::collections::HashSet::new();
    println!("On this network:");
    while let Ok(Some(d)) = tokio::time::timeout_at(deadline, rx.recv()).await {
        if let Discovery::Up(h) = d
            && seen.insert(h.host_id.clone())
        {
            let addr = h.addrs.first().map(|a| a.to_string()).unwrap_or_default();
            let paired = if keystore.by_host_id(&h.host_id).is_some() { "paired" } else { "not paired" };
            println!("  {}  {}  {}  {}  {}", h.name, h.host_id, addr, h.os, paired);
        }
    }
    println!("Paired:");
    for p in keystore.peers.iter().filter(|p| !p.host_id.is_empty()) {
        println!("  {}  {}  last {}", p.name, p.host_id, p.last_addr);
    }
    Ok(())
}

/// Resolves `--connect`: an address, or a host ID via mDNS and then the
/// last known address.
fn fp_of(p: &qr_net::keystore::Peer) -> Option<qr_net::Fingerprint> {
    let b: Vec<u8> = (0..p.fingerprint.len())
        .step_by(2)
        .filter_map(|i| p.fingerprint.get(i..i + 2).and_then(|h| u8::from_str_radix(h, 16).ok()))
        .collect();
    b.try_into().ok()
}

async fn lookup_server(s: &str) -> Result<SocketAddr> {
    let s = if s.contains(':') { s.to_string() } else { format!("{s}:4500") };
    tokio::net::lookup_host(&s).await?.next().ok_or_else(|| anyhow!("no address for {s}"))
}

/// Resolves `--connect`. An address is used as is. A host ID is looked
/// for on the LAN first (mDNS, 1.5 s), then through the rendezvous server
/// (`--rendezvous`, or the one the host reported when paired), then at its
/// last known address.
pub async fn resolve(
    target: &str,
    keystore: &KeyStore,
    rendezvous: Option<&str>,
) -> Result<(session::Target, Option<qr_net::Fingerprint>)> {
    use session::Target;
    if target.starts_with("qr-") {
        let peer = keystore.by_host_id(target);
        let fp = peer.and_then(fp_of);
        if let Ok((_b, mut rx)) = Browser::start() {
            let deadline = tokio::time::Instant::now() + Duration::from_millis(1500);
            while let Ok(Some(d)) = tokio::time::timeout_at(deadline, rx.recv()).await {
                if let Discovery::Up(h) = d
                    && h.host_id == target
                    && let Some(a) = h.addrs.first()
                {
                    return Ok((Target::Addr(*a), fp));
                }
            }
        }
        let server =
            rendezvous.map(str::to_string).or_else(|| peer.map(|p| p.rendezvous.clone()).filter(|s| !s.is_empty()));
        if let Some(server) = server {
            let server = lookup_server(&server).await?;
            return Ok((Target::Rendezvous { server, host_id: target.to_string() }, fp));
        }
        if let Some(p) = peer
            && let Ok(a) = p.last_addr.parse()
        {
            return Ok((Target::Addr(a), fp));
        }
        bail!("The host {target} isn't on this network, and no rendezvous server is known for it (use --rendezvous).");
    }
    let addr = if target.contains(':') { target.to_string() } else { format!("{target}:9450") };
    let a = addr
        .to_socket_addrs()
        .with_context(|| format!("resolving {addr}"))?
        .next()
        .ok_or_else(|| anyhow!("no address for {addr}"))?;
    let fp = keystore.peers.iter().find(|p| p.last_addr == a.to_string()).and_then(fp_of);
    Ok((Target::Addr(a), fp))
}

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .with_writer(std::io::stderr)
        .init();
    let args = Args::parse();
    qr_net::init_crypto();
    if args.headless || args.list {
        return tokio::runtime::Runtime::new()?.block_on(cli(args));
    }
    // The window client runs its own runtime next to the event loop.
    let dir = args.config_dir.clone().unwrap_or_else(|| identity::default_dir("client"));
    let name = args.name.clone().unwrap_or_else(hostname);
    let id = Identity::load_or_create(&dir, &name)?;
    let keystore = KeyStore::open(&dir.join("paired.json"))?;
    qr_media::init()?;
    let decoders = qr_media::probe::decoders()?;
    anyhow::ensure!(!decoders.is_empty(), "no working video decoder found");
    app::run(app::Config {
        id,
        keystore: Arc::new(Mutex::new(keystore)),
        decoders,
        decoder_override: args.decoder.clone(),
        name,
        connect: args.connect.clone(),
        pin: args.pin.clone(),
        rendezvous: args.rendezvous.clone(),
    })
}

/// `--list` and `--headless`.
async fn cli(args: Args) -> Result<()> {
    let dir = args.config_dir.clone().unwrap_or_else(|| identity::default_dir("client"));
    let name = args.name.clone().unwrap_or_else(hostname);
    let id = Identity::load_or_create(&dir, &name)?;
    let keystore = Mutex::new(KeyStore::open(&dir.join("paired.json"))?);
    if args.list {
        return list(&keystore.lock().expect("lock"), args.list_secs).await;
    }
    let target = args.connect.clone().ok_or_else(|| anyhow!("give --connect ADDR|ID, or --list"))?;
    let (target, expect_fp) = resolve(&target, &keystore.lock().expect("lock"), args.rendezvous.as_deref()).await?;
    qr_media::init()?;
    let decoders = tokio::task::spawn_blocking(qr_media::probe::decoders).await??;
    anyhow::ensure!(!decoders.is_empty(), "no working video decoder found");

    let decoded = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let d2 = decoded.clone();
    let last_frame: Arc<Mutex<Option<qr_media::decode::DecodedFrame>>> = Arc::default();
    let lf = args.dump_frame.is_some().then(|| last_frame.clone());
    let sink: session::FrameSink = Arc::new(move |f, _t| {
        d2.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        if let Some(lf) = &lf {
            *lf.lock().expect("lock") = Some(f);
        }
    });
    let opts = session::ConnectOpts {
        target,
        expect_fp,
        pin: args.pin.clone(),
        name,
        want: args.size.as_deref().map(parse_size).unwrap_or((0, 0, 0)),
        decoders,
        decoder_override: args.decoder.clone(),
        keep_samples: args.latency_log.is_some(),
        audio_out: audio_out(&args.audio_out),
        mic: args.mic.as_deref().map(mic_source),
        recv_dir: recv_dir(&args),
        clipboard: if args.no_clipboard {
            None
        } else {
            args.clipboard_display.clone().or_else(|| std::env::var("DISPLAY").ok())
        },
    };
    let c = session::connect(&id, &keystore, opts, sink).await?;
    eprintln!("connected to {} ({})", c.host_name, c.host_id);
    let mut stats_out: Option<Box<dyn std::io::Write + Send>> = match args.stats_json.as_deref() {
        Some("-") => Some(Box::new(std::io::stdout())),
        Some(p) => Some(Box::new(std::fs::File::create(p)?)),
        None => None,
    };
    if let Some(p) = &args.input_trace {
        qr_net::trace::open(p)?;
    }
    {
        let mut ev = c.files.events.subscribe();
        let (files, ctl, cancel_after) = (c.files.clone(), c.ctl.clone(), args.cancel_after);
        tokio::spawn(async move {
            use qr_net::files::Progress;
            let mut cancelled = std::collections::HashSet::new();
            while let Ok(e) = ev.recv().await {
                match e {
                    Progress::Sending { id, done, .. } => {
                        if cancel_after.is_some_and(|n| done >= n) && cancelled.insert(id) {
                            eprintln!("FILE cancel {id} at {done}");
                            files.cancel(id, &ctl);
                        }
                    }
                    Progress::Sent { name, .. } => eprintln!("FILE sent {name}"),
                    Progress::Received { path, .. } => eprintln!("FILE received {}", path.display()),
                    Progress::Failed { name, reason, .. } => eprintln!("FILE failed {name}: {reason}"),
                    Progress::Receiving { .. } => {}
                }
            }
        });
    }
    start_pads(&c);
    if args.fake_pad {
        let (conn, ctl) = (c.conn.clone(), c.ctl.clone());
        std::thread::spawn(move || pads::fake(conn, ctl));
    }
    if args.sharp {
        (c.ctl)(qr_proto::Msg::SetMode { modes: qr_proto::msg::mode::SHARP_TEXT });
    }
    for p in &args.send_file {
        c.files.offer(p, &c.ctl).await?;
    }
    if let Some(path) = args.script.clone() {
        let (input, ctl) = (c.input.clone(), c.ctl.clone());
        tokio::spawn(async move {
            if let Err(e) = script::run(&path, &input, &ctl).await {
                tracing::error!("script: {e:#}");
            }
        });
    }
    let start = Instant::now();
    let mut ended = c.ended.clone();
    let mut tick = tokio::time::interval(Duration::from_secs(1));
    tick.tick().await;
    let reason = loop {
        tokio::select! {
            _ = tick.tick() => {
                let st = c.shared.lock().expect("lock").stats.clone();
                if let Some(o) = stats_out.as_mut() {
                    writeln!(o, "{}", serde_json::to_string(&st)?)?;
                    o.flush()?;
                }
            }
            _ = ended.changed() => break ended.borrow().clone().unwrap_or_default(),
            _ = tokio::signal::ctrl_c() => break "interrupted".into(),
        }
        if args.frames.is_some_and(|n| decoded.load(std::sync::atomic::Ordering::Relaxed) >= n)
            || args.duration.is_some_and(|d| start.elapsed().as_secs_f32() >= d)
        {
            break "done".into();
        }
    };
    if let Some(p) = &args.latency_log {
        let mut f = std::io::BufWriter::new(std::fs::File::create(p)?);
        for s in &c.shared.lock().expect("lock").samples {
            writeln!(f, "{}", serde_json::to_string(s)?)?;
        }
    }
    if let (Some(p), Some(f)) = (&args.dump_frame, last_frame.lock().expect("lock").as_ref()) {
        std::fs::write(p, &f.data)?;
        let meta = serde_json::json!({
            "width": f.width, "height": f.height, "format": format!("{:?}", f.format),
            "offsets": f.offsets, "strides": f.strides,
        });
        std::fs::write(p.with_extension("json"), meta.to_string())?;
    }
    session::bye(&c).await;
    eprintln!("decoded {} frames; {reason}", decoded.load(std::sync::atomic::Ordering::Relaxed));
    if reason != "done" && reason != "interrupted" {
        bail!(reason);
    }
    Ok(())
}

use std::io::Write as _;
