//! Encode a test pattern and decode it with every working decoder of the
//! same codec; print timings.
use qr_media::{decode::Decoder, encode::*, probe};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

fn main() -> anyhow::Result<()> {
    let base = Instant::now();
    let now = move || base.elapsed().as_micros() as u64;
    let encs = probe::encoders()?;
    let decs = probe::decoders()?;
    for e in encs.iter().filter(|e| e.element != "svtav1enc") {
        for d in decs.iter().filter(|d| d.codec == e.codec) {
            let out = Arc::new(Mutex::new(Vec::new()));
            let o2 = out.clone();
            let dec = Arc::new(Decoder::start(e.codec, d.element, move |f| {
                o2.lock().unwrap().push(f)
            })?);
            let dec2 = dec.clone();
            let n = Arc::new(Mutex::new(0u32));
            let n2 = n.clone();
            let cfg = EncodeConfig {
                encoder: e.clone(),
                source: Source::Test {
                    pattern: "smpte".into(),
                },
                width: 1920,
                height: 1080,
                fps: 60,
                bitrate_kbps: 20_000,
                chroma444: false,
            };
            let enc = Encoder::start(&cfg, qr_media::ClockMap::new(now), move |f| {
                let mut n = n2.lock().unwrap();
                dec2.push(*n, &f.au, f.keyframe).unwrap();
                *n += 1;
            })?;
            std::thread::sleep(Duration::from_millis(1500));
            drop(enc);
            std::thread::sleep(Duration::from_millis(100));
            let f = out.lock().unwrap();
            let mut dt: Vec<u64> = f.iter().map(|x| x.decode_us).collect();
            dt.sort();
            println!(
                "{:12} -> {:12} decoded {}/{} {}x{} {:?} decode med={}µs p95={}µs err={:?}",
                e.element,
                d.element,
                f.len(),
                *n.lock().unwrap(),
                f.first().map_or(0, |x| x.width),
                f.first().map_or(0, |x| x.height),
                f.first().map(|x| x.format),
                dt.get(dt.len() / 2).unwrap_or(&0),
                dt.get(dt.len() * 95 / 100).unwrap_or(&0),
                dec.error()
            );
        }
    }
    Ok(())
}
