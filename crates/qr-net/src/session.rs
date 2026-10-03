//! Framed streams and the authenticated session handshake.
//!
//! Host side: accept the control stream, read `Hello`, check the version
//! and the client's certificate, answer `HelloAck`, and run PIN pairing if
//! the client is unknown. Client side: the mirror image, plus pinning the
//! host's fingerprint when it is already known.

use std::net::SocketAddr;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use qr_proto::msg::ErrorCode;
use qr_proto::{MAX_FRAME, Msg, PROTO_MAJOR, PROTO_MINOR};

use crate::identity::{Fingerprint, Identity, fingerprint, hex};
use crate::keystore::{KeyStore, Peer, now_unix};
use crate::pairing::{EXPORTER_LABEL, Pairing, PinLimiter, Transcript};
use crate::tls::peer_cert;

/// Writes length-prefixed frames to a QUIC stream.
pub struct FramedSend(pub quinn::SendStream);

impl FramedSend {
    pub async fn send_raw(&mut self, body: &[u8]) -> Result<()> {
        self.0.write_all(&(body.len() as u32).to_le_bytes()).await?;
        self.0.write_all(body).await?;
        Ok(())
    }

    pub async fn send(&mut self, m: &Msg) -> Result<()> {
        self.send_raw(&m.encode()).await
    }
}

/// Reads length-prefixed frames from a QUIC stream.
pub struct FramedRecv(pub quinn::RecvStream);

impl FramedRecv {
    /// Next frame, or `None` at a clean end of stream.
    pub async fn recv_raw(&mut self) -> Result<Option<Vec<u8>>> {
        let mut len = [0u8; 4];
        match self.0.read_exact(&mut len).await {
            Ok(()) => {}
            Err(quinn::ReadExactError::FinishedEarly(0)) => return Ok(None),
            Err(e) => return Err(e.into()),
        }
        let n = u32::from_le_bytes(len) as usize;
        if n > MAX_FRAME {
            bail!("frame of {n} bytes exceeds the limit");
        }
        let mut body = vec![0u8; n];
        self.0.read_exact(&mut body).await?;
        Ok(Some(body))
    }

    pub async fn recv(&mut self) -> Result<Msg> {
        let body = self.recv_raw().await?.ok_or_else(|| anyhow!("control stream closed"))?;
        Ok(Msg::decode(&body)?)
    }
}

/// A peer's `Error` message as a Rust error.
#[derive(Debug, thiserror::Error)]
#[error("{text}")]
pub struct PeerError {
    pub code: ErrorCode,
    pub text: String,
}

fn expect_no_error(m: Msg) -> Result<Msg> {
    match m {
        Msg::Error { code, text } => Err(PeerError { code: ErrorCode::from_u16(code), text }.into()),
        m => Ok(m),
    }
}

fn exporter(conn: &quinn::Connection) -> Result<[u8; 32]> {
    let mut out = [0u8; 32];
    conn.export_keying_material(&mut out, EXPORTER_LABEL, b"").map_err(|_| anyhow!("TLS exporter unavailable"))?;
    Ok(out)
}

const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(30);

/// Sends an `Error` and finishes the stream, then waits (briefly) until the
/// peer has read it, so closing the connection doesn't discard it.
async fn refuse(tx: &mut FramedSend, code: ErrorCode, text: &str) -> Result<()> {
    tx.send(&Msg::Error { code: code as u16, text: text.into() }).await?;
    let _ = tx.0.finish();
    let _ = tokio::time::timeout(Duration::from_secs(2), tx.0.stopped()).await;
    Ok(())
}

pub struct HostConfig<'a> {
    pub id: &'a Identity,
    pub name: &'a str,
    pub rendezvous: &'a str,
}

/// What the host learned about an authenticated client.
pub struct HostSession {
    pub control_tx: FramedSend,
    pub control_rx: FramedRecv,
    pub hello: Msg,
    pub client_fp: Fingerprint,
    pub client_name: String,
    pub paired_now: bool,
}

/// Host side of the handshake. `pin` is the current PIN; on a successful
/// pairing the caller should rotate it.
pub async fn host_handshake(
    conn: &quinn::Connection,
    cfg: &HostConfig<'_>,
    keystore: &std::sync::Mutex<KeyStore>,
    pin: &str,
    limiter: &std::sync::Mutex<PinLimiter>,
) -> Result<HostSession> {
    tokio::time::timeout(HANDSHAKE_TIMEOUT, host_handshake_inner(conn, cfg, keystore, pin, limiter))
        .await
        .context("handshake timed out")?
}

async fn host_handshake_inner(
    conn: &quinn::Connection,
    cfg: &HostConfig<'_>,
    keystore: &std::sync::Mutex<KeyStore>,
    pin: &str,
    limiter: &std::sync::Mutex<PinLimiter>,
) -> Result<HostSession> {
    let (tx, rx) = conn.accept_bi().await.context("waiting for the control stream")?;
    let mut tx = FramedSend(tx);
    let mut rx = FramedRecv(rx);
    let hello = rx.recv().await?;
    let Msg::Hello { major, ref name, .. } = hello else { bail!("expected Hello, got {hello:?}") };
    if major != PROTO_MAJOR {
        let text = format!(
            "This host runs protocol v{PROTO_MAJOR}; the client runs v{major}. Update the {}.",
            if major < PROTO_MAJOR { "client" } else { "host" }
        );
        refuse(&mut tx, ErrorCode::VersionMismatch, &text).await?;
        bail!(text);
    }
    let cert = peer_cert(conn).ok_or_else(|| anyhow!("client presented no certificate"))?;
    let client_fp = fingerprint(&cert);
    let client_name = name.clone();
    let paired = keystore.lock().expect("keystore lock").get(&client_fp).is_some();
    tx.send(&Msg::HelloAck {
        major: PROTO_MAJOR,
        minor: PROTO_MINOR,
        name: cfg.name.into(),
        host_id: cfg.id.host_id(),
        need_pin: !paired,
        rendezvous: cfg.rendezvous.into(),
    })
    .await?;
    if paired {
        return Ok(HostSession { control_tx: tx, control_rx: rx, hello, client_fp, client_name, paired_now: false });
    }

    let ip = conn.remote_address().ip();
    if !limiter.lock().expect("limiter lock").allowed(ip, Instant::now()) {
        let text = "Too many wrong PINs from this address. Wait a minute and try again.".to_string();
        refuse(&mut tx, ErrorCode::RateLimited, &text).await?;
        bail!(text);
    }
    let Msg::PairSpake { data: msg_a } = expect_no_error(rx.recv().await?)? else {
        bail!("expected PairSpake");
    };
    let (state, msg_b) = Pairing::host(pin);
    tx.send(&Msg::PairSpake { data: msg_b }).await?;
    let t = Transcript { client_fp, host_fp: cfg.id.fingerprint, exporter: exporter(conn)? };
    let keys = state.finish(&msg_a, &t);
    let Msg::PairConfirm { mac } = expect_no_error(rx.recv().await?)? else { bail!("expected PairConfirm") };
    let Some(keys) = keys.filter(|k| k.check(true, &mac)) else {
        limiter.lock().expect("limiter lock").failure(ip, Instant::now());
        let text = "That PIN doesn't match. Check the code on the host and try again.".to_string();
        refuse(&mut tx, ErrorCode::PinWrong, &text).await?;
        bail!("client {ip} sent a wrong PIN");
    };
    tx.send(&Msg::PairConfirm { mac: keys.host_mac() }).await?;
    limiter.lock().expect("limiter lock").success(ip);
    keystore.lock().expect("keystore lock").upsert(Peer {
        fingerprint: hex(&client_fp),
        name: client_name.clone(),
        host_id: String::new(),
        added: now_unix(),
        last_addr: conn.remote_address().to_string(),
        rendezvous: String::new(),
        clipboard: true,
    })?;
    Ok(HostSession { control_tx: tx, control_rx: rx, hello, client_fp, client_name, paired_now: true })
}

pub struct ClientSession {
    pub control_tx: FramedSend,
    pub control_rx: FramedRecv,
    pub host_name: String,
    pub host_id: String,
    pub host_fp: Fingerprint,
    pub rendezvous: String,
    pub paired_now: bool,
}

#[derive(Debug, thiserror::Error)]
pub enum ClientAuthError {
    #[error("this host is not paired yet; enter the PIN shown on the host")]
    PinNeeded,
    #[error(
        "the host's identity changed (expected {expected}, got {got}); \
         it may be a different machine. Remove it and pair again if this is expected"
    )]
    HostKeyMismatch { expected: String, got: String },
    #[error(
        "the host says this client is paired, but this client doesn't know the host; remove the client on the host and pair again"
    )]
    UnknownHost,
}

/// Client side. `expect_fp` pins the host when connecting to a known host
/// (by ID or from the key store).
pub async fn client_handshake(
    conn: &quinn::Connection,
    my_fp: Fingerprint,
    hello: Msg,
    keystore: &std::sync::Mutex<KeyStore>,
    pin: Option<&str>,
    expect_fp: Option<Fingerprint>,
) -> Result<ClientSession> {
    tokio::time::timeout(HANDSHAKE_TIMEOUT, client_handshake_inner(conn, my_fp, hello, keystore, pin, expect_fp))
        .await
        .context("handshake timed out")?
}

async fn client_handshake_inner(
    conn: &quinn::Connection,
    my_fp: Fingerprint,
    hello: Msg,
    keystore: &std::sync::Mutex<KeyStore>,
    pin: Option<&str>,
    expect_fp: Option<Fingerprint>,
) -> Result<ClientSession> {
    let cert = peer_cert(conn).ok_or_else(|| anyhow!("host presented no certificate"))?;
    let host_fp = fingerprint(&cert);
    if let Some(e) = expect_fp
        && e != host_fp
    {
        return Err(ClientAuthError::HostKeyMismatch { expected: hex(&e), got: hex(&host_fp) }.into());
    }
    let (tx, rx) = conn.open_bi().await?;
    let mut tx = FramedSend(tx);
    let mut rx = FramedRecv(rx);
    tx.send(&hello).await?;
    let ack = expect_no_error(rx.recv().await?)?;
    let Msg::HelloAck { name, host_id, need_pin, rendezvous, .. } = ack else {
        bail!("expected HelloAck, got {ack:?}")
    };
    let known = keystore.lock().expect("keystore lock").get(&host_fp).is_some();
    let session = |tx, rx, paired_now| ClientSession {
        control_tx: tx,
        control_rx: rx,
        host_name: name.clone(),
        host_id: host_id.clone(),
        host_fp,
        rendezvous: rendezvous.clone(),
        paired_now,
    };
    if !need_pin {
        if !known {
            return Err(ClientAuthError::UnknownHost.into());
        }
        let mut ks = keystore.lock().expect("keystore lock");
        if let Some(p) = ks.get_mut(&host_fp) {
            p.last_addr = conn.remote_address().to_string();
            p.rendezvous = rendezvous.clone();
            p.name = name.clone();
            let _ = ks.save();
        }
        return Ok(session(tx, rx, false));
    }
    let Some(pin) = pin else {
        let _ = tx.0.finish();
        return Err(ClientAuthError::PinNeeded.into());
    };
    let (state, msg_a) = Pairing::client(pin);
    tx.send(&Msg::PairSpake { data: msg_a }).await?;
    let Msg::PairSpake { data: msg_b } = expect_no_error(rx.recv().await?)? else { bail!("expected PairSpake") };
    let t = Transcript { client_fp: my_fp, host_fp, exporter: exporter(conn)? };
    let keys = state.finish(&msg_b, &t).ok_or_else(|| anyhow!("pairing failed"))?;
    tx.send(&Msg::PairConfirm { mac: keys.client_mac() }).await?;
    let Msg::PairConfirm { mac } = expect_no_error(rx.recv().await?)? else { bail!("expected PairConfirm") };
    if !keys.check(false, &mac) {
        bail!("the host failed pairing confirmation; the connection may be intercepted");
    }
    keystore.lock().expect("keystore lock").upsert(Peer {
        fingerprint: hex(&host_fp),
        name: name.clone(),
        host_id: host_id.clone(),
        added: now_unix(),
        last_addr: conn.remote_address().to_string(),
        rendezvous: rendezvous.clone(),
        clipboard: true,
    })?;
    Ok(session(tx, rx, true))
}

/// The peer's address, for logs and the key store.
pub fn describe(conn: &quinn::Connection) -> SocketAddr {
    conn.remote_address()
}
