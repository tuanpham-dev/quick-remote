//! Web client gateway: serves the page and a WebTransport endpoint on the
//! same port (TCP for the page, UDP for WebTransport).
//!
//! A browser authenticates on its first bidirectional stream: with a
//! WebCrypto key enrolled earlier (it signs a nonce), or by PIN pairing
//! (the same SPAKE2 as native clients, run in WebAssembly), which enrols
//! its key. The gateway then opens an ordinary QUIC session to this host
//! on loopback with its own trusted identity and pipes the browser's
//! control stream, uni streams and datagrams through, so the session code
//! serves browsers and native clients alike.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use qr_net::keystore::{KeyStore, Peer, now_unix};
use qr_net::pairing::{Pairing, PinLimiter, Transcript};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[derive(rust_embed::Embed)]
#[folder = "../../web/dist"]
struct Assets;

/// The WebTransport certificate: ECDSA P-256, valid 13 days (browsers
/// accept a pinned self-signed certificate only for at most 14).
pub struct WebCert {
    pub der: Vec<u8>,
    pub key: Vec<u8>,
    pub hash: [u8; 32],
}

pub fn make_cert(names: Vec<String>) -> Result<WebCert> {
    let key = rcgen::KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256)?;
    let mut p = rcgen::CertificateParams::new(names)?;
    let now = time::OffsetDateTime::now_utc();
    p.not_before = now - time::Duration::hours(1);
    p.not_after = now + time::Duration::days(13);
    let cert = p.self_signed(&key)?;
    let der = cert.der().to_vec();
    let hash = Sha256::digest(&der).into();
    Ok(WebCert { der, key: key.serialize_der(), hash })
}

/// A browser session as the host should see it.
#[derive(Debug, Clone, Copy)]
pub struct WebPeer {
    pub addr: SocketAddr,
    pub rtt: Duration,
    /// Largest WebTransport datagram to the browser: video shards must fit.
    pub max_datagram: usize,
}

/// Browser sessions currently piped, by the gateway's local port, so the
/// host judges the real path (not loopback) and sizes datagrams for it.
#[derive(Default)]
pub struct WebPeers(pub Mutex<HashMap<u16, WebPeer>>);

pub struct Gateway {
    pub cert: WebCert,
    pub host_addr: SocketAddr,
    pub id: qr_net::Identity,
    pub web_keys: Mutex<KeyStore>,
    pub pin: Arc<Mutex<String>>,
    pub fixed_pin: Option<String>,
    pub limiter: Arc<Mutex<PinLimiter>>,
    pub peers: Arc<WebPeers>,
}

pub async fn run(gw: Arc<Gateway>, listen: SocketAddr) -> Result<()> {
    // The page: plain HTTP on loopback (a secure context already), HTTPS
    // with the same certificate elsewhere.
    let tcp = tokio::net::TcpListener::bind(listen).await.with_context(|| format!("binding TCP {listen}"))?;
    let tls = if listen.ip().is_loopback() {
        None
    } else {
        let cfg = rustls::ServerConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
            .with_safe_default_protocol_versions()?
            .with_no_client_auth()
            .with_single_cert(
                vec![gw.cert.der.clone().into()],
                rustls::pki_types::PrivateKeyDer::Pkcs8(gw.cert.key.clone().into()),
            )?;
        Some(tokio_rustls::TlsAcceptor::from(Arc::new(cfg)))
    };
    tokio::spawn(async move {
        loop {
            let Ok((s, _)) = tcp.accept().await else { continue };
            let tls = tls.clone();
            tokio::spawn(async move {
                let r = match tls {
                    Some(t) => match t.accept(s).await {
                        Ok(s) => serve_http(s).await,
                        Err(e) => Err(e.into()),
                    },
                    None => serve_http(s).await,
                };
                if let Err(e) = r {
                    tracing::debug!("http: {e:#}");
                }
            });
        }
    });

    let identity = wtransport::Identity::new(
        wtransport::tls::CertificateChain::new(vec![wtransport::tls::Certificate::from_der(gw.cert.der.clone())?]),
        wtransport::tls::PrivateKey::from_der_pkcs8(gw.cert.key.clone()),
    );
    let cfg = wtransport::ServerConfig::builder()
        .with_bind_address(listen)
        .with_identity(identity)
        .keep_alive_interval(Some(Duration::from_secs(1)))
        .build();
    let ep = wtransport::Endpoint::server(cfg)?;
    loop {
        let incoming = ep.accept().await;
        let gw = gw.clone();
        tokio::spawn(async move {
            let r = async {
                let req = incoming.await?;
                let conn = req.accept().await?;
                session(gw, conn).await
            }
            .await;
            if let Err(e) = r {
                tracing::info!("web session ended: {e:#}");
            }
        });
    }
}

async fn serve_http<S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin>(mut s: S) -> Result<()> {
    let mut buf = vec![0u8; 8192];
    let n = s.read(&mut buf).await?;
    let req = String::from_utf8_lossy(&buf[..n]);
    let path = req.split_whitespace().nth(1).unwrap_or("/");
    let path = path.split('?').next().unwrap_or("/").trim_start_matches('/');
    let path = if path.is_empty() { "index.html" } else { path };
    let (status, ctype, body) = match Assets::get(path) {
        Some(f) => {
            let ctype = match Path::new(path).extension().and_then(|e| e.to_str()) {
                Some("html") => "text/html; charset=utf-8",
                Some("js") => "text/javascript",
                Some("wasm") => "application/wasm",
                Some("css") => "text/css",
                _ => "application/octet-stream",
            };
            ("200 OK", ctype, f.data.into_owned())
        }
        None => ("404 Not Found", "text/plain", b"not found".to_vec()),
    };
    let head = format!(
        "HTTP/1.1 {status}\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\nCache-Control: no-store\r\n\
         Cross-Origin-Opener-Policy: same-origin\r\nConnection: close\r\n\r\n",
        body.len()
    );
    s.write_all(head.as_bytes()).await?;
    s.write_all(&body).await?;
    s.shutdown().await?;
    Ok(())
}

async fn send_json<W: tokio::io::AsyncWrite + Unpin>(w: &mut W, v: &Value) -> Result<()> {
    let b = serde_json::to_vec(v)?;
    w.write_all(&(b.len() as u32).to_le_bytes()).await?;
    w.write_all(&b).await?;
    Ok(())
}

async fn recv_json<R: tokio::io::AsyncRead + Unpin>(r: &mut R) -> Result<Value> {
    let n = r.read_u32_le().await? as usize;
    if n > 64 * 1024 {
        bail!("auth message too large");
    }
    let mut b = vec![0u8; n];
    r.read_exact(&mut b).await?;
    Ok(serde_json::from_slice(&b)?)
}

fn b64(v: &Value, k: &str) -> Result<Vec<u8>> {
    Ok(B64.decode(v[k].as_str().ok_or_else(|| anyhow!("missing {k}"))?)?)
}

/// The uncompressed P-256 point at the end of a SubjectPublicKeyInfo.
fn spki_point(spki: &[u8]) -> Result<&[u8]> {
    if spki.len() < 65 || spki[spki.len() - 65] != 4 {
        bail!("not a P-256 public key");
    }
    Ok(&spki[spki.len() - 65..])
}

/// Authenticates the browser on its first stream; returns its key id.
async fn authenticate(gw: &Gateway, conn: &wtransport::Connection) -> Result<String> {
    let (mut tx, mut rx) = conn.accept_bi().await?;
    let mut nonce = [0u8; 32];
    rand::RngCore::fill_bytes(&mut rand::thread_rng(), &mut nonce);
    send_json(&mut tx, &json!({"t": "hello", "nonce": B64.encode(nonce), "hostFp": B64.encode(gw.cert.hash)})).await?;
    let m = recv_json(&mut rx).await?;
    let spki = b64(&m, "spki")?;
    let point = spki_point(&spki)?.to_vec();
    let fp: [u8; 32] = Sha256::digest(&spki).into();
    let key_id = qr_net::identity::hex(&fp);
    match m["t"].as_str() {
        Some("key") => {
            let sig = b64(&m, "sig")?;
            let mut msg = nonce.to_vec();
            msg.extend_from_slice(&gw.cert.hash);
            let ok = ring::signature::UnparsedPublicKey::new(&ring::signature::ECDSA_P256_SHA256_FIXED, &point)
                .verify(&msg, &sig)
                .is_ok();
            let known = gw.web_keys.lock().expect("lock").get(&fp).is_some();
            if !(ok && known) {
                send_json(&mut tx, &json!({"t": "error", "code": "unpaired", "text": "This browser isn't paired with the host. Enter the PIN shown on the host."})).await?;
                bail!("unknown or invalid browser key");
            }
        }
        Some("pair") => {
            let ip = conn.remote_address().ip();
            if !gw.limiter.lock().expect("lock").allowed(ip, Instant::now()) {
                send_json(&mut tx, &json!({"t": "error", "code": "rate", "text": "Too many wrong PINs from this address. Wait a minute and try again."})).await?;
                bail!("rate limited");
            }
            let pin = gw.pin.lock().expect("lock").clone();
            let (state, msg_b) = Pairing::host(&pin);
            send_json(&mut tx, &json!({"t": "pair", "msg": B64.encode(&msg_b)})).await?;
            let t = Transcript { client_fp: fp, host_fp: gw.cert.hash, exporter: [0; 32] };
            let keys = state.finish(&b64(&m, "msg")?, &t);
            let c = recv_json(&mut rx).await?;
            let mac: [u8; 32] = b64(&c, "mac")?.try_into().map_err(|_| anyhow!("bad mac"))?;
            let Some(keys) = keys.filter(|k| k.check(true, &mac)) else {
                gw.limiter.lock().expect("lock").failure(ip, Instant::now());
                send_json(&mut tx, &json!({"t": "error", "code": "pin", "text": "That PIN doesn't match. Check the code on the host and try again."})).await?;
                bail!("wrong PIN from {ip}");
            };
            send_json(&mut tx, &json!({"t": "confirm", "mac": B64.encode(keys.host_mac())})).await?;
            gw.limiter.lock().expect("lock").success(ip);
            gw.web_keys.lock().expect("lock").upsert(Peer {
                fingerprint: key_id.clone(),
                name: m["name"].as_str().unwrap_or("browser").to_string(),
                host_id: String::new(),
                added: now_unix(),
                last_addr: conn.remote_address().to_string(),
                rendezvous: String::new(),
                clipboard: true,
            })?;
            if gw.fixed_pin.is_none() {
                let fresh = qr_net::pairing::new_pin();
                println!("PIN {fresh}");
                *gw.pin.lock().expect("lock") = fresh;
            }
        }
        _ => bail!("unexpected auth message"),
    }
    send_json(&mut tx, &json!({"t": "ok"})).await?;
    let _ = tx.finish().await;
    Ok(key_id)
}

async fn session(gw: Arc<Gateway>, wt: wtransport::Connection) -> Result<()> {
    let browser = wt.remote_address();
    let key = authenticate(&gw, &wt).await?;
    tracing::info!(%browser, key = &key[..12], "browser authenticated");

    // A native session to this host on loopback.
    let bind: SocketAddr = "127.0.0.1:0".parse()?;
    let socket = qr_net::endpoint::bind_socket(bind)?;
    let mut ep = quinn::Endpoint::new(quinn::EndpointConfig::default(), None, socket, Arc::new(quinn::TokioRuntime))?;
    ep.set_default_client_config(qr_net::endpoint::client_config(Some(&gw.id), &[qr_proto::ALPN])?);
    let local_port = ep.local_addr()?.port();
    // Registered before connecting: the host reads it at session start.
    // WebTransport adds a quarter-stream-ID prefix to each datagram.
    let max_datagram = wt.max_datagram_size().unwrap_or(1100).saturating_sub(8);
    gw.peers.0.lock().expect("lock").insert(local_port, WebPeer { addr: browser, rtt: wt.rtt(), max_datagram });
    let host = match ep.connect(gw.host_addr, "quickremote")?.await {
        Ok(h) => h,
        Err(e) => {
            gw.peers.0.lock().expect("lock").remove(&local_port);
            return Err(e).context("connecting to the host");
        }
    };

    let r = pipe(&wt, &host, &gw.peers, local_port).await;
    gw.peers.0.lock().expect("lock").remove(&local_port);
    host.close(0u32.into(), b"browser left");
    wt.close(0u32.into(), b"");
    r
}

async fn pipe(wt: &wtransport::Connection, host: &quinn::Connection, peers: &WebPeers, port: u16) -> Result<()> {
    // Control: the browser's next bidirectional stream.
    let (b_tx, b_rx) = wt.accept_bi().await?;
    let (h_tx, h_rx) = host.open_bi().await?;
    let mut tasks = tokio::task::JoinSet::new();
    tasks.spawn(copy(b_rx, h_tx));
    tasks.spawn(copy(h_rx, b_tx));
    // Browser → host unidirectional streams (input, files).
    {
        let (wt, host) = (wt.clone(), host.clone());
        tasks.spawn(async move {
            while let Ok(r) = wt.accept_uni().await {
                let host = host.clone();
                tokio::spawn(async move {
                    if let Ok(w) = host.open_uni().await {
                        let _ = copy(r, w).await;
                    }
                });
            }
            Ok(())
        });
    }
    // Host → browser unidirectional streams (files).
    {
        let (wt, host) = (wt.clone(), host.clone());
        tasks.spawn(async move {
            while let Ok(r) = host.accept_uni().await {
                let wt = wt.clone();
                tokio::spawn(async move {
                    if let Ok(o) = wt.open_uni().await
                        && let Ok(w) = o.await
                    {
                        let _ = copy(r, w).await;
                    }
                });
            }
            Ok(())
        });
    }
    // Datagrams both ways.
    {
        let (wt, host) = (wt.clone(), host.clone());
        tasks.spawn(async move {
            loop {
                let d = host.read_datagram().await?;
                let _ = wt.send_datagram(d);
            }
        });
    }
    {
        let (wt, host) = (wt.clone(), host.clone());
        tasks.spawn(async move {
            loop {
                let d = wt.receive_datagram().await?;
                let _ = host.send_datagram(d.payload());
            }
        });
    }
    // Keep the host's view of the browser's RTT current.
    let rtt = async {
        loop {
            tokio::time::sleep(Duration::from_millis(500)).await;
            if let Some(e) = peers.0.lock().expect("lock").get_mut(&port) {
                e.rtt = wt.rtt();
            }
        }
    };
    tokio::select! {
        r = tasks.join_next() => {
            tasks.abort_all();
            match r {
                Some(Ok(Err(e))) => Err(e),
                _ => Ok(()),
            }
        }
        _ = rtt => Ok(()),
    }
}

async fn copy<R, W>(mut r: R, mut w: W) -> Result<()>
where
    R: tokio::io::AsyncRead + Unpin,
    W: tokio::io::AsyncWrite + Unpin,
{
    tokio::io::copy(&mut r, &mut w).await?;
    w.shutdown().await?;
    Ok(())
}

/// Opens the gateway identity and makes the host trust it.
pub fn gateway_identity(dir: &Path, keystore: &Mutex<KeyStore>) -> Result<qr_net::Identity> {
    let id = qr_net::Identity::load_or_create(&dir.join("gateway"), "web gateway")?;
    let mut ks = keystore.lock().expect("lock");
    if ks.get(&id.fingerprint).is_none() {
        ks.upsert(Peer {
            fingerprint: qr_net::identity::hex(&id.fingerprint),
            name: "web gateway".into(),
            host_id: String::new(),
            added: now_unix(),
            last_addr: String::new(),
            rendezvous: String::new(),
            clipboard: true,
        })?;
    }
    Ok(id)
}
