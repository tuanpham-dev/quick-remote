//! A Wayland clipboard client for tests (wlr-data-control), like wl-copy and
//! wl-paste, on the same code as the host's:
//!   wlclipctl <socket> set-text <text> | set-png <file>   (holds it 8 s)
//!   wlclipctl <socket> get <out-file> <timeout s>          (the next change)
#[path = "../src/wlclip.rs"]
mod wlclip;

use std::time::{Duration, Instant};

use qr_clip::{Command, Content};

fn main() -> anyhow::Result<()> {
    let a: Vec<String> = std::env::args().collect();
    anyhow::ensure!(a.len() >= 4, "usage: wlclipctl <socket> set-text <text> | set-png <file> | get <out> <secs>");
    let (tx, rx) = wlclip::spawn(std::path::Path::new(&a[1]))?;
    match a[2].as_str() {
        "set-text" => tx.send(Command::Set(Content::Text(a[3].clone())))?,
        "set-png" => tx.send(Command::Set(Content::Png(std::fs::read(&a[3])?)))?,
        "get" => {
            let secs: u64 = a.get(4).and_then(|s| s.parse().ok()).unwrap_or(8);
            let start = Instant::now();
            let end = start + Duration::from_secs(secs);
            while let Some(left) = end.checked_duration_since(Instant::now()) {
                let Ok(c) = rx.recv_timeout(left) else { break };
                // The selection already there arrives first: not a change.
                if start.elapsed() < Duration::from_millis(300) {
                    continue;
                }
                match c {
                    Content::Text(t) => std::fs::write(&a[3], t)?,
                    Content::Png(p) => std::fs::write(&a[3], p)?,
                }
                return Ok(());
            }
            anyhow::bail!("no clipboard change");
        }
        other => anyhow::bail!("unknown command {other}"),
    }
    std::thread::sleep(Duration::from_secs(8));
    Ok(())
}
