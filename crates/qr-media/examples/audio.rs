//! Capture a test tone, drop every 20th packet, play into a WAV file.
use qr_media::audio::*;
use std::sync::Arc;
use std::time::{Duration, Instant};
fn main() -> anyhow::Result<()> {
    let base = Instant::now();
    let now = move || base.elapsed().as_micros() as u64;
    let out = std::env::args().nth(1).unwrap_or("/tmp/qr-audio.wav".into());
    let player = Arc::new(AudioPlayer::start(&AudioSink::File { path: out.clone() }, 2)?);
    let p2 = player.clone();
    let n = Arc::new(std::sync::atomic::AtomicU32::new(0));
    let n2 = n.clone();
    let cap = AudioCapture::start(
        &AudioSource::Test { wave: "sine".into() },
        2,
        128_000,
        qr_media::ClockMap::new(now),
        move |pkt, _ts| {
            let seq = n2.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            if seq % 20 != 19 {
                p2.push(seq, &pkt).unwrap();
            }
        },
    )?;
    std::thread::sleep(Duration::from_secs(2));
    drop(cap);
    player.finish();
    println!(
        "packets {} err={:?}/{:?} -> {out} ({} bytes)",
        n.load(std::sync::atomic::Ordering::Relaxed),
        player.error(),
        None::<()>,
        std::fs::metadata(&out)?.len()
    );
    Ok(())
}
