//! QuickRemote client.

mod app;
mod keymap;
mod render;
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
    /// Requested virtual-display size, `WxH@Hz` (0 = host decides).
    #[arg(long)]
    size: Option<String>,
}

fn hostname() -> String {
    std::fs::read_to_string("/etc/hostname")
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|_| "client".into())
}

fn parse_size(s: &str) -> (u16, u16, u16) {
    let (wh, hz) = s.split_once('@').unwrap_or((s, "0"));
    let (w, h) = wh.split_once('x').unwrap_or(("0", "0"));
    (
        w.parse().unwrap_or(0),
        h.parse().unwrap_or(0),
        hz.parse().unwrap_or(0),
    )
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
            let paired = if keystore.by_host_id(&h.host_id).is_some() {
                "paired"
            } else {
                "not paired"
            };
            println!(
                "  {}  {}  {}  {}  {}",
                h.name, h.host_id, addr, h.os, paired
            );
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
pub async fn resolve(
    target: &str,
    keystore: &KeyStore,
) -> Result<(SocketAddr, Option<qr_net::Fingerprint>)> {
    if target.starts_with("qr-") {
        let peer = keystore.by_host_id(target);
        let fp = peer.and_then(|p| {
            let b: Vec<u8> = (0..p.fingerprint.len())
                .step_by(2)
                .filter_map(|i| u8::from_str_radix(&p.fingerprint[i..i + 2], 16).ok())
                .collect();
            b.try_into().ok()
        });
        let (_b, mut rx) = Browser::start()?;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
        while let Ok(Some(d)) = tokio::time::timeout_at(deadline, rx.recv()).await {
            if let Discovery::Up(h) = d
                && h.host_id == target
                && let Some(a) = h.addrs.first()
            {
                return Ok((*a, fp));
            }
        }
        if let Some(p) = peer
            && let Ok(a) = p.last_addr.parse()
        {
            return Ok((a, fp));
        }
        bail!("The host {target} isn't on this network and has no known address.");
    }
    let addr = if target.contains(':') {
        target.to_string()
    } else {
        format!("{target}:9450")
    };
    let a = addr
        .to_socket_addrs()
        .with_context(|| format!("resolving {addr}"))?
        .next()
        .ok_or_else(|| anyhow!("no address for {addr}"))?;
    let fp = keystore
        .peers
        .iter()
        .find(|p| p.last_addr == a.to_string())
        .and_then(|p| {
            let b: Vec<u8> = (0..p.fingerprint.len())
                .step_by(2)
                .filter_map(|i| u8::from_str_radix(&p.fingerprint[i..i + 2], 16).ok())
                .collect();
            b.try_into().ok()
        });
    Ok((a, fp))
}

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .with_writer(std::io::stderr)
        .init();
    let args = Args::parse();
    qr_net::init_crypto();
    if args.headless || args.list {
        return tokio::runtime::Runtime::new()?.block_on(cli(args));
    }
    // The window client runs its own runtime next to the event loop.
    let dir = args
        .config_dir
        .clone()
        .unwrap_or_else(|| identity::default_dir("client"));
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
    })
}

/// `--list` and `--headless`.
async fn cli(args: Args) -> Result<()> {
    let dir = args
        .config_dir
        .clone()
        .unwrap_or_else(|| identity::default_dir("client"));
    let name = args.name.clone().unwrap_or_else(hostname);
    let id = Identity::load_or_create(&dir, &name)?;
    let keystore = Mutex::new(KeyStore::open(&dir.join("paired.json"))?);
    if args.list {
        return list(&keystore.lock().expect("lock"), args.list_secs).await;
    }
    let target = args
        .connect
        .clone()
        .ok_or_else(|| anyhow!("give --connect ADDR|ID, or --list"))?;
    let (addr, expect_fp) = resolve(&target, &keystore.lock().expect("lock")).await?;
    qr_media::init()?;
    let decoders = tokio::task::spawn_blocking(qr_media::probe::decoders).await??;
    anyhow::ensure!(!decoders.is_empty(), "no working video decoder found");

    let decoded = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let d2 = decoded.clone();
    let sink: session::FrameSink = Arc::new(move |_f, _t| {
        d2.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    });
    let opts = session::ConnectOpts {
        addr,
        expect_fp,
        pin: args.pin.clone(),
        name,
        want: args.size.as_deref().map(parse_size).unwrap_or((0, 0, 0)),
        decoders,
        decoder_override: args.decoder.clone(),
        keep_samples: args.latency_log.is_some(),
    };
    let c = session::connect(&id, &keystore, opts, sink).await?;
    eprintln!("connected to {} ({})", c.host_name, c.host_id);
    let mut stats_out: Option<Box<dyn std::io::Write + Send>> = match args.stats_json.as_deref() {
        Some("-") => Some(Box::new(std::io::stdout())),
        Some(p) => Some(Box::new(std::fs::File::create(p)?)),
        None => None,
    };
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
        if args
            .frames
            .is_some_and(|n| decoded.load(std::sync::atomic::Ordering::Relaxed) >= n)
            || args
                .duration
                .is_some_and(|d| start.elapsed().as_secs_f32() >= d)
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
    session::bye(&c).await;
    eprintln!(
        "decoded {} frames; {reason}",
        decoded.load(std::sync::atomic::Ordering::Relaxed)
    );
    if reason != "done" && reason != "interrupted" {
        bail!(reason);
    }
    Ok(())
}

use std::io::Write as _;
