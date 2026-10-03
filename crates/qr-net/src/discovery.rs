//! LAN discovery over mDNS/DNS-SD: `_quickremote._udp.local.`

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};

use anyhow::Result;
use mdns_sd::{ServiceDaemon, ServiceEvent, ServiceInfo};

pub const SERVICE: &str = "_quickremote._udp.local.";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostInfo {
    pub name: String,
    pub host_id: String,
    pub os: String,
    pub proto: String,
    /// QUIC address(es), IPv4 first.
    pub addrs: Vec<SocketAddr>,
    /// Web client port, 0 when off.
    pub web_port: u16,
    /// The mDNS instance name (removal events refer to it).
    pub instance: String,
}

pub struct Advertiser {
    daemon: ServiceDaemon,
    fullname: String,
}

impl Advertiser {
    /// Starts advertising this host on every interface.
    pub fn start(name: &str, host_id: &str, os: &str, port: u16, web_port: u16) -> Result<Self> {
        let daemon = ServiceDaemon::new()?;
        let hostname = format!("{}.local.", host_id);
        let props = [
            ("id", host_id.to_string()),
            ("name", name.to_string()),
            ("os", os.to_string()),
            ("proto", qr_proto::PROTO_MAJOR.to_string()),
            ("web", web_port.to_string()),
        ];
        // The instance name must be unique on the link: the host ID is.
        let info =
            ServiceInfo::new(SERVICE, host_id, &hostname, (), port, &props[..])?.enable_addr_auto();
        let fullname = info.get_fullname().to_string();
        daemon.register(info)?;
        Ok(Self { daemon, fullname })
    }
}

impl Drop for Advertiser {
    fn drop(&mut self) {
        let _ = self.daemon.unregister(&self.fullname);
        let _ = self.daemon.shutdown();
    }
}

#[derive(Debug, Clone)]
pub enum Discovery {
    Up(HostInfo),
    Down { instance: String },
}

/// Browses for hosts; events arrive on the returned channel until the
/// `Browser` is dropped.
pub struct Browser {
    daemon: ServiceDaemon,
}

impl Browser {
    pub fn start() -> Result<(Self, tokio::sync::mpsc::UnboundedReceiver<Discovery>)> {
        let daemon = ServiceDaemon::new()?;
        let rx = daemon.browse(SERVICE)?;
        let (tx, out) = tokio::sync::mpsc::unbounded_channel();
        std::thread::Builder::new()
            .name("qr-mdns".into())
            .spawn(move || {
                while let Ok(ev) = rx.recv() {
                    let d = match ev {
                        ServiceEvent::ServiceResolved(s) if s.is_valid() => {
                            let prop = |k: &str| {
                                s.txt_properties
                                    .get_property_val_str(k)
                                    .unwrap_or("")
                                    .to_string()
                            };
                            let mut ips: Vec<IpAddr> =
                                s.addresses.iter().map(|a| a.to_ip_addr()).collect();
                            ips.sort_by_key(|ip| (ip.is_ipv6(), *ip));
                            Discovery::Up(HostInfo {
                                name: prop("name"),
                                host_id: prop("id"),
                                os: prop("os"),
                                proto: prop("proto"),
                                addrs: ips
                                    .into_iter()
                                    .map(|ip| SocketAddr::new(ip, s.port))
                                    .collect(),
                                web_port: prop("web").parse().unwrap_or(0),
                                instance: s.fullname.clone(),
                            })
                        }
                        ServiceEvent::ServiceRemoved(_, fullname) => {
                            Discovery::Down { instance: fullname }
                        }
                        _ => continue,
                    };
                    if tx.send(d).is_err() {
                        break;
                    }
                }
            })?;
        Ok((Self { daemon }, out))
    }
}

impl Drop for Browser {
    fn drop(&mut self) {
        let _ = self.daemon.stop_browse(SERVICE);
        let _ = self.daemon.shutdown();
    }
}

/// Keeps the current set of discovered hosts by instance name.
#[derive(Default)]
pub struct HostList(pub HashMap<String, HostInfo>);

impl HostList {
    pub fn apply(&mut self, d: Discovery) {
        match d {
            Discovery::Up(h) => {
                self.0.insert(h.instance.clone(), h);
            }
            Discovery::Down { instance } => {
                self.0.remove(&instance);
            }
        }
    }
}
