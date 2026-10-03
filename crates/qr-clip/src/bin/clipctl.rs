//! Test helper: `qr-clipctl DISPLAY set-text TEXT | set-png FILE | get OUT`.
//! `set-*` keeps serving until killed; `get` waits for the next change
//! (or reads the current owner) and writes it to OUT.
use std::time::Duration;

use qr_clip::{Command, Content};

fn main() -> anyhow::Result<()> {
    let a: Vec<String> = std::env::args().collect();
    anyhow::ensure!(a.len() >= 4, "usage: qr-clipctl DISPLAY set-text TEXT | set-png FILE | get OUT [secs]");
    let (tx, rx) = qr_clip::spawn(&a[1])?;
    match a[2].as_str() {
        "set-text" => {
            tx.send(Command::Set(Content::Text(a[3].clone())))?;
            loop {
                std::thread::sleep(Duration::from_secs(3600));
            }
        }
        "set-png" => {
            tx.send(Command::Set(Content::Png(std::fs::read(&a[3])?)))?;
            loop {
                std::thread::sleep(Duration::from_secs(3600));
            }
        }
        "get" => {
            let secs = a.get(4).and_then(|s| s.parse().ok()).unwrap_or(10);
            let c = rx.recv_timeout(Duration::from_secs(secs))?;
            match c {
                Content::Text(t) => std::fs::write(&a[3], t)?,
                Content::Png(p) => std::fs::write(&a[3], p)?,
            }
            Ok(())
        }
        other => anyhow::bail!("unknown command {other}"),
    }
}
