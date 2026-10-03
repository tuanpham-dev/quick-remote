//! Optional input-latency trace: `<label> <CLOCK_MONOTONIC µs>` lines, so a
//! client and a host on the same machine can be compared directly.

use std::io::Write;
use std::sync::Mutex;

static FILE: Mutex<Option<std::fs::File>> = Mutex::new(None);

/// Starts tracing to `path` (from `QR_INPUT_TRACE` or a flag).
pub fn open(path: &str) -> std::io::Result<()> {
    *FILE.lock().expect("lock") = Some(std::fs::File::create(path)?);
    Ok(())
}

/// Opens the trace named by the `QR_INPUT_TRACE` environment variable.
pub fn from_env() {
    if let Ok(p) = std::env::var("QR_INPUT_TRACE")
        && let Err(e) = open(&p)
    {
        tracing::warn!("input trace {p}: {e}");
    }
}

/// CLOCK_MONOTONIC in µs: the same clock in every process on the machine.
#[cfg(unix)]
pub fn monotonic_us() -> u64 {
    let mut ts = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    // SAFETY: clock_gettime writes one timespec into `ts`.
    unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) };
    ts.tv_sec as u64 * 1_000_000 + ts.tv_nsec as u64 / 1000
}

#[cfg(not(unix))]
pub fn monotonic_us() -> u64 {
    crate::clock::now_us()
}

pub fn event(label: &str) {
    let mut f = FILE.lock().expect("lock");
    if let Some(f) = f.as_mut() {
        let _ = writeln!(f, "{label} {}", monotonic_us());
    }
}

pub fn enabled() -> bool {
    FILE.lock().expect("lock").is_some()
}
