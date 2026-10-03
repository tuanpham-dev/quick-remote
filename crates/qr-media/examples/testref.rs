//! Renders one frame of a test source as raw I420: `testref text 1280 720 out.i420`.
use gstreamer as gst;
use gstreamer::prelude::*;

fn main() -> anyhow::Result<()> {
    let a: Vec<String> = std::env::args().collect();
    qr_media::init()?;
    let (w, h): (u32, u32) = (a[2].parse()?, a[3].parse()?);
    let src = qr_media::encode::source_desc(&qr_media::encode::Source::Test { pattern: a[1].clone() }, w, h, 30);
    let desc = format!(
        "{src} ! videoconvert ! video/x-raw,format=I420 ! identity eos-after=3 ! multifilesink location={} max-files=1",
        a[4]
    );
    let p = gst::parse::launch(&desc)?;
    p.set_state(gst::State::Playing)?;
    let bus = p.bus().unwrap();
    bus.timed_pop_filtered(gst::ClockTime::from_seconds(10), &[gst::MessageType::Eos, gst::MessageType::Error]);
    p.set_state(gst::State::Null)?;
    Ok(())
}
