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
    // Unacknowledged stream data quinn may buffer. A big backlog of file
    // data queues in front of video datagrams; 4 MiB still covers
    // 1 Gbit/s × 30 ms.
    t.send_window(4 * 1024 * 1024);
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
    set_buffers(&s);
    s.bind(&addr.into()).with_context(|| format!("binding UDP {addr}"))?;
    s.set_nonblocking(true)?;
    Ok(s.into())
}

/// Asks for [`BUFFER`]-sized kernel socket buffers. Linux caps the request
/// at `net.core.rmem_max` (often 208 KiB), and a buffer that small drops
/// packets whenever a burst arrives (measured: 13k drops during a file
/// transfer next to a 1080p60 stream). `SO_RCVBUFFORCE` bypasses the cap
/// where allowed (CAP_NET_ADMIN); otherwise the shortfall is logged with
/// the fix.
fn set_buffers(s: &socket2::Socket) {
    #[cfg(target_os = "linux")]
    {
        use std::os::fd::AsRawFd;
        let v = BUFFER as libc::c_int;
        for opt in [libc::SO_RCVBUFFORCE, libc::SO_SNDBUFFORCE] {
            // SAFETY: setsockopt reads one c_int from `v` on a valid socket.
            unsafe {
                libc::setsockopt(
                    s.as_raw_fd(),
                    libc::SOL_SOCKET,
                    opt,
                    &v as *const _ as *const libc::c_void,
                    std::mem::size_of::<libc::c_int>() as libc::socklen_t,
                );
            }
        }
    }
    if s.recv_buffer_size().unwrap_or(0) < BUFFER {
        let _ = s.set_recv_buffer_size(BUFFER);
    }
    if s.send_buffer_size().unwrap_or(0) < BUFFER {
        let _ = s.set_send_buffer_size(BUFFER);
    }
    // Linux reports double the usable size.
    let got = s.recv_buffer_size().unwrap_or(0) / if cfg!(target_os = "linux") { 2 } else { 1 };
    if got < BUFFER / 2 {
        tracing::warn!(
            "UDP receive buffer is {} KiB; bursts will drop packets. Raise the limit: \
             sudo sysctl -w net.core.rmem_max=16777216 net.core.wmem_max=16777216",
            got / 1024
        );
    }
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
