fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(std::env::var("RUST_LOG").unwrap_or("info".into()))
        .init();
    for e in qr_media::probe::encoders()? {
        println!(
            "enc {:14} {:5} hw={} 444={}",
            e.element,
            e.codec.name(),
            e.hw,
            e.chroma444
        );
    }
    for d in qr_media::probe::decoders()? {
        println!("dec {:14} {:5} hw={}", d.element, d.codec.name(), d.hw);
    }
    Ok(())
}
