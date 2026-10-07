//! Smoke test of the virtual display: start it, cast it through the
//! portal, encode a few seconds, resize, add an output, report.
#[path = "../src/vdisplay.rs"]
mod vdisplay;
#[path = "../src/portal.rs"]
mod portal;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let gpu = std::env::args().any(|a| a == "--gpu");
    let mut vd = vdisplay::VirtualDisplay::start(1280, 720, 60, Some("foot || weston-terminal || xterm"), gpu)?;
    std::thread::sleep(std::time::Duration::from_secs(1));
    println!("monitors: {:?}", vd.monitors()?);
    let cast = portal::start(Some(&vd.bus_address()), None).await?;
    println!("cast node={} size={:?}", cast.node, cast.size);
    qr_media::init()?;
    let desc = format!("pipewiresrc fd={} path={} num-buffers=60 ! videoconvert ! video/x-raw,format=I420 ! fakesink name=s", cast.raw_fd(), cast.node);
    let p = qr_media::gstreamer::parse::launch(&desc)?.downcast::<qr_media::gstreamer::Pipeline>().unwrap();
    use qr_media::gstreamer::prelude::*;
    p.set_state(qr_media::gstreamer::State::Playing)?;
    let bus = p.bus().unwrap();
    let msg = bus.timed_pop_filtered(qr_media::gstreamer::ClockTime::from_seconds(15), &[qr_media::gstreamer::MessageType::Eos, qr_media::gstreamer::MessageType::Error]);
    println!("pipeline: {:?}", msg.map(|m| m.type_()));
    let caps = p.by_name("s").and_then(|s| s.static_pad("sink")).and_then(|pad| pad.current_caps());
    println!("caps: {caps:?}");
    p.set_state(qr_media::gstreamer::State::Null)?;
    vd.set_mode("HEADLESS-1", 1920, 1080, 60)?;
    let second = vd.add_output(1280, 720, 30)?;
    println!("after resize + add: {:?}", vd.monitors()?.iter().map(|m| (m.name.clone(), m.width, m.height, m.x)).collect::<Vec<_>>());
    println!("second output {second}; exited: {:?}", vd.exited());
    Ok(())
}
