//! The outbox: files the host user puts in a folder go to the connected
//! client and are removed once it confirms them.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use qr_net::files::{Control, Files, Progress};

pub async fn run(dir: PathBuf, files: Arc<Files>, ctl: Control) {
    if let Err(e) = tokio::fs::create_dir_all(&dir).await {
        tracing::warn!("outbox {}: {e}", dir.display());
        return;
    }
    let mut events = files.events.subscribe();
    // Offered and not yet confirmed: id → path.
    let mut pending: HashMap<u32, PathBuf> = HashMap::new();
    let mut tick = tokio::time::interval(Duration::from_secs(1));
    loop {
        tokio::select! {
            _ = tick.tick() => {
                let Ok(mut rd) = tokio::fs::read_dir(&dir).await else { continue };
                while let Ok(Some(e)) = rd.next_entry().await {
                    let p = e.path();
                    let is_file = e.file_type().await.is_ok_and(|t| t.is_file());
                    let partial = p.extension().is_some_and(|x| x == "part");
                    if !is_file || partial || pending.values().any(|q| *q == p) {
                        continue;
                    }
                    match files.offer(&p, &ctl).await {
                        Ok(id) => {
                            tracing::info!(file = %p.display(), "outbox: sending");
                            pending.insert(id, p);
                        }
                        Err(e) => tracing::warn!("outbox: {e:#}"),
                    }
                }
            }
            ev = events.recv() => match ev {
                Ok(Progress::Sent { id, .. }) => {
                    if let Some(p) = pending.remove(&id) {
                        let _ = tokio::fs::remove_file(&p).await;
                    }
                }
                // A failed transfer is retried on the next scan.
                Ok(Progress::Failed { id, .. }) => {
                    pending.remove(&id);
                }
                Ok(_) => {}
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                Err(_) => return,
            }
        }
    }
}
