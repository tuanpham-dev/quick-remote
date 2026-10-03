//! Paired peers, by certificate fingerprint, in a JSON file.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::identity::{Fingerprint, hex};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Peer {
    /// Hex SHA-256 of the peer's certificate.
    pub fingerprint: String,
    pub name: String,
    /// Host ID (for hosts; empty for clients).
    #[serde(default)]
    pub host_id: String,
    /// Unix seconds when paired.
    pub added: u64,
    /// Last address the peer was reached at (`ip:port`).
    #[serde(default)]
    pub last_addr: String,
    /// Rendezvous server the host registers with (`host:port`).
    #[serde(default)]
    pub rendezvous: String,
    /// Clipboard sync with this peer (client-side setting).
    #[serde(default = "yes")]
    pub clipboard: bool,
}

fn yes() -> bool {
    true
}

pub struct KeyStore {
    path: PathBuf,
    pub peers: Vec<Peer>,
}

impl KeyStore {
    pub fn open(path: &Path) -> Result<Self> {
        let peers = match std::fs::read(path) {
            Ok(b) => {
                serde_json::from_slice(&b).with_context(|| format!("parsing {}", path.display()))?
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
        };
        Ok(Self {
            path: path.to_path_buf(),
            peers,
        })
    }

    pub fn in_memory() -> Self {
        Self {
            path: PathBuf::new(),
            peers: Vec::new(),
        }
    }

    pub fn get(&self, fp: &Fingerprint) -> Option<&Peer> {
        let h = hex(fp);
        self.peers.iter().find(|p| p.fingerprint == h)
    }

    pub fn get_mut(&mut self, fp: &Fingerprint) -> Option<&mut Peer> {
        let h = hex(fp);
        self.peers.iter_mut().find(|p| p.fingerprint == h)
    }

    pub fn by_host_id(&self, id: &str) -> Option<&Peer> {
        self.peers.iter().find(|p| p.host_id == id)
    }

    /// Adds or replaces the entry for `peer.fingerprint` and saves.
    pub fn upsert(&mut self, peer: Peer) -> Result<()> {
        self.peers.retain(|p| p.fingerprint != peer.fingerprint);
        self.peers.push(peer);
        self.save()
    }

    pub fn save(&self) -> Result<()> {
        if self.path.as_os_str().is_empty() {
            return Ok(());
        }
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let tmp = self.path.with_extension("tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(&self.peers)?)?;
        std::fs::rename(&tmp, &self.path).with_context(|| format!("saving {}", self.path.display()))
    }
}

pub fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}
