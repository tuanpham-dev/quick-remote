//! Decode delay at a low frame rate, where a decoder holding one picture
//! shows as a whole frame interval (100 ms at 10 fps), and whether every
//! decoder outputs the same pictures (a hash of their visible pixels). One
//! encoded stream feeds them all:
//!   cargo run -p qr-media --example hold -- [encoder] [decoder...]
use qr_media::decode::{Decoder, PixelFormat};
use qr_media::{encode::*, probe};
use std::hash::{DefaultHasher, Hash, Hasher};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

fn main() -> anyhow::Result<()> {
    let base = Instant::now();
    let now = move || base.elapsed().as_micros() as u64;
    let args: Vec<String> = std::env::args().skip(1).collect();
    let enc_name = args.first().map(String::as_str).unwrap_or("x264enc");
    let encoder = probe::encoders()?.into_iter().find(|e| e.element == enc_name).expect("encoder not available");
    let decs: Vec<String> = if args.len() > 1 {
        args[1..].to_vec()
    } else {
        probe::decoders()?.into_iter().filter(|d| d.codec == encoder.codec).map(|d| d.element.to_string()).collect()
    };
    let aus = Arc::new(Mutex::new(Vec::new()));
    let a2 = aus.clone();
    let cfg = EncodeConfig {
        encoder: encoder.clone(),
        source: Source::Test { pattern: "ball".into() },
        width: 640,
        height: 360,
        fps: 10,
        bitrate_kbps: 2_000,
        chroma444: false,
    };
    let enc = Encoder::start(&cfg, qr_media::ClockMap::new(now), move |f| {
        a2.lock().unwrap().push((f.au.to_vec(), f.keyframe))
    })?;
    std::thread::sleep(Duration::from_millis(3000));
    drop(enc);
    let aus = aus.lock().unwrap().clone();
    for d in decs {
        let out = Arc::new(Mutex::new(Vec::new()));
        let o2 = out.clone();
        let dec = Decoder::start(encoder.codec, &d, move |f| {
            // Visible pixels only: strides differ between decoders.
            let mut h = DefaultHasher::new();
            let (w, ht) = (f.width as usize, f.height as usize);
            let planes: &[(usize, usize)] = match f.format {
                PixelFormat::Nv12 => &[(w, ht), (w, ht / 2)],
                _ => &[(w, ht), (w / 2, ht / 2), (w / 2, ht / 2)],
            };
            // NV12 and I420 hash differently: compare like with like.
            (f.format == PixelFormat::Nv12).hash(&mut h);
            for (i, &(pw, ph)) in planes.iter().enumerate() {
                for y in 0..ph {
                    let o = f.offsets[i] + y * f.strides[i];
                    f.data[o..o + pw].hash(&mut h);
                }
            }
            o2.lock().unwrap().push((f.decode_us, h.finish()));
        })?;
        for (i, (au, key)) in aus.iter().enumerate() {
            dec.push(i as u32, au, *key)?;
            std::thread::sleep(Duration::from_millis(100));
        }
        std::thread::sleep(Duration::from_millis(200));
        let frames = out.lock().unwrap().clone();
        let mut dt: Vec<u64> = frames.iter().map(|f| f.0).collect();
        dt.sort();
        let p50 = dt.get(dt.len() / 2).copied().unwrap_or(0) as f64 / 1000.0;
        let mut h = DefaultHasher::new();
        frames.iter().for_each(|f| f.1.hash(&mut h));
        println!(
            "{enc_name} -> {d}: {}/{} frames, p50 {p50:.1} ms, pictures {:016x}",
            frames.len(),
            aus.len(),
            h.finish()
        );
    }
    Ok(())
}
