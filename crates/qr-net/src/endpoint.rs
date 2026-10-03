//! quinn endpoint tuned for media: datagrams on, BBR, large buffers.

use std::net::{SocketAddr, UdpSocket};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use quinn::crypto::rustls::{QuicClientConfig, QuicServerConfig};

use crate::identity::Identity;
use crate::tls;

/// Socket and QUIC buffer sizes. 8 MiB holds ~70 ms of a 1 Gbit/s stream.
pub const BUFFER: usize = 8 * 1024 * 1024;

pub fn transport_config() -> quinn::TransportConfig {
    let mut t = quinn::TransportConfig::default();
    t.max_idle_timeout(Some(Duration::from_secs(5).try_into().expect("in range")));
    t.keep_alive_interval(Some(Duration::from_secs(1)));
    t.datagram_receive_buffer_size(Some(BUFFER));
    t.datagram_send_buffer_size(BUFFER);
    t.stream_receive_window((16u32 * 1024 * 1024).into());
    t.receive_window((64u32 * 1024 * 1024).into());
    t.send_window(64 * 1024 * 1024);
    t.max_concurrent_uni_streams(64u32.into());
    t.max_concurrent_bidi_streams(8u32.into());
    t.initial_rtt(Duration::from_millis(10));
    // Fixed 1200-byte packets. With path-MTU discovery, random loss can
    // trigger black-hole detection, which lowers the MTU and silently drops
    // queued video datagrams sized for the larger one (measured: half of
    // every frame lost at 5% random loss). A 20% higher packet rate on
    // gigabit LANs is the cheaper price.
    t.mtu_discovery_config(None);
    // QR_GSO=0 turns off UDP segmentation offload. Tests need it: netem
    // drops a GSO batch (up to ~10 datagrams) as one packet, turning random
    // loss into bursts that real links don't have.
    if std::env::var("QR_GSO").is_ok_and(|v| v == "0") {
        t.enable_segmentation_offload(false);
    }
    let mut bbr = quinn::congestion::BbrConfig::default();
    bbr.initial_window(1024 * 1024);
    t.congestion_controller_factory(Arc::new(bbr));
    t
}

/// Binds a UDP socket with large kernel buffers.
pub fn bind_socket(addr: SocketAddr) -> Result<UdpSocket> {
    use socket2::{Domain, Protocol, Socket, Type};
    let s = Socket::new(Domain::for_address(addr), Type::DGRAM, Some(Protocol::UDP))?;
    if addr.is_ipv6() {
        // Dual-stack, so one socket serves IPv4 and IPv6 peers.
        let _ = s.set_only_v6(false);
    }
    let _ = s.set_recv_buffer_size(BUFFER);
    let _ = s.set_send_buffer_size(BUFFER);
    s.bind(&addr.into()).with_context(|| format!("binding UDP {addr}"))?;
    s.set_nonblocking(true)?;
    Ok(s.into())
}

pub fn server_config(id: &Identity, alpn: &[&[u8]]) -> Result<quinn::ServerConfig> {
    let tls = tls::server_config(id, alpn)?;
    let mut cfg = quinn::ServerConfig::with_crypto(Arc::new(QuicServerConfig::try_from(tls)?));
    cfg.transport_config(Arc::new(transport_config()));
    // Clients behind NAT rebinding keep their session.
    cfg.migration(true);
    Ok(cfg)
}

pub fn client_config(id: Option<&Identity>, alpn: &[&[u8]]) -> Result<quinn::ClientConfig> {
    let tls = tls::client_config(id, alpn)?;
    let mut cfg = quinn::ClientConfig::new(Arc::new(QuicClientConfig::try_from(tls)?));
    cfg.transport_config(Arc::new(transport_config()));
    Ok(cfg)
}

/// An endpoint that accepts (when `server` is set) and connects, with the
/// device identity as its certificate both ways.
pub fn endpoint(addr: SocketAddr, id: &Identity, server: bool, alpn: &[&[u8]]) -> Result<quinn::Endpoint> {
    let socket = bind_socket(addr)?;
    let server_cfg = if server { Some(server_config(id, alpn)?) } else { None };
    let mut ep =
        quinn::Endpoint::new(quinn::EndpointConfig::default(), server_cfg, socket, Arc::new(quinn::TokioRuntime))?;
    ep.set_default_client_config(client_config(Some(id), alpn)?);
    Ok(ep)
}
