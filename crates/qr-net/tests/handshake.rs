//! Loopback tests of the session handshake and datagram path.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use qr_net::keystore::KeyStore;
use qr_net::pairing::PinLimiter;
use qr_net::session::{ClientAuthError, HostConfig, PeerError, client_handshake, host_handshake};
use qr_net::{Identity, endpoint};
use qr_proto::msg::ErrorCode;
use qr_proto::{ALPN, Msg, PROTO_MAJOR};

fn hello(major: u16) -> Msg {
    Msg::Hello {
        major,
        minor: 0,
        name: "test-client".into(),
        os: "linux".into(),
        want_width: 0,
        want_height: 0,
        want_refresh: 0,
        decoders: vec![],
    }
}

struct Host {
    addr: std::net::SocketAddr,
    keystore: Arc<Mutex<KeyStore>>,
    results: tokio::sync::mpsc::UnboundedReceiver<Result<bool, String>>,
    id: Identity,
}

fn start_host(pin: &'static str) -> Host {
    qr_net::init_crypto();
    let id = Identity::generate("host").unwrap();
    let ep = endpoint::endpoint("127.0.0.1:0".parse().unwrap(), &id, true, &[ALPN]).unwrap();
    let addr = ep.local_addr().unwrap();
    let keystore = Arc::new(Mutex::new(KeyStore::in_memory()));
    let limiter = Arc::new(Mutex::new(PinLimiter::default()));
    let (tx, results) = tokio::sync::mpsc::unbounded_channel();
    let (ks, hid) = (keystore.clone(), id.clone());
    tokio::spawn(async move {
        while let Some(inc) = ep.accept().await {
            let conn = inc.await.unwrap();
            let cfg = HostConfig { id: &hid, name: "host", rendezvous: "" };
            let r = host_handshake(&conn, &cfg, &ks, pin, &limiter).await;
            let r = match r {
                Ok(mut s) => {
                    s.control_tx.send(&Msg::SessionStart { profile: 1, path: 1, modes: 0 }).await.unwrap();
                    // Echo datagrams back until the client goes away.
                    let c2 = conn.clone();
                    tokio::spawn(async move {
                        while let Ok(d) = c2.read_datagram().await {
                            let _ = c2.send_datagram(d);
                        }
                    });
                    Ok(s.paired_now)
                }
                Err(e) => Err(e.to_string()),
            };
            tx.send(r).unwrap();
        }
    });
    Host { addr, keystore, results, id }
}

async fn connect(
    host: &Host,
    client: &Identity,
    ks: &Mutex<KeyStore>,
    pin: Option<&str>,
    expect: Option<[u8; 32]>,
    major: u16,
) -> anyhow::Result<(quinn::Connection, qr_net::session::ClientSession)> {
    let ep = endpoint::endpoint("127.0.0.1:0".parse().unwrap(), client, false, &[ALPN])?;
    let conn = ep.connect(host.addr, "quickremote")?.await?;
    let s = client_handshake(&conn, client.fingerprint, hello(major), ks, pin, expect).await?;
    Ok((conn, s))
}

#[tokio::test]
async fn pair_then_reconnect_without_pin() {
    let mut host = start_host("424242");
    let client = Identity::generate("client").unwrap();
    let ks = Mutex::new(KeyStore::in_memory());

    // No PIN for an unpaired host.
    let e = connect(&host, &client, &ks, None, None, PROTO_MAJOR).await.err().expect("should fail");
    assert!(matches!(e.downcast_ref::<ClientAuthError>(), Some(ClientAuthError::PinNeeded)), "{e}");
    assert!(host.results.recv().await.unwrap().is_err());

    // Wrong PIN.
    let e = connect(&host, &client, &ks, Some("000000"), None, PROTO_MAJOR).await.err().expect("should fail");
    let pe = e.downcast_ref::<PeerError>().unwrap_or_else(|| panic!("{e:#}"));
    assert_eq!(pe.code, ErrorCode::PinWrong);
    assert!(host.results.recv().await.unwrap().is_err());

    // Right PIN pairs both ways.
    let (_c, s) = connect(&host, &client, &ks, Some("424242"), None, PROTO_MAJOR).await.unwrap();
    assert!(s.paired_now);
    assert_eq!(host.results.recv().await.unwrap(), Ok(true));
    assert!(ks.lock().unwrap().get(&host.id.fingerprint).is_some());
    assert!(host.keystore.lock().unwrap().get(&client.fingerprint).is_some());

    // Reconnect: no PIN asked, host pinned.
    let (conn, mut s) = connect(&host, &client, &ks, None, Some(host.id.fingerprint), PROTO_MAJOR).await.unwrap();
    assert!(!s.paired_now);
    assert_eq!(host.results.recv().await.unwrap(), Ok(false));
    assert!(matches!(s.control_rx.recv().await.unwrap(), Msg::SessionStart { .. }));

    // Datagrams flow: 10,000 round trips of 1 KB.
    assert!(conn.max_datagram_size().unwrap() >= 1100);
    let mut got = 0;
    for i in 0..10_000u32 {
        let mut d = vec![0u8; 1000];
        d[..4].copy_from_slice(&i.to_le_bytes());
        conn.send_datagram(d.into()).unwrap();
        if i % 64 == 63 {
            while got <= i - 32 {
                let r = tokio::time::timeout(Duration::from_secs(2), conn.read_datagram()).await;
                match r {
                    Ok(Ok(_)) => got += 1,
                    _ => break,
                }
            }
        }
    }
    assert!(got > 9_000, "only {got} datagrams echoed");
}

#[tokio::test]
async fn wrong_host_key_aborts() {
    let host = start_host("111111");
    let client = Identity::generate("client").unwrap();
    let ks = Mutex::new(KeyStore::in_memory());
    let e =
        connect(&host, &client, &ks, Some("111111"), Some([0xee; 32]), PROTO_MAJOR).await.err().expect("should fail");
    assert!(matches!(e.downcast_ref::<ClientAuthError>(), Some(ClientAuthError::HostKeyMismatch { .. })), "{e}");
}

#[tokio::test]
async fn version_mismatch_is_refused() {
    let mut host = start_host("111111");
    let client = Identity::generate("client").unwrap();
    let ks = Mutex::new(KeyStore::in_memory());
    let e = connect(&host, &client, &ks, Some("111111"), None, 2).await.err().expect("should fail");
    let pe = e.downcast_ref::<PeerError>().unwrap_or_else(|| panic!("{e:#}"));
    assert_eq!(pe.code, ErrorCode::VersionMismatch);
    assert!(pe.text.contains("v1") && pe.text.contains("v2"), "{}", pe.text);
    assert!(host.results.recv().await.unwrap().is_err());
}

#[tokio::test]
async fn rate_limit_after_five_wrong_pins() {
    let mut host = start_host("999999");
    let client = Identity::generate("client").unwrap();
    let ks = Mutex::new(KeyStore::in_memory());
    for _ in 0..5 {
        let _ = connect(&host, &client, &ks, Some("000000"), None, PROTO_MAJOR).await;
        let _ = host.results.recv().await;
    }
    let e = connect(&host, &client, &ks, Some("999999"), None, PROTO_MAJOR).await.err().expect("should fail");
    let pe = e.downcast_ref::<PeerError>().unwrap_or_else(|| panic!("{e:#}"));
    assert_eq!(pe.code, ErrorCode::RateLimited);
}
