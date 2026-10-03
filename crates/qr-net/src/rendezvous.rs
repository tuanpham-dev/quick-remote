//! Peer side of the rendezvous protocol: hosts register and answer
//! introductions; clients look a host up and connect directly (after
//! punching their NATs) or through the relay.

use std::net::{SocketAddr, UdpSocket};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use qr_proto::rendezvous::{ALPN, PUNCH_MAGIC, REGISTER_CONTEXT, RvMsg, TOKEN_LEN};
use tokio::sync::mpsc;

use crate::endpoint;
use crate::identity::Identity;
use crate::session::{FramedRecv, FramedSend};

/// How long the client tries direct paths before asking for a relay.
pub const DIRECT_TIMEOUT: Duration = Duration::from_millis(2500);

/// This machine's addresses on `port`, for peers on the same network.
pub fn local_candidates(port: u16) -> Vec<String> {
    // Without an interface-enumeration dependency: ask the routing table
    // which source address reaches a public address.
    let mut out = Vec::new();
    for probe in ["192.0.2.1:9", "[2001:db8::1]:9"] {
        let bind = if probe.starts_with('[') { "[::]:0" } else { "0.0.0.0:0" };
        if let Ok(s) = UdpSocket::bind(bind)
            && s.connect(probe).is_ok()
            && let Ok(a) = s.local_addr()
        {
            out.push(SocketAddr::new(a.ip(), port).to_string());
        }
    }
    out
}

/// A rendezvous connection on a shared endpoint.
pub struct RvConn {
    pub conn: quinn::Connection,
    tx: mpsc::UnboundedSender<RvMsg>,
    pub rx: mpsc::UnboundedReceiver<RvMsg>,
}

impl RvConn {
    pub async fn connect(ep: &quinn::Endpoint, server: SocketAddr, id: &Identity) -> Result<Self> {
        let cfg = endpoint::client_config(Some(id), &[ALPN])?;
        let conn = ep
            .connect_with(cfg, server, "quickremote-relay")?
            .await
            .with_context(|| format!("connecting to the rendezvous server {server}"))?;
        let (s, r) = conn.open_bi().await?;
        let mut s = FramedSend(s);
        let mut r = FramedRecv(r);
        // QUIC opens the stream to the server with its first bytes: an
        // empty frame, which the server skips.
        s.send_raw(&[]).await?;
        let (tx, mut out) = mpsc::unbounded_channel::<RvMsg>();
        let (inc_tx, rx) = mpsc::unbounded_channel::<RvMsg>();
        tokio::spawn(async move {
            while let Some(m) = out.recv().await {
                if s.send_raw(&m.encode()).await.is_err() {
                    return;
                }
            }
        });
        tokio::spawn(async move {
            while let Ok(Some(b)) = r.recv_raw().await {
                match RvMsg::decode(&b) {
                    Ok(m) => {
                        if inc_tx.send(m).is_err() {
                            return;
                        }
                    }
                    Err(e) => tracing::debug!("bad rendezvous message: {e}"),
                }
            }
        });
        Ok(Self { conn, tx, rx })
    }

    pub fn send(&self, m: RvMsg) {
        let _ = self.tx.send(m);
    }

    pub async fn recv(&mut self) -> Result<RvMsg> {
        self.rx.recv().await.ok_or_else(|| anyhow!("the rendezvous server closed the connection"))
    }
}

fn punch_packet(token: &[u8; TOKEN_LEN]) -> Vec<u8> {
    let mut p = PUNCH_MAGIC.to_vec();
    p.extend_from_slice(token);
    p
}

/// Sends 5 punch packets 20 ms apart to every candidate, after `delay`.
pub async fn punch(raw: &UdpSocket, token: &[u8; TOKEN_LEN], candidates: &[SocketAddr], delay: Duration) {
    tokio::time::sleep(delay).await;
    let p = punch_packet(token);
    for _ in 0..5 {
        for c in candidates {
            let _ = raw.send_to(&p, c);
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

fn parse_candidates(observed: &str, locals: &[String]) -> Vec<SocketAddr> {
    let mut v: Vec<SocketAddr> =
        std::iter::once(observed).chain(locals.iter().map(String::as_str)).filter_map(|s| s.parse().ok()).collect();
    v.dedup();
    v
}

/// Host side: stays registered; punches toward introduced clients and
/// opens relay ports when asked. `relays` collects the relay addresses in
/// use, so the host can tell relayed sessions apart.
pub async fn host_loop(
    ep: quinn::Endpoint,
    raw: Arc<UdpSocket>,
    server: SocketAddr,
    id: Identity,
    relays: Arc<Mutex<Vec<SocketAddr>>>,
    registered: tokio::sync::watch::Sender<Option<String>>,
) {
    let mut wait = Duration::from_secs(1);
    loop {
        match host_once(&ep, &raw, server, &id, &relays, &registered).await {
            Ok(()) => wait = Duration::from_secs(1),
            Err(e) => tracing::warn!("rendezvous: {e:#}"),
        }
        let _ = registered.send(None);
        tokio::time::sleep(wait).await;
        wait = (wait * 2).min(Duration::from_secs(30));
    }
}

async fn host_once(
    ep: &quinn::Endpoint,
    raw: &Arc<UdpSocket>,
    server: SocketAddr,
    id: &Identity,
    relays: &Arc<Mutex<Vec<SocketAddr>>>,
    registered: &tokio::sync::watch::Sender<Option<String>>,
) -> Result<()> {
    let mut rv = RvConn::connect(ep, server, id).await?;
    let port = ep.local_addr()?.port();
    loop {
        match rv.recv().await? {
            RvMsg::Challenge { nonce } => {
                let mut msg = REGISTER_CONTEXT.to_vec();
                msg.extend_from_slice(&nonce);
                rv.send(RvMsg::Register {
                    host_id: id.host_id(),
                    cert_der: id.cert.to_vec(),
                    signature: id.sign(&msg)?,
                    locals: local_candidates(port),
                });
            }
            RvMsg::Registered { observed } => {
                tracing::info!(%observed, "registered with the rendezvous server");
                let _ = registered.send(Some(observed));
            }
            RvMsg::Introduce { token, peer_observed, peer_locals, punch_in_ms, .. } => {
                let c = parse_candidates(&peer_observed, &peer_locals);
                tracing::info!(client = %peer_observed, "introduced; punching");
                let raw = raw.clone();
                tokio::spawn(async move { punch(&raw, &token, &c, Duration::from_millis(punch_in_ms as u64)).await });
            }
            RvMsg::RelayAllocated { token, relay_addr } => {
                let addr: SocketAddr = relay_addr.parse().context("bad relay address")?;
                relays.lock().expect("lock").push(addr);
                tracing::info!(%addr, "relay allocated; opening the path");
                // Hello every 500 ms for 10 s: the client may connect late.
                let raw = raw.clone();
                tokio::spawn(async move {
                    let p = punch_packet(&token);
                    for _ in 0..20 {
                        let _ = raw.send_to(&p, addr);
                        tokio::time::sleep(Duration::from_millis(500)).await;
                    }
                });
            }
            RvMsg::Error { text, .. } => bail!(text),
            _ => {}
        }
    }
}

/// How the client reached the host.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Path {
    Direct,
    Relayed,
}

/// Client side: connects to `host_id` through the rendezvous server.
pub async fn client_connect(
    ep: &quinn::Endpoint,
    raw: &Arc<UdpSocket>,
    server: SocketAddr,
    id: &Identity,
    host_id: &str,
) -> Result<(quinn::Connection, Path)> {
    let mut rv = RvConn::connect(ep, server, id).await?;
    let port = ep.local_addr()?.port();
    rv.send(RvMsg::Lookup { host_id: host_id.into(), locals: local_candidates(port) });
    let (token, candidates, delay) = loop {
        match tokio::time::timeout(Duration::from_secs(5), rv.recv())
            .await
            .context("no answer from the rendezvous server")??
        {
            RvMsg::Introduce { token, peer_observed, peer_locals, punch_in_ms, .. } => {
                break (
                    token,
                    parse_candidates(&peer_observed, &peer_locals),
                    Duration::from_millis(punch_in_ms as u64),
                );
            }
            RvMsg::Error { text, .. } => bail!(text),
            _ => {}
        }
    };
    {
        let (raw, c) = (raw.clone(), candidates.clone());
        tokio::spawn(async move { punch(&raw, &token, &c, delay).await });
    }
    // Dial every candidate once the host has started punching; first
    // handshake wins.
    tokio::time::sleep(delay + Duration::from_millis(40)).await;
    let direct = async {
        let mut set = tokio::task::JoinSet::new();
        for c in candidates {
            let connecting = ep.connect(c, "quickremote");
            set.spawn(async move { connecting?.await.map_err(anyhow::Error::from) });
        }
        while let Some(r) = set.join_next().await {
            if let Ok(Ok(conn)) = r {
                return Some(conn);
            }
        }
        None
    };
    if let Ok(Some(conn)) = tokio::time::timeout(DIRECT_TIMEOUT, direct).await {
        tracing::info!(addr = %conn.remote_address(), "direct path");
        return Ok((conn, Path::Direct));
    }

    tracing::info!("no direct path; asking for a relay");
    rv.send(RvMsg::RelayRequest { token });
    let relay: SocketAddr = loop {
        match tokio::time::timeout(Duration::from_secs(5), rv.recv()).await.context("no relay answer")?? {
            RvMsg::RelayAllocated { relay_addr, .. } => break relay_addr.parse().context("bad relay address")?,
            RvMsg::Error { text, .. } => bail!(text),
            _ => {}
        }
    };
    // Open our NAT toward the relay port, then dial the host through it.
    let p = punch_packet(&token);
    let hello = {
        let raw = raw.clone();
        tokio::spawn(async move {
            for _ in 0..40 {
                let _ = raw.send_to(&p, relay);
                tokio::time::sleep(Duration::from_millis(250)).await;
            }
        })
    };
    tokio::time::sleep(Duration::from_millis(300)).await;
    let conn = tokio::time::timeout(Duration::from_secs(10), ep.connect(relay, "quickremote")?)
        .await
        .context("connecting through the relay timed out")??;
    hello.abort();
    tracing::info!(%relay, "relayed path");
    Ok((conn, Path::Relayed))
}
