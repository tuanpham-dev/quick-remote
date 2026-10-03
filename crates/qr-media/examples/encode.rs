//! Encodes a test pattern with each working encoder; checks IDR on demand.
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

fn main() -> anyhow::Result<()> {
    let base = Instant::now();
    let now = move || base.elapsed().as_micros() as u64;
    let encs = qr_media::probe::encoders()?;
    for e in encs {
        let cfg = qr_media::encode::EncodeConfig {
            encoder: e.clone(),
            source: qr_media::encode::Source::Test {
                pattern: "ball".into(),
            },
            width: 1920,
            height: 1080,
            fps: 60,
            bitrate_kbps: 20_000,
            chroma444: false,
        };
        let frames = Arc::new(Mutex::new(Vec::new()));
        let f2 = frames.clone();
        let enc = qr_media::encode::Encoder::start(&cfg, qr_media::ClockMap::new(now), move |f| {
            f2.lock().unwrap().push(f)
        })?;
        std::thread::sleep(Duration::from_millis(1000));
        let n_before = frames.lock().unwrap().len();
        enc.force_keyframe();
        let asked = frames.lock().unwrap().len();
        std::thread::sleep(Duration::from_millis(500));
        if let Some(err) = enc.error() {
            println!("{}: ERROR {err}", e.element);
            continue;
        }
        drop(enc);
        let f = frames.lock().unwrap();
        let keys: Vec<usize> = f
            .iter()
            .enumerate()
            .filter(|(_, x)| x.keyframe)
            .map(|(i, _)| i)
            .collect();
        let lat: Vec<u64> = f.iter().map(|x| x.encode_end_us - x.capture_us).collect();
        let enc_t: Vec<u64> = f
            .iter()
            .map(|x| x.encode_end_us - x.encode_start_us)
            .collect();
        let med = |v: &[u64]| {
            let mut v = v.to_vec();
            v.sort();
            v.get(v.len() / 2).copied().unwrap_or(0)
        };
        let idr_late = keys.iter().find(|&&k| k >= asked).map(|k| k - asked);
        let kbps = f.iter().skip(5).map(|x| x.au.len()).sum::<usize>() as f64 * 8.0 / 1.4 / 1000.0;
        println!(
            "{:12} {}x{} frames={} ({n_before} before IDR req) keys={keys:?} idr_late={idr_late:?} capture→encoded med={}µs encode med={}µs ~{kbps:.0} kbps",
            e.element,
            f[0].width,
            f[0].height,
            f.len(),
            med(&lat),
            med(&enc_t)
        );
    }
    Ok(())
}
