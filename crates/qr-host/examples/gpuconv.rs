//! Smoke test for the GPU capture converter: allocate, convert once.
//!   cargo run -p qr-host --example gpuconv [WxH]   (needs the render group)
#[path = "../src/gpuconv.rs"]
#[allow(dead_code)]
mod gpuconv;

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();
    qr_media::init()?;
    let (w, h) = std::env::args()
        .nth(1)
        .and_then(|s| s.split_once('x').and_then(|(a, b)| Some((a.parse().ok()?, b.parse().ok()?))))
        .unwrap_or((1920, 1080));
    const XRGB: u32 = u32::from_le_bytes(*b"XR24");
    eprintln!("creating converter {w}x{h}");
    let mut c = gpuconv::GpuConv::new("/dev/dri/renderD128", w, h, XRGB, 4)?;
    eprintln!("capture bo modifier {:#x}, {} planes", c.capture.modifier, c.capture.planes.len());
    c.bind()?;
    let t = std::time::Instant::now();
    for _ in 0..100 {
        let f = c.convert(false).expect("free output");
        let _ = f;
    }
    eprintln!("100 conversions: {:.2} ms each", t.elapsed().as_secs_f64() * 10.0);
    Ok(())
}
