//! Plays a rumble effect on the first input device whose name contains the
//! argument, like a game would: `rumble "QuickRemote Gamepad"`.
use evdev::{Device, FFEffectData, FFEffectKind, FFReplay, FFTrigger};

fn main() -> anyhow::Result<()> {
    let want = std::env::args().nth(1).unwrap_or("QuickRemote Gamepad".into());
    let (_, mut dev) = evdev::enumerate()
        .find(|(_, d)| d.name().is_some_and(|n| n.contains(&want)))
        .ok_or_else(|| anyhow::anyhow!("no device named {want}"))?;
    let mut fx = dev.upload_ff_effect(FFEffectData {
        direction: 0,
        trigger: FFTrigger::default(),
        replay: FFReplay { length: 300, delay: 0 },
        kind: FFEffectKind::Rumble { strong_magnitude: 40000, weak_magnitude: 20000 },
    })?;
    fx.play(1)?;
    std::thread::sleep(std::time::Duration::from_millis(400));
    println!("played rumble on {}", dev.name().unwrap_or("?"));
    let _ = Device::open("/dev/null");
    Ok(())
}
