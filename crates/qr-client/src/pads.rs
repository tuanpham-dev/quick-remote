//! Client gamepads (gilrs): up to 4 pads become `PadState` datagrams (full
//! snapshots on change and every 100 ms) with attach/detach on the control
//! stream; host rumble plays back through force feedback.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use bytes::Bytes;
use gilrs::{Axis, Button, Event, EventType, GamepadId, Gilrs};
use qr_proto::Msg;
use qr_proto::datagram::{PadState, pad};

struct Slot {
    id: GamepadId,
    seq: u32,
    last: (u32, [i16; 6]),
    sent_at: Instant,
}

const BUTTONS: [(Button, u32); 15] = [
    (Button::South, pad::A),
    (Button::East, pad::B),
    (Button::West, pad::X),
    (Button::North, pad::Y),
    (Button::LeftTrigger, pad::LB),
    (Button::RightTrigger, pad::RB),
    (Button::Select, pad::BACK),
    (Button::Start, pad::START),
    (Button::Mode, pad::GUIDE),
    (Button::LeftThumb, pad::LS),
    (Button::RightThumb, pad::RS),
    (Button::DPadUp, pad::UP),
    (Button::DPadDown, pad::DOWN),
    (Button::DPadLeft, pad::LEFT),
    (Button::DPadRight, pad::RIGHT),
];

fn snapshot(g: &gilrs::Gamepad) -> (u32, [i16; 6]) {
    let mut b = 0;
    for (btn, bit) in BUTTONS {
        if g.is_pressed(btn) {
            b |= bit;
        }
    }
    let ax = |a: Axis, flip: bool| {
        let v = g.value(a);
        ((if flip { -v } else { v }) * 32767.0).round().clamp(-32768.0, 32767.0) as i16
    };
    let trig = |btn: Button| (g.button_data(btn).map_or(0.0, |d| d.value()) * 32767.0) as i16;
    // gilrs' +Y is up; the protocol's is down.
    (
        b,
        [
            ax(Axis::LeftStickX, false),
            ax(Axis::LeftStickY, true),
            ax(Axis::RightStickX, false),
            ax(Axis::RightStickY, true),
            trig(Button::LeftTrigger2),
            trig(Button::RightTrigger2),
        ],
    )
}

/// Runs on its own thread until the session's control channel closes.
pub fn run(
    conn: quinn::Connection,
    ctl: qr_net::files::Control,
    rumble: std::sync::mpsc::Receiver<(u8, u16, u16, u16)>,
) {
    let mut gilrs = match Gilrs::new() {
        Ok(g) => g,
        Err(e) => {
            tracing::warn!("gamepads are off: {e}");
            return;
        }
    };
    let mut slots: HashMap<u8, Slot> = HashMap::new();
    let send = |idx: u8, slot: &mut Slot, state: (u32, [i16; 6])| {
        slot.seq = slot.seq.wrapping_add(1);
        slot.last = state;
        slot.sent_at = Instant::now();
        let p = PadState { idx, seq: slot.seq, buttons: state.0, axes: state.1 };
        conn.send_datagram(Bytes::from(p.encode())).is_ok()
    };
    loop {
        while let Some(Event { id, event, .. }) = gilrs.next_event_blocking(Some(Duration::from_millis(10))) {
            match event {
                EventType::Connected => {
                    if let Some(idx) = (0..4u8).find(|i| !slots.contains_key(i)) {
                        slots.insert(idx, Slot { id, seq: 0, last: (0, [0; 6]), sent_at: Instant::now() });
                        ctl(Msg::PadAttach { idx, kind: 1 });
                        tracing::info!(idx, name = gilrs.gamepad(id).name(), "gamepad attached");
                    }
                }
                EventType::Disconnected => {
                    if let Some((&idx, _)) = slots.iter().find(|(_, s)| s.id == id) {
                        slots.remove(&idx);
                        ctl(Msg::PadDetach { idx });
                    }
                }
                _ => {}
            }
        }
        for (&idx, slot) in slots.iter_mut() {
            let state = snapshot(&gilrs.gamepad(slot.id));
            if (state != slot.last || slot.sent_at.elapsed() >= Duration::from_millis(100)) && !send(idx, slot, state) {
                return;
            }
        }
        while let Ok((idx, strong, weak, ms)) = rumble.try_recv() {
            let Some(slot) = slots.get(&idx) else { continue };
            play_rumble(&mut gilrs, slot.id, strong, weak, ms);
        }
    }
}

fn play_rumble(gilrs: &mut Gilrs, id: GamepadId, strong: u16, weak: u16, ms: u16) {
    use gilrs::ff::{BaseEffect, BaseEffectType, EffectBuilder, Replay, Ticks};
    let dur = Ticks::from_ms(ms as u32);
    let effect = EffectBuilder::new()
        .add_effect(BaseEffect {
            kind: BaseEffectType::Strong { magnitude: strong },
            scheduling: Replay { play_for: dur, ..Default::default() },
            ..Default::default()
        })
        .add_effect(BaseEffect {
            kind: BaseEffectType::Weak { magnitude: weak },
            scheduling: Replay { play_for: dur, ..Default::default() },
            ..Default::default()
        })
        .gamepads(&[id])
        .finish(gilrs);
    match effect {
        Ok(e) => {
            let _ = e.play();
            // The effect stops when dropped: keep it for its duration.
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(ms as u64 + 50));
                drop(e);
            });
        }
        Err(e) => tracing::debug!("rumble unsupported: {e}"),
    }
}

/// `--fake-pad`: pads 0 and 1 attach; pad 0 (after 400 ms neutral, so a
/// recorder can attach) presses A and sweeps its left
/// stick, pad 1 holds B; then pad 1 is unplugged (tests).
pub fn fake(conn: quinn::Connection, ctl: qr_net::files::Control) {
    ctl(Msg::PadAttach { idx: 0, kind: 1 });
    ctl(Msg::PadAttach { idx: 1, kind: 1 });
    for seq in 1..=100u32 {
        let x = ((seq as f32 / 100.0) * 65535.0 - 32768.0) as i16;
        let p0 = PadState {
            idx: 0,
            seq,
            buttons: if (20..60).contains(&seq) { pad::A } else { 0 },
            axes: [x, 0, 0, 0, 0, 32767],
        };
        let p1 = PadState { idx: 1, seq, buttons: pad::B, axes: [0; 6] };
        let _ = conn.send_datagram(Bytes::from(p0.encode()));
        let _ = conn.send_datagram(Bytes::from(p1.encode()));
        std::thread::sleep(Duration::from_millis(20));
    }
    std::thread::sleep(Duration::from_millis(1500));
    ctl(Msg::PadDetach { idx: 1 });
}
