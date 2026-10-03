//! qr-relay: rendezvous and relay for QuickRemote hosts and clients.
//!
//! Rendezvous (QUIC, ALPN `qr-rv/1`, one stream per peer):
//! - a host proves its host ID with a signature from its identity key and
//!   stays registered while connected;
//! - a client looks a host up; both get each other's observed and local
//!   addresses and punch holes in their NATs at the same moment.
//!
//! Relay: when punching fails, a session gets its own UDP port. Each peer
//! sends `PUNCH_MAGIC ‖ token` to it (opening its NAT toward the port), and
//! the relay then forwards every other packet between the two addresses
//! it learned. The session itself stays end-to-end QUIC: the relay sees
//! only ciphertext.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use clap::Parser;
use qr_net::identity::{self, Identity, fingerprint, host_id};
use qr_net::session::{FramedRecv, FramedSend};
use qr_proto::rendezvous::{PUNCH_MAGIC, REGISTER_CONTEXT, RvMsg, TOKEN_LEN, rv_error};
use rand::RngCore;
use tokio::net::UdpSocket;
use tokio::sync::mpsc;

#[derive(Parser, Debug)]
#[command(name = "qr-relay", about = "Rendezvous and relay server for QuickRemote")]
struct Args {
    /// UDP address for rendezvous connections.
    #[arg(long, default_value = "0.0.0.0:4500")]
    listen: SocketAddr,
    /// Public address peers use for relay ports (default: the listen IP).
    #[arg(long)]
    public_ip: Option<std::net::IpAddr>,
    /// First UDP port for relayed sessions.
    #[arg(long, default_value_t = 40000)]
    relay_port_min: u16,
    /// Last UDP port for relayed sessions.
    #[arg(long, default_value_t = 40999)]
    relay_port_max: u16,
    /// Most simultaneous relayed sessions.
    #[arg(long, default_value_t = 200)]
    max_relays: usize,
    /// Per-session relay bandwidth cap in Mbit/s (0: none).
    #[arg(long, default_value_t = 0)]
    relay_mbps: u32,
    /// Identity directory (default ~/.config/quickremote/relay).
    #[arg(long)]
    config_dir: Option<PathBuf>,
}

/// A registered host: where to reach its rendezvous stream.
struct Host {
    tx: mpsc::UnboundedSender<RvMsg>,
    observed: SocketAddr,
    locals: Vec<String>,
}

/// A pending introduction, so either side can ask for a relay by token.
struct Intro {
    host_tx: mpsc::UnboundedSender<RvMsg>,
    client_tx: mpsc::UnboundedSender<RvMsg>,
    relay: Option<SocketAddr>,
    created: Instant,
}

#[derive(Default)]
struct State {
    hosts: HashMap<String, Host>,
    intros: HashMap<[u8; TOKEN_LEN], Intro>,
    relays: usize,
}

struct Ctx {
    args: Args,
    state: Mutex<State>,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();
    let args = Args::parse();
    qr_net::init_crypto();
    let dir = args.config_dir.clone().unwrap_or_else(|| identity::default_dir("relay"));
    let id = Identity::load_or_create(&dir, "qr-relay")?;
    let ep = qr_net::endpoint::endpoint(args.listen, &id, true, &[qr_proto::rendezvous::ALPN])?;
    println!("QuickRemote relay listening on {}", ep.local_addr()?);
    println!("FINGERPRINT {}", identity::hex(&id.fingerprint));
    let ctx = Arc::new(Ctx { args, state: Mutex::default() });

    // Forget introductions nobody used.
    {
        let ctx = ctx.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(30)).await;
                let mut st = ctx.state.lock().expect("lock");
                st.intros.retain(|_, i| i.created.elapsed() < Duration::from_secs(120) || i.relay.is_some());
            }
        });
    }

    while let Some(inc) = ep.accept().await {
        let ctx = ctx.clone();
        tokio::spawn(async move {
            let conn = match inc.await {
                Ok(c) => c,
                Err(e) => {
                    tracing::debug!("handshake failed: {e}");
                    return;
                }
            };
            let peer = conn.remote_address();
            if let Err(e) = peer_session(ctx, conn).await {
                tracing::debug!(%peer, "peer left: {e:#}");
            }
        });
    }
    Ok(())
}

async fn peer_session(ctx: Arc<Ctx>, conn: quinn::Connection) -> Result<()> {
    let (tx, rx) = conn.accept_bi().await?;
    let mut tx = FramedSend(tx);
    let mut rx = FramedRecv(rx);
    let observed = conn.remote_address();

    // Everything sent to this peer goes through one writer task.
    let (out_tx, mut out_rx) = mpsc::unbounded_channel::<RvMsg>();
    let writer = tokio::spawn(async move {
        while let Some(m) = out_rx.recv().await {
            if tx.send_raw(&m.encode()).await.is_err() {
                return;
            }
        }
    });

    let mut nonce = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut nonce);
    let _ = out_tx.send(RvMsg::Challenge { nonce });
    let mut registered: Option<String> = None;

    let result: Result<()> = async {
        while let Some(body) = rx.recv_raw().await? {
            // Peers open the stream with an empty frame.
            if body.is_empty() {
                continue;
            }
            match RvMsg::decode(&body)? {
                RvMsg::Register { host_id: hid, cert_der, signature, locals } => {
                    let mut msg = REGISTER_CONTEXT.to_vec();
                    msg.extend_from_slice(&nonce);
                    if host_id(&fingerprint(&cert_der)) != hid || !identity::verify(&cert_der, &msg, &signature) {
                        let _ = out_tx.send(RvMsg::Error {
                            code: rv_error::BAD_SIGNATURE,
                            text: "the host ID doesn't match its key".into(),
                        });
                        bail!("bad registration for {hid}");
                    }
                    tracing::info!(host = %hid, %observed, "host registered");
                    ctx.state
                        .lock()
                        .expect("lock")
                        .hosts
                        .insert(hid.clone(), Host { tx: out_tx.clone(), observed, locals });
                    registered = Some(hid);
                    let _ = out_tx.send(RvMsg::Registered { observed: observed.to_string() });
                }
                RvMsg::Lookup { host_id: hid, locals } => {
                    let mut st = ctx.state.lock().expect("lock");
                    let Some(h) = st.hosts.get(&hid) else {
                        let _ = out_tx.send(RvMsg::Error {
                            code: rv_error::UNKNOWN_HOST,
                            text: "The host isn't online on the rendezvous server. Check that the host is running and \
                                   has internet access turned on."
                                .into(),
                        });
                        continue;
                    };
                    let mut token = [0u8; TOKEN_LEN];
                    rand::thread_rng().fill_bytes(&mut token);
                    let _ = h.tx.send(RvMsg::Introduce {
                        token,
                        peer_observed: observed.to_string(),
                        peer_locals: locals,
                        punch_in_ms: 300,
                        initiator: false,
                    });
                    let _ = out_tx.send(RvMsg::Introduce {
                        token,
                        peer_observed: h.observed.to_string(),
                        peer_locals: h.locals.clone(),
                        punch_in_ms: 300,
                        initiator: true,
                    });
                    let host_tx = h.tx.clone();
                    st.intros.insert(
                        token,
                        Intro { host_tx, client_tx: out_tx.clone(), relay: None, created: Instant::now() },
                    );
                    tracing::info!(host = %hid, client = %observed, "introduced");
                }
                RvMsg::RelayRequest { token } => {
                    let r = allocate_relay(&ctx, token).await;
                    match r {
                        Ok(addr) => tracing::info!(%addr, "relay allocated"),
                        Err(e) => {
                            let _ =
                                out_tx.send(RvMsg::Error { code: rv_error::NO_RELAY_CAPACITY, text: format!("{e:#}") });
                        }
                    }
                }
                other => tracing::debug!(?other, "unexpected message"),
            }
        }
        Ok(())
    }
    .await;

    if let Some(hid) = registered {
        let mut st = ctx.state.lock().expect("lock");
        // Only remove our own entry (the host may have re-registered).
        if st.hosts.get(&hid).is_some_and(|h| h.tx.same_channel(&out_tx)) {
            st.hosts.remove(&hid);
            tracing::info!(host = %hid, "host left");
        }
    }
    writer.abort();
    result
}

/// Opens a relay port for an introduced pair and tells both peers.
async fn allocate_relay(ctx: &Arc<Ctx>, token: [u8; TOKEN_LEN]) -> Result<SocketAddr> {
    {
        let st = ctx.state.lock().expect("lock");
        let intro = st.intros.get(&token).context("unknown session token")?;
        if let Some(addr) = intro.relay {
            // Already allocated (both sides asked): repeat the answer.
            let m = RvMsg::RelayAllocated { token, relay_addr: addr.to_string() };
            let _ = intro.host_tx.send(m.clone());
            let _ = intro.client_tx.send(m);
            return Ok(addr);
        }
        if st.relays >= ctx.args.max_relays {
            bail!("the relay is full");
        }
    }
    let ip = ctx.args.public_ip.unwrap_or(ctx.args.listen.ip());
    let mut sock = None;
    for _ in 0..50 {
        let port =
            rand::random::<u16>() % (ctx.args.relay_port_max - ctx.args.relay_port_min + 1) + ctx.args.relay_port_min;
        // Bound to the public address: on a multi-homed server, a socket on
        // 0.0.0.0 may answer from another address, which NATs drop.
        if let Ok(s) = UdpSocket::bind(SocketAddr::new(ip, port)).await {
            sock = Some(s);
            break;
        }
    }
    let sock = sock.context("no free relay port")?;
    let public = SocketAddr::new(ip, sock.local_addr()?.port());
    let m = RvMsg::RelayAllocated { token, relay_addr: public.to_string() };
    {
        let mut st = ctx.state.lock().expect("lock");
        let intro = st.intros.get_mut(&token).context("session expired")?;
        intro.relay = Some(public);
        let _ = intro.host_tx.send(m.clone());
        let _ = intro.client_tx.send(m);
        st.relays += 1;
    }
    let ctx = ctx.clone();
    tokio::spawn(async move {
        if let Err(e) = relay(sock, token, ctx.args.relay_mbps).await {
            tracing::info!("relay ended: {e:#}");
        }
        let mut st = ctx.state.lock().expect("lock");
        st.relays -= 1;
        st.intros.remove(&token);
    });
    Ok(public)
}

/// Forwards packets between the two peers that said hello with the token.
/// Ends after 30 s without traffic.
async fn relay(sock: UdpSocket, token: [u8; TOKEN_LEN], mbps: u32) -> Result<()> {
    let mut peers: Vec<SocketAddr> = Vec::with_capacity(2);
    let mut buf = vec![0u8; 65536];
    let mut hello = PUNCH_MAGIC.to_vec();
    hello.extend_from_slice(&token);
    let mut budget = 0f64;
    let mut last = Instant::now();
    loop {
        let (n, from) = match tokio::time::timeout(Duration::from_secs(30), sock.recv_from(&mut buf)).await {
            Ok(r) => r?,
            Err(_) => bail!("idle"),
        };
        let pkt = &buf[..n];
        if pkt.starts_with(&hello) {
            if !peers.contains(&from) {
                if peers.len() == 2 {
                    // A peer behind a rebinding NAT: replace its old address.
                    peers.remove(0);
                }
                peers.push(from);
                tracing::debug!(%from, "relay peer joined");
            }
            continue;
        }
        let Some(to) = (match peers.as_slice() {
            [a, b] if from == *a => Some(*b),
            [a, b] if from == *b => Some(*a),
            _ => None,
        }) else {
            continue;
        };
        if mbps > 0 {
            let now = Instant::now();
            budget = (budget + now.duration_since(last).as_secs_f64() * mbps as f64 * 1e6).min(mbps as f64 * 1e5);
            last = now;
            if budget < (n * 8) as f64 {
                continue;
            }
            budget -= (n * 8) as f64;
        }
        let _ = sock.send_to(pkt, to).await;
    }
}
