//! File transfer, both directions, over the session's QUIC connection.
//!
//! The sender offers (`FileOffer` with size and SHA-256) on the control
//! stream; the receiver answers `FileAccept`; the sender then opens a
//! unidirectional stream `[0x02][u32 id][bytes…]` at low priority, so video
//! and input are never queued behind it. The receiver writes `name.part`,
//! checks the hash, renames, and reports `FileDone`. `FileCancel` from either
//! side stops the stream and removes the partial file.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result, bail};
use qr_proto::Msg;
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// Chunk size for reading and writing.
const CHUNK: usize = 256 * 1024;

/// Stream priority below the default (0) of control and input streams.
pub const FILE_PRIORITY: i32 = -10;

pub async fn sha256_file(path: &Path) -> Result<[u8; 32]> {
    let mut f = tokio::fs::File::open(path).await.with_context(|| format!("opening {}", path.display()))?;
    let mut h = Sha256::new();
    let mut buf = vec![0u8; CHUNK];
    loop {
        let n = f.read(&mut buf).await?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(h.finalize().into())
}

/// A file name safe to create in the receive directory.
pub fn safe_name(name: &str) -> String {
    let base = name.rsplit(['/', '\\']).next().unwrap_or("");
    let cleaned: String =
        base.chars().map(|c| if c.is_control() || c == ':' { '_' } else { c }).collect::<String>().trim().to_string();
    if cleaned.is_empty() || cleaned == "." || cleaned == ".." { "file".into() } else { cleaned }
}

/// A name in `dir` that doesn't exist yet: `a.txt`, `a (1).txt`, …
fn unique(dir: &Path, name: &str) -> PathBuf {
    let p = dir.join(name);
    if !p.exists() {
        return p;
    }
    let (stem, ext) = match name.rsplit_once('.') {
        Some((s, e)) if !s.is_empty() => (s.to_string(), format!(".{e}")),
        _ => (name.to_string(), String::new()),
    };
    (1..).map(|i| dir.join(format!("{stem} ({i}){ext}"))).find(|p| !p.exists()).expect("some name is free")
}

#[derive(Debug, Clone, PartialEq)]
pub enum Progress {
    Sending { id: u32, name: String, done: u64, size: u64 },
    Receiving { id: u32, name: String, done: u64, size: u64 },
    Sent { id: u32, name: String },
    Received { id: u32, path: PathBuf },
    Failed { id: u32, name: String, reason: String },
}

struct Offer {
    name: String,
    size: u64,
    sha256: [u8; 32],
}

struct Outgoing {
    path: PathBuf,
    name: String,
    size: u64,
    cancel: Option<tokio::sync::oneshot::Sender<()>>,
}

/// One side's file state for a session.
pub struct Files {
    conn: quinn::Connection,
    /// Where accepted files are written.
    pub dir: PathBuf,
    next_id: Mutex<u32>,
    incoming: Mutex<HashMap<u32, Offer>>,
    outgoing: Mutex<HashMap<u32, Outgoing>>,
    cancelled_in: Mutex<std::collections::HashSet<u32>>,
    /// Progress for the UI (and tests).
    pub events: tokio::sync::broadcast::Sender<Progress>,
}

/// Sends control messages for the file layer.
pub type Control = Arc<dyn Fn(Msg) + Send + Sync>;

impl Files {
    /// `first_id` keeps the two sides' ids apart (client odd, host even).
    pub fn new(conn: quinn::Connection, dir: PathBuf, first_id: u32) -> Arc<Self> {
        let (events, _) = tokio::sync::broadcast::channel(256);
        Arc::new(Self {
            conn,
            dir,
            next_id: Mutex::new(first_id),
            incoming: Mutex::default(),
            outgoing: Mutex::default(),
            cancelled_in: Mutex::default(),
            events,
        })
    }

    /// Offers a file to the peer; the transfer starts when it accepts.
    pub async fn offer(&self, path: &Path, control: &Control) -> Result<u32> {
        let meta = tokio::fs::metadata(path).await.with_context(|| format!("reading {}", path.display()))?;
        if !meta.is_file() {
            bail!("{} is not a file", path.display());
        }
        let sha256 = sha256_file(path).await?;
        let name = safe_name(&path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default());
        let id = {
            let mut n = self.next_id.lock().expect("lock");
            let id = *n;
            *n += 2;
            id
        };
        self.outgoing.lock().expect("lock").insert(
            id,
            Outgoing { path: path.to_path_buf(), name: name.clone(), size: meta.len(), cancel: None },
        );
        control(Msg::FileOffer { id, name, size: meta.len(), sha256 });
        Ok(id)
    }

    /// Handles a file-related control message; returns false for others.
    pub fn on_control(self: &Arc<Self>, m: &Msg, control: &Control) -> bool {
        match m {
            Msg::FileOffer { id, name, size, sha256 } => {
                self.incoming
                    .lock()
                    .expect("lock")
                    .insert(*id, Offer { name: safe_name(name), size: *size, sha256: *sha256 });
                control(Msg::FileAccept { id: *id });
            }
            Msg::FileAccept { id } => {
                let this = self.clone();
                let (id, control) = (*id, control.clone());
                tokio::spawn(async move {
                    if let Err(e) = this.send(id).await {
                        let name = this.outgoing.lock().expect("lock").remove(&id).map(|o| o.name).unwrap_or_default();
                        let _ = this.events.send(Progress::Failed { id, name, reason: format!("{e:#}") });
                        control(Msg::FileCancel { id, reason: format!("{e:#}") });
                    }
                });
            }
            Msg::FileCancel { id, reason } => {
                // Either an upload we're sending (stop it) or a download we're
                // receiving (the stream handler cleans up).
                if let Some(o) = self.outgoing.lock().expect("lock").get_mut(id)
                    && let Some(c) = o.cancel.take()
                {
                    let _ = c.send(());
                }
                self.cancelled_in.lock().expect("lock").insert(*id);
                tracing::info!(id, %reason, "file transfer cancelled by the peer");
            }
            Msg::FileDone { id, ok } => {
                if let Some(o) = self.outgoing.lock().expect("lock").remove(id) {
                    let _ = self.events.send(if *ok {
                        Progress::Sent { id: *id, name: o.name }
                    } else {
                        Progress::Failed { id: *id, name: o.name, reason: "the receiver's copy didn't match".into() }
                    });
                }
            }
            _ => return false,
        }
        true
    }

    /// Cancels a transfer this side started or is receiving.
    pub fn cancel(&self, id: u32, control: &Control) {
        if let Some(o) = self.outgoing.lock().expect("lock").get_mut(&id)
            && let Some(c) = o.cancel.take()
        {
            let _ = c.send(());
        }
        self.cancelled_in.lock().expect("lock").insert(id);
        control(Msg::FileCancel { id, reason: "cancelled".into() });
    }

    async fn send(&self, id: u32) -> Result<()> {
        let (path, name, size, mut cancelled) = {
            let mut out = self.outgoing.lock().expect("lock");
            let o = out.get_mut(&id).context("unknown transfer")?;
            let (tx, rx) = tokio::sync::oneshot::channel();
            o.cancel = Some(tx);
            (o.path.clone(), o.name.clone(), o.size, rx)
        };
        let mut f = tokio::fs::File::open(&path).await?;
        let mut st = self.conn.open_uni().await?;
        st.set_priority(FILE_PRIORITY)?;
        let mut head = vec![qr_proto::uni::FILE];
        head.extend_from_slice(&id.to_le_bytes());
        st.write_all(&head).await?;
        let mut buf = vec![0u8; CHUNK];
        let mut done = 0u64;
        let mut pace = Pacer::new(&self.conn);
        loop {
            pace.wait(&self.conn).await;
            let n = tokio::select! {
                n = f.read(&mut buf) => n?,
                _ = &mut cancelled => {
                    let _ = st.reset(1u32.into());
                    bail!("cancelled");
                }
            };
            if n == 0 {
                break;
            }
            tokio::select! {
                r = st.write_all(&buf[..n]) => r?,
                _ = &mut cancelled => {
                    let _ = st.reset(1u32.into());
                    bail!("cancelled");
                }
            }
            done += n as u64;
            pace.sent(n);
            let _ = self.events.send(Progress::Sending { id, name: name.clone(), done, size });
        }
        st.finish()?;
        Ok(())
    }

    /// Receives one file stream (after its type byte); reports `FileDone`.
    pub async fn receive(&self, mut st: quinn::RecvStream, control: &Control) -> Result<()> {
        let id = st.read_u32_le().await?;
        let Some(offer) = self.incoming.lock().expect("lock").remove(&id) else {
            bail!("file stream {id} was never offered");
        };
        tokio::fs::create_dir_all(&self.dir).await?;
        let dest = unique(&self.dir, &offer.name);
        let part = dest.with_file_name(format!("{}.part", dest.file_name().unwrap_or_default().to_string_lossy()));
        let r = self.receive_into(&mut st, id, &offer, &part).await;
        match r {
            Ok(true) => {
                tokio::fs::rename(&part, &dest).await?;
                control(Msg::FileDone { id, ok: true });
                let _ = self.events.send(Progress::Received { id, path: dest });
                Ok(())
            }
            Ok(false) => {
                let _ = tokio::fs::remove_file(&part).await;
                control(Msg::FileDone { id, ok: false });
                let _ = self.events.send(Progress::Failed {
                    id,
                    name: offer.name,
                    reason: "the received file didn't match its checksum".into(),
                });
                Ok(())
            }
            Err(e) => {
                let _ = tokio::fs::remove_file(&part).await;
                let _ = self.events.send(Progress::Failed { id, name: offer.name, reason: format!("{e:#}") });
                Err(e)
            }
        }
    }

    /// Writes the stream to `part`; returns whether the hash matched.
    async fn receive_into(&self, st: &mut quinn::RecvStream, id: u32, offer: &Offer, part: &Path) -> Result<bool> {
        let mut f = tokio::fs::File::create(part).await?;
        let mut h = Sha256::new();
        let mut buf = vec![0u8; CHUNK];
        let mut done = 0u64;
        loop {
            if self.cancelled_in.lock().expect("lock").remove(&id) {
                bail!("cancelled");
            }
            let n = match st.read(&mut buf).await? {
                Some(n) => n,
                None => break,
            };
            h.update(&buf[..n]);
            f.write_all(&buf[..n]).await?;
            done += n as u64;
            if done > offer.size {
                bail!("more data than offered");
            }
            let _ = self.events.send(Progress::Receiving { id, name: offer.name.clone(), done, size: offer.size });
        }
        f.flush().await?;
        Ok(done == offer.size && <[u8; 32]>::from(h.finalize()) == offer.sha256)
    }
}

/// Paces file data so it never builds a queue in front of video: the
/// rate grows 25% every 100 ms while the round trip stays within 2 ms of
/// the lowest seen, and drops by a third when queueing pushes it 4 ms
/// higher. Video datagrams share the path, and QUIC stream priority does
/// not order stream data against datagrams.
struct Pacer {
    rate_bps: f64,
    min_rtt: std::time::Duration,
    budget: f64,
    last: std::time::Instant,
    last_adjust: std::time::Instant,
}

impl Pacer {
    const START_BPS: f64 = 20e6;
    const MIN_BPS: f64 = 1e6;

    fn new(conn: &quinn::Connection) -> Self {
        let now = std::time::Instant::now();
        Self { rate_bps: Self::START_BPS, min_rtt: conn.rtt(), budget: 0.0, last: now, last_adjust: now }
    }

    fn sent(&mut self, bytes: usize) {
        self.budget -= bytes as f64 * 8.0;
    }

    async fn wait(&mut self, conn: &quinn::Connection) {
        let now = std::time::Instant::now();
        let rtt = conn.rtt();
        self.min_rtt = self.min_rtt.min(rtt);
        if now.duration_since(self.last_adjust) >= std::time::Duration::from_millis(100) {
            self.last_adjust = now;
            let queued = rtt.saturating_sub(self.min_rtt);
            if queued > std::time::Duration::from_millis(4) {
                self.rate_bps = (self.rate_bps * 0.67).max(Self::MIN_BPS);
            } else if queued < std::time::Duration::from_millis(2) {
                self.rate_bps *= 1.25;
            }
        }
        self.budget = (self.budget + now.duration_since(self.last).as_secs_f64() * self.rate_bps)
            .min(self.rate_bps * 0.01);
        self.last = now;
        if self.budget < 0.0 {
            let wait = std::time::Duration::from_secs_f64(-self.budget / self.rate_bps);
            tokio::time::sleep(wait).await;
            self.budget = 0.0;
            self.last = std::time::Instant::now();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_sanitized() {
        assert_eq!(safe_name("../../etc/passwd"), "passwd");
        assert_eq!(safe_name("C:\\Users\\a\\b.txt"), "b.txt");
        assert_eq!(safe_name(".."), "file");
        assert_eq!(safe_name("a\u{7}b"), "a_b");
    }

    #[test]
    fn unique_names() {
        let d = std::env::temp_dir().join(format!("qr-files-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("a.txt"), b"x").unwrap();
        assert_eq!(unique(&d, "a.txt").file_name().unwrap(), "a (1).txt");
        assert_eq!(unique(&d, "b").file_name().unwrap(), "b");
        std::fs::remove_dir_all(d).unwrap();
    }
}
