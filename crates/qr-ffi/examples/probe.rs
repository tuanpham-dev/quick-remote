//! Runs the Android session core on Linux against a host: pairs, pulls
//! frames and audio for a few seconds, sends input, and reports.
//!   cargo run -p qr-ffi --example probe -- ADDR PIN
use std::time::{Duration, Instant};

fn main() -> anyhow::Result<()> {
    qr_net::init_crypto();
    let a: Vec<String> = std::env::args().collect();
    let dir = std::env::temp_dir().join(format!("qr-ffi-probe-{}", std::process::id()));
    let h = quickremote::core::connect(
        dir.to_string_lossy().into(),
        a[1].clone(),
        a.get(2).cloned(),
        "android-probe".into(),
        None,
        1,
        (1280, 720),
    )?;
    let s = quickremote::core::handles().lock().unwrap().get(&h).cloned().unwrap();
    let (mut frames, mut keys, mut audio, mut bytes) = (0, 0, 0, 0usize);
    let t0 = Instant::now();
    let mut first_key_at = None;
    while t0.elapsed() < Duration::from_secs(5) {
        if let Some(f) = s.next_video(20) {
            frames += 1;
            bytes += f.au.len();
            if f.keyframe {
                keys += 1;
                first_key_at.get_or_insert(t0.elapsed());
            }
            s.decoded(f.id, 1000);
        }
        while s.next_audio(0).is_some() {
            audio += 1;
        }
        s.input(qr_proto::InputEvent::Key { code: 30, down: true });
        s.input(qr_proto::InputEvent::Key { code: 30, down: false });
        s.motion(1.0, 0.0, None);
    }
    let mut events = Vec::new();
    while let Some(e) = s.poll_event() {
        events.push(e);
    }
    println!(
        "frames={frames} keyframes={keys} first_key={first_key_at:?} kbytes={} audio_packets={audio} events={events:?}",
        bytes / 1024
    );
    s.close();
    Ok(())
}
