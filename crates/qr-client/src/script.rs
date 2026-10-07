//! Scripted input for headless tests (`--script FILE`), one command a line:
//!
//! ```text
//! sleep <ms>
//! key <evdev code> down|up      tap <evdev code>
//! text <rest of line>
//! button <n> down|up
//! rel <dx> <dy> [count]          (count steps, 1 ms apart)
//! abs <x> <y> [stream]        (stream pixels)
//! wheel <dx> <dy>
//! release
//! pen <x> <y> <pressure> <tilt_x> <tilt_y> <flags>
//! resize <w> <h> [refresh]     (virtual display mode)
//! monitor <id> [stream]        (another stream opens a second picture)
//! stopstream <stream>
//! ```
//!
//! Every input event sent is written to the input trace (if open).

use std::time::Duration;

use anyhow::{Context, Result, bail};
use qr_proto::InputEvent;

use crate::session::Input;

fn label(ev: &InputEvent) -> String {
    match ev {
        InputEvent::Key { code, down } => format!("key:{code}:{}", if *down { "d" } else { "u" }),
        InputEvent::Button { button, down } => format!("button:{button}:{}", if *down { "d" } else { "u" }),
        InputEvent::Text { .. } => "text".into(),
        InputEvent::Wheel { .. } => "wheel".into(),
        InputEvent::Abs { .. } => "abs".into(),
        InputEvent::ReleaseAll => "release".into(),
        InputEvent::Pen { .. } => "pen".into(),
    }
}

fn send(input: &Input, ev: InputEvent) {
    qr_net::trace::event(&label(&ev));
    input.send(ev);
}

pub async fn run(path: &str, input: &Input, ctl: &qr_net::files::Control) -> Result<()> {
    let text = std::fs::read_to_string(path).with_context(|| format!("reading {path}"))?;
    for (n, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (cmd, rest) = line.split_once(' ').unwrap_or((line, ""));
        let a: Vec<&str> = rest.split_whitespace().collect();
        let num = |i: usize| -> Result<i64> {
            a.get(i).with_context(|| format!("line {}: missing argument", n + 1))?.parse::<i64>().map_err(Into::into)
        };
        let down = |i: usize| a.get(i) == Some(&"down");
        match cmd {
            "resize" => {
                qr_net::trace::event("resize");
                ctl(qr_proto::Msg::Resize {
                    stream: 0,
                    width: num(0)? as u16,
                    height: num(1)? as u16,
                    refresh: num(2).unwrap_or(0) as u16,
                });
            }
            "monitor" => {
                ctl(qr_proto::Msg::SelectMonitor { stream: num(1).unwrap_or(0) as u8, monitor: num(0)? as u8 })
            }
            "stopstream" => ctl(qr_proto::Msg::StreamStop { stream: num(0)? as u8 }),
            "sleep" => tokio::time::sleep(Duration::from_millis(num(0)? as u64)).await,
            "key" => send(input, InputEvent::Key { code: num(0)? as u16, down: down(1) }),
            "tap" => {
                send(input, InputEvent::Key { code: num(0)? as u16, down: true });
                send(input, InputEvent::Key { code: num(0)? as u16, down: false });
            }
            "text" => send(input, InputEvent::Text { text: rest.to_string() }),
            "button" => send(input, InputEvent::Button { button: num(0)? as u8, down: down(1) }),
            "rel" => {
                let count = if a.len() > 2 { num(2)? } else { 1 };
                for _ in 0..count {
                    input.rel(num(0)? as f64, num(1)? as f64);
                    if count > 1 {
                        tokio::time::sleep(Duration::from_millis(1)).await;
                    }
                }
            }
            "abs" => input.abs(num(2).unwrap_or(0) as u8, num(0)? as u16, num(1)? as u16),
            "wheel" => send(input, InputEvent::Wheel { dx: num(0)? as i16, dy: num(1)? as i16 }),
            "release" => send(input, InputEvent::ReleaseAll),
            // pen <x> <y> <pressure 0-65535> <tilt_x> <tilt_y> <flags>, in stream pixels.
            "pen" => send(
                input,
                InputEvent::Pen {
                    stream: 0,
                    x: num(0)? as u32 * 16,
                    y: num(1)? as u32 * 16,
                    pressure: num(2)? as u16,
                    tilt_x: num(3)? as i8,
                    tilt_y: num(4)? as i8,
                    flags: num(5)? as u8,
                },
            ),
            other => bail!("line {}: unknown command {other}", n + 1),
        }
    }
    Ok(())
}
