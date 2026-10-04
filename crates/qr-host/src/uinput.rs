//! uinput injection: virtual keyboard, mouse (relative and absolute), pen
//! tablet and Xbox 360-style gamepads with rumble. libinput-based
//! compositors (sway, mutter, KWin) pick the devices up; games read the
//! pads directly. Needs write access to /dev/uinput (scripts/setup-root.sh).

use std::collections::HashMap;
use std::os::fd::AsRawFd;
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result};
use evdev::uinput::VirtualDevice;
use evdev::{
    AbsInfo, AbsoluteAxisCode, AttributeSet, BusType, EventSummary, EventType, FFEffectCode, FFEffectKind, InputEvent,
    InputId, KeyCode, PropType, RelativeAxisCode, UInputCode, UinputAbsSetup,
};

use crate::input::Injector;

const SYN: u16 = 0; // EV_SYN / SYN_REPORT

fn ev(t: EventType, code: u16, value: i32) -> InputEvent {
    InputEvent::new(t.0, code, value)
}

fn syn() -> InputEvent {
    InputEvent::new(EventType::SYNCHRONIZATION.0, SYN, 0)
}

pub struct Uinput {
    kbd: VirtualDevice,
    mouse: VirtualDevice,
    pen: VirtualDevice,
    /// Screen size for absolute coordinates.
    size: (i32, i32),
    pads: HashMap<u8, mpsc::Sender<(u32, [i16; 6])>>,
    rumble: Arc<Mutex<Vec<(u8, u16, u16)>>>,
    pen_tool: Option<KeyCode>,
    pen_contact: bool,
}

impl Uinput {
    pub fn open(width: i32, height: i32) -> Result<Self> {
        let mut keys = AttributeSet::<KeyCode>::new();
        for c in 1..=248u16 {
            keys.insert(KeyCode::new(c));
        }
        let kbd = VirtualDevice::builder()
            .context("opening /dev/uinput (needs the input group; run scripts/setup-root.sh)")?
            .name("QuickRemote Keyboard")
            .input_id(InputId::new(BusType::BUS_VIRTUAL, 0x5152, 1, 1))
            .with_keys(&keys)?
            .build()?;

        let mut buttons = AttributeSet::<KeyCode>::new();
        for b in [KeyCode::BTN_LEFT, KeyCode::BTN_RIGHT, KeyCode::BTN_MIDDLE, KeyCode::BTN_SIDE, KeyCode::BTN_EXTRA] {
            buttons.insert(b);
        }
        let mut rel = AttributeSet::<RelativeAxisCode>::new();
        for r in [
            RelativeAxisCode::REL_X,
            RelativeAxisCode::REL_Y,
            RelativeAxisCode::REL_WHEEL,
            RelativeAxisCode::REL_HWHEEL,
            RelativeAxisCode::REL_WHEEL_HI_RES,
            RelativeAxisCode::REL_HWHEEL_HI_RES,
        ] {
            rel.insert(r);
        }
        let abs = |code, max| UinputAbsSetup::new(code, AbsInfo::new(0, 0, max, 0, 0, 1));
        let mouse = VirtualDevice::builder()?
            .name("QuickRemote Mouse")
            .input_id(InputId::new(BusType::BUS_VIRTUAL, 0x5152, 2, 1))
            .with_keys(&buttons)?
            .with_relative_axes(&rel)?
            .with_absolute_axis(&abs(AbsoluteAxisCode::ABS_X, width - 1))?
            .with_absolute_axis(&abs(AbsoluteAxisCode::ABS_Y, height - 1))?
            .build()?;

        let mut pen_keys = AttributeSet::<KeyCode>::new();
        for k in [KeyCode::BTN_TOOL_PEN, KeyCode::BTN_TOOL_RUBBER, KeyCode::BTN_TOUCH, KeyCode::BTN_STYLUS] {
            pen_keys.insert(k);
        }
        let mut props = AttributeSet::<PropType>::new();
        props.insert(PropType::DIRECT);
        // Sub-pixel position: the protocol sends pixels × 16.
        let pen = VirtualDevice::builder()?
            .name("QuickRemote Pen")
            .input_id(InputId::new(BusType::BUS_VIRTUAL, 0x5152, 3, 1))
            .with_keys(&pen_keys)?
            .with_properties(&props)?
            .with_absolute_axis(&UinputAbsSetup::new(
                AbsoluteAxisCode::ABS_X,
                AbsInfo::new(0, 0, width * 16 - 1, 0, 0, 16),
            ))?
            .with_absolute_axis(&UinputAbsSetup::new(
                AbsoluteAxisCode::ABS_Y,
                AbsInfo::new(0, 0, height * 16 - 1, 0, 0, 16),
            ))?
            .with_absolute_axis(&UinputAbsSetup::new(
                AbsoluteAxisCode::ABS_PRESSURE,
                AbsInfo::new(0, 0, 4095, 0, 0, 0),
            ))?
            .with_absolute_axis(&UinputAbsSetup::new(AbsoluteAxisCode::ABS_TILT_X, AbsInfo::new(0, -64, 63, 0, 0, 0)))?
            .with_absolute_axis(&UinputAbsSetup::new(AbsoluteAxisCode::ABS_TILT_Y, AbsInfo::new(0, -64, 63, 0, 0, 0)))?
            .build()?;
        Ok(Self {
            kbd,
            mouse,
            pen,
            size: (width, height),
            pads: HashMap::new(),
            rumble: Arc::default(),
            pen_tool: None,
            pen_contact: false,
        })
    }
}

/// One virtual Xbox 360 pad on its own thread: state in through a channel,
/// rumble effects out through `rumble`.
fn spawn_pad(idx: u8, rumble: Arc<Mutex<Vec<(u8, u16, u16)>>>) -> Result<mpsc::Sender<(u32, [i16; 6])>> {
    use qr_proto::datagram::pad as p;
    let mut keys = AttributeSet::<KeyCode>::new();
    let map: [(u32, KeyCode); 11] = [
        (p::A, KeyCode::BTN_SOUTH),
        (p::B, KeyCode::BTN_EAST),
        (p::X, KeyCode::BTN_WEST),
        (p::Y, KeyCode::BTN_NORTH),
        (p::LB, KeyCode::BTN_TL),
        (p::RB, KeyCode::BTN_TR),
        (p::BACK, KeyCode::BTN_SELECT),
        (p::START, KeyCode::BTN_START),
        (p::GUIDE, KeyCode::BTN_MODE),
        (p::LS, KeyCode::BTN_THUMBL),
        (p::RS, KeyCode::BTN_THUMBR),
    ];
    for (_, k) in map {
        keys.insert(k);
    }
    let stick = |c| UinputAbsSetup::new(c, AbsInfo::new(0, -32768, 32767, 16, 128, 0));
    let trig = |c| UinputAbsSetup::new(c, AbsInfo::new(0, 0, 255, 0, 0, 0));
    let hat = |c| UinputAbsSetup::new(c, AbsInfo::new(0, -1, 1, 0, 0, 0));
    let mut ff = AttributeSet::<FFEffectCode>::new();
    ff.insert(FFEffectCode::FF_RUMBLE);
    // Vendor/product of a wired Xbox 360 pad: games and SDL map it right.
    let mut dev = VirtualDevice::builder()?
        .name("QuickRemote Gamepad (Xbox 360)")
        .input_id(InputId::new(BusType::BUS_USB, 0x045e, 0x028e, 0x0114))
        .with_keys(&keys)?
        .with_absolute_axis(&stick(AbsoluteAxisCode::ABS_X))?
        .with_absolute_axis(&stick(AbsoluteAxisCode::ABS_Y))?
        .with_absolute_axis(&stick(AbsoluteAxisCode::ABS_RX))?
        .with_absolute_axis(&stick(AbsoluteAxisCode::ABS_RY))?
        .with_absolute_axis(&trig(AbsoluteAxisCode::ABS_Z))?
        .with_absolute_axis(&trig(AbsoluteAxisCode::ABS_RZ))?
        .with_absolute_axis(&hat(AbsoluteAxisCode::ABS_HAT0X))?
        .with_absolute_axis(&hat(AbsoluteAxisCode::ABS_HAT0Y))?
        .with_ff(&ff)?
        .with_ff_effects_max(16)
        .build()?;
    // Non-blocking reads, so one thread can both apply state and serve
    // force-feedback uploads.
    // SAFETY: fcntl on the device's own open file descriptor.
    unsafe {
        let fd = dev.as_raw_fd();
        let fl = libc::fcntl(fd, libc::F_GETFL);
        libc::fcntl(fd, libc::F_SETFL, fl | libc::O_NONBLOCK);
    }
    let (tx, rx) = mpsc::channel::<(u32, [i16; 6])>();
    std::thread::Builder::new().name(format!("qr-pad-{idx}")).spawn(move || {
        let mut effects: HashMap<i16, (u16, u16, u16)> = HashMap::new();
        loop {
            match rx.recv_timeout(Duration::from_millis(5)) {
                Ok((buttons, axes)) => {
                    let mut e: Vec<InputEvent> =
                        map.iter().map(|(bit, k)| ev(EventType::KEY, k.0, (buttons & bit != 0) as i32)).collect();
                    let a = |c: AbsoluteAxisCode, v: i32| ev(EventType::ABSOLUTE, c.0, v);
                    e.push(a(AbsoluteAxisCode::ABS_X, axes[0] as i32));
                    e.push(a(AbsoluteAxisCode::ABS_Y, axes[1] as i32));
                    e.push(a(AbsoluteAxisCode::ABS_RX, axes[2] as i32));
                    e.push(a(AbsoluteAxisCode::ABS_RY, axes[3] as i32));
                    e.push(a(AbsoluteAxisCode::ABS_Z, (axes[4].max(0) as i32) / 128));
                    e.push(a(AbsoluteAxisCode::ABS_RZ, (axes[5].max(0) as i32) / 128));
                    let hx = (buttons & p::RIGHT != 0) as i32 - (buttons & p::LEFT != 0) as i32;
                    let hy = (buttons & p::DOWN != 0) as i32 - (buttons & p::UP != 0) as i32;
                    e.push(a(AbsoluteAxisCode::ABS_HAT0X, hx));
                    e.push(a(AbsoluteAxisCode::ABS_HAT0Y, hy));
                    e.push(syn());
                    let _ = dev.emit(&e);
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => return,
                Err(mpsc::RecvTimeoutError::Timeout) => {}
            }
            // Force feedback from games.
            let events: Vec<InputEvent> = match dev.fetch_events() {
                Ok(it) => it.collect(),
                Err(_) => continue,
            };
            for e in events {
                match e.destructure() {
                    EventSummary::UInput(ue, UInputCode::UI_FF_UPLOAD, _) => {
                        if let Ok(up) = dev.process_ff_upload(ue) {
                            if let FFEffectKind::Rumble { strong_magnitude, weak_magnitude } = up.effect().kind {
                                effects.insert(
                                    up.effect_id(),
                                    (strong_magnitude, weak_magnitude, up.effect().replay.length),
                                );
                            }
                        }
                    }
                    EventSummary::UInput(ue, UInputCode::UI_FF_ERASE, _) => {
                        if let Ok(er) = dev.process_ff_erase(ue) {
                            effects.remove(&(er.effect_id() as i16));
                        }
                    }
                    EventSummary::ForceFeedback(_, code, value) => {
                        let (s, w, _) = if value > 0 {
                            effects.get(&(code.0 as i16)).copied().unwrap_or((0, 0, 0))
                        } else {
                            (0, 0, 0)
                        };
                        rumble.lock().expect("lock").push((idx, s, w));
                    }
                    _ => {}
                }
            }
        }
    })?;
    Ok(tx)
}

impl Injector for Uinput {
    fn key(&mut self, code: u16, down: bool) -> Result<()> {
        self.kbd.emit(&[ev(EventType::KEY, code, down as i32), syn()])?;
        Ok(())
    }

    fn button(&mut self, b: u8, down: bool) -> Result<()> {
        let k = match b {
            1 => KeyCode::BTN_LEFT,
            2 => KeyCode::BTN_MIDDLE,
            3 => KeyCode::BTN_RIGHT,
            4 => KeyCode::BTN_SIDE,
            5 => KeyCode::BTN_EXTRA,
            _ => return Ok(()),
        };
        self.mouse.emit(&[ev(EventType::KEY, k.0, down as i32), syn()])?;
        Ok(())
    }

    fn abs(&mut self, x: u16, y: u16) -> Result<()> {
        let (w, h) = self.size;
        self.mouse.emit(&[
            ev(EventType::ABSOLUTE, AbsoluteAxisCode::ABS_X.0, (x as i32).min(w - 1)),
            ev(EventType::ABSOLUTE, AbsoluteAxisCode::ABS_Y.0, (y as i32).min(h - 1)),
            syn(),
        ])?;
        Ok(())
    }

    fn rel(&mut self, dx: i32, dy: i32) -> Result<()> {
        self.mouse.emit(&[
            ev(EventType::RELATIVE, RelativeAxisCode::REL_X.0, dx),
            ev(EventType::RELATIVE, RelativeAxisCode::REL_Y.0, dy),
            syn(),
        ])?;
        Ok(())
    }

    fn wheel(&mut self, dx: i32, dy: i32) -> Result<()> {
        // High-resolution wheel events are in the same 1/120 units.
        let mut e = Vec::new();
        if dy != 0 {
            e.push(ev(EventType::RELATIVE, RelativeAxisCode::REL_WHEEL_HI_RES.0, dy));
            if dy.abs() >= 120 {
                e.push(ev(EventType::RELATIVE, RelativeAxisCode::REL_WHEEL.0, dy / 120));
            }
        }
        if dx != 0 {
            e.push(ev(EventType::RELATIVE, RelativeAxisCode::REL_HWHEEL_HI_RES.0, dx));
            if dx.abs() >= 120 {
                e.push(ev(EventType::RELATIVE, RelativeAxisCode::REL_HWHEEL.0, dx / 120));
            }
        }
        if !e.is_empty() {
            e.push(syn());
            self.mouse.emit(&e)?;
        }
        Ok(())
    }

    fn text(&mut self, _text: &str) -> Result<()> {
        // A uinput keyboard has no layout of its own; arbitrary characters
        // need the compositor's input method (or XTest on X11 hosts).
        anyhow::bail!("text input through uinput is not supported; use an X11 source or the client's key events")
    }

    fn pen(&mut self, x: u32, y: u32, pressure: u16, tilt: (i8, i8), flags: u8) -> Result<()> {
        use qr_proto::input::pen;
        let mut e = Vec::new();
        let tool = if flags & pen::IN_RANGE == 0 {
            None
        } else if flags & pen::ERASER != 0 {
            Some(KeyCode::BTN_TOOL_RUBBER)
        } else {
            Some(KeyCode::BTN_TOOL_PEN)
        };
        if tool != self.pen_tool {
            if let Some(old) = self.pen_tool {
                e.push(ev(EventType::KEY, old.0, 0));
            }
            if let Some(new) = tool {
                e.push(ev(EventType::KEY, new.0, 1));
            }
            self.pen_tool = tool;
        }
        let contact = flags & pen::CONTACT != 0;
        if contact != self.pen_contact {
            e.push(ev(EventType::KEY, KeyCode::BTN_TOUCH.0, contact as i32));
            self.pen_contact = contact;
        }
        e.push(ev(EventType::KEY, KeyCode::BTN_STYLUS.0, (flags & pen::BARREL != 0) as i32));
        e.push(ev(EventType::ABSOLUTE, AbsoluteAxisCode::ABS_X.0, x as i32));
        e.push(ev(EventType::ABSOLUTE, AbsoluteAxisCode::ABS_Y.0, y as i32));
        e.push(ev(EventType::ABSOLUTE, AbsoluteAxisCode::ABS_PRESSURE.0, pressure as i32 * 4095 / 65535));
        e.push(ev(EventType::ABSOLUTE, AbsoluteAxisCode::ABS_TILT_X.0, (tilt.0 as i32 * 64 / 90).clamp(-64, 63)));
        e.push(ev(EventType::ABSOLUTE, AbsoluteAxisCode::ABS_TILT_Y.0, (tilt.1 as i32 * 64 / 90).clamp(-64, 63)));
        e.push(syn());
        self.pen.emit(&e)?;
        Ok(())
    }

    fn pad(&mut self, idx: u8, buttons: u32, axes: [i16; 6]) -> Result<()> {
        if idx >= 4 {
            return Ok(());
        }
        if !self.pads.contains_key(&idx) {
            let tx = spawn_pad(idx, self.rumble.clone())?;
            self.pads.insert(idx, tx);
            tracing::info!(idx, "virtual gamepad attached");
        }
        let _ = self.pads[&idx].send((buttons, axes));
        Ok(())
    }

    fn pad_detach(&mut self, idx: u8) -> Result<()> {
        // Dropping the sender ends the pad's thread, which drops the device.
        self.pads.remove(&idx);
        Ok(())
    }

    fn take_rumble(&mut self) -> Vec<(u8, u16, u16)> {
        std::mem::take(&mut *self.rumble.lock().expect("lock"))
    }

    fn name(&self) -> &'static str {
        "uinput"
    }
}
