//! Input injection: client events → the host's display.
//!
//! Backends: XTest into an X11 display (real X11 desktops, Xwayland), and
//! uinput virtual devices (keyboard, mouse, pen, gamepads) that any
//! libinput-based compositor and every game reading evdev picks up.

use std::collections::HashSet;
use std::sync::Mutex;

use anyhow::{Context, Result, bail};
use qr_proto::InputEvent;
use qr_proto::datagram::{Motion, PadState, kind};

use crate::HostCtx;

pub trait Injector: Send {
    fn key(&mut self, code: u16, down: bool) -> Result<()>;
    fn button(&mut self, button: u8, down: bool) -> Result<()>;
    fn abs(&mut self, x: u16, y: u16) -> Result<()>;
    fn rel(&mut self, dx: i32, dy: i32) -> Result<()>;
    /// 1/120-notch units, positive up/right.
    fn wheel(&mut self, dx: i32, dy: i32) -> Result<()>;
    fn text(&mut self, text: &str) -> Result<()>;
    fn pen(&mut self, _x: u32, _y: u32, _pressure: u16, _tilt: (i8, i8), _flags: u8) -> Result<()> {
        Ok(())
    }
    /// Full state of client pad `idx` (attaches a virtual pad on first use).
    fn pad(&mut self, _idx: u8, _buttons: u32, _axes: [i16; 6]) -> Result<()> {
        Ok(())
    }
    fn pad_detach(&mut self, _idx: u8) -> Result<()> {
        Ok(())
    }
    /// Rumble requested by games since the last call: (pad, strong, weak).
    fn take_rumble(&mut self) -> Vec<(u8, u16, u16)> {
        Vec::new()
    }
    fn name(&self) -> &'static str;
}

/// Drops everything (sources without input, e.g. the test pattern).
pub struct NoInput;

impl Injector for NoInput {
    fn key(&mut self, _: u16, _: bool) -> Result<()> {
        Ok(())
    }
    fn button(&mut self, _: u8, _: bool) -> Result<()> {
        Ok(())
    }
    fn abs(&mut self, _: u16, _: u16) -> Result<()> {
        Ok(())
    }
    fn rel(&mut self, _: i32, _: i32) -> Result<()> {
        Ok(())
    }
    fn wheel(&mut self, _: i32, _: i32) -> Result<()> {
        Ok(())
    }
    fn text(&mut self, _: &str) -> Result<()> {
        Ok(())
    }
    fn name(&self) -> &'static str {
        "none"
    }
}

pub mod xtest {
    use super::*;
    use x11rb::connection::{Connection, RequestConnection as _};
    use x11rb::protocol::xproto::{self, ConnectionExt as _};
    use x11rb::protocol::xtest::ConnectionExt as _;
    use x11rb::rust_connection::RustConnection;

    const KEY_PRESS: u8 = 2;
    const KEY_RELEASE: u8 = 3;
    const BUTTON_PRESS: u8 = 4;
    const BUTTON_RELEASE: u8 = 5;
    const MOTION: u8 = 6;

    pub struct XTest {
        conn: RustConnection,
        root: xproto::Window,
        /// Keycodes with no symbols, lent to characters in rotation. An app
        /// reads the keymap when it handles the event, possibly after the
        /// next remap, so a keycode isn't reused until the others have been.
        spares: Vec<u8>,
        /// Which character each spare currently types.
        lent: Vec<Option<char>>,
        next_spare: usize,
        wheel_acc: (i32, i32),
    }

    impl XTest {
        pub fn open(display: &str) -> Result<Self> {
            let (conn, screen) =
                x11rb::connect(Some(display)).with_context(|| format!("opening X display {display}"))?;
            conn.extension_information(x11rb::protocol::xtest::X11_EXTENSION_NAME)?
                .context("the X server has no XTEST extension")?;
            let root = conn.setup().roots[screen].root;
            let spares = find_spares(&conn, 16).unwrap_or_default();
            // Some clients (Xlib with XKB) lose the first keymap change they
            // see after starting; make that a harmless one now, not the
            // first borrowed character (measured with xev: the first
            // borrowed character was dropped in about half the runs).
            if let Some(&kc) = spares.first() {
                conn.change_keyboard_mapping(1, kc, 2, &[0, 0])?;
                conn.get_input_focus()?.reply()?;
            }
            let lent = vec![None; spares.len()];
            Ok(Self { conn, root, spares, lent, next_spare: 0, wheel_acc: (0, 0) })
        }

        fn fake(&self, kind: u8, detail: u8, x: i16, y: i16) -> Result<()> {
            self.conn.xtest_fake_input(kind, detail, x11rb::CURRENT_TIME, self.root, x, y, 0)?;
            Ok(())
        }

        fn flush(&self) -> Result<()> {
            self.conn.flush()?;
            Ok(())
        }
    }

    /// Up to `n` keycodes without symbols, highest first.
    fn find_spares(conn: &RustConnection, n: usize) -> Result<Vec<u8>> {
        let (min, max) = (conn.setup().min_keycode, conn.setup().max_keycode);
        let map = conn.get_keyboard_mapping(min, max - min + 1)?.reply()?;
        let per = map.keysyms_per_keycode as usize;
        Ok(map
            .keysyms
            .chunks(per)
            .enumerate()
            .rev()
            .filter(|(_, syms)| syms.iter().all(|s| *s == 0))
            .map(|(i, _)| min + i as u8)
            .take(n)
            .collect())
    }

    impl Injector for XTest {
        fn key(&mut self, code: u16, down: bool) -> Result<()> {
            // X keycodes are evdev codes + 8 on evdev/Xwayland servers.
            let Ok(kc) = u8::try_from(code + 8) else { return Ok(()) };
            self.fake(if down { KEY_PRESS } else { KEY_RELEASE }, kc, 0, 0)?;
            self.flush()
        }

        fn button(&mut self, button: u8, down: bool) -> Result<()> {
            let b = match button {
                1..=3 => button,
                4 => 8,
                5 => 9,
                _ => return Ok(()),
            };
            self.fake(if down { BUTTON_PRESS } else { BUTTON_RELEASE }, b, 0, 0)?;
            self.flush()
        }

        fn abs(&mut self, x: u16, y: u16) -> Result<()> {
            self.fake(MOTION, 0, x as i16, y as i16)?;
            self.flush()
        }

        fn rel(&mut self, dx: i32, dy: i32) -> Result<()> {
            self.fake(MOTION, 1, dx.clamp(-32768, 32767) as i16, dy.clamp(-32768, 32767) as i16)?;
            self.flush()
        }

        fn wheel(&mut self, dx: i32, dy: i32) -> Result<()> {
            self.wheel_acc.0 += dx;
            self.wheel_acc.1 += dy;
            let mut clicks = Vec::new();
            for (acc, plus, minus) in [(&mut self.wheel_acc.1, 4u8, 5u8), (&mut self.wheel_acc.0, 7, 6)] {
                while *acc >= 120 {
                    *acc -= 120;
                    clicks.push(plus);
                }
                while *acc <= -120 {
                    *acc += 120;
                    clicks.push(minus);
                }
            }
            for b in clicks {
                self.fake(BUTTON_PRESS, b, 0, 0)?;
                self.fake(BUTTON_RELEASE, b, 0, 0)?;
            }
            self.flush()
        }

        /// Types each character: with the layout's own key (plus Shift) when
        /// it has one, else by lending a spare keycode. Spares rotate, so an
        /// app still reading one keystroke never sees its keycode remapped.
        fn text(&mut self, text: &str) -> Result<()> {
            let (min, max) = (self.conn.setup().min_keycode, self.conn.setup().max_keycode);
            let map = self.conn.get_keyboard_mapping(min, max - min + 1)?.reply()?;
            let per = map.keysyms_per_keycode as usize;
            let find = |sym: u32| -> Option<(u8, bool)> {
                map.keysyms.chunks(per).enumerate().find_map(|(i, syms)| {
                    let kc = min + i as u8;
                    if syms.first() == Some(&sym) {
                        Some((kc, false))
                    } else if syms.get(1) == Some(&sym) {
                        Some((kc, true))
                    } else {
                        None
                    }
                })
            };
            const SHIFT: u8 = 42 + 8;
            for ch in text.chars() {
                let ch = if ch == '\r' { '\n' } else { ch };
                let sym: u32 = match ch {
                    '\n' => 0xff0d,
                    '\t' => 0xff09,
                    '\u{8}' => 0xff08,
                    c if (c as u32) < 0x100 => c as u32,
                    c => 0x0100_0000 + c as u32,
                };
                let (kc, shift) = match self.lent.iter().position(|c| *c == Some(ch)) {
                    Some(i) => (self.spares[i], false),
                    None => match find(sym) {
                        Some(k) if !self.spares.contains(&k.0) => k,
                        _ => {
                            if self.spares.is_empty() {
                                tracing::warn!(?ch, "no spare keycode to type this character; skipped");
                                continue;
                            }
                            let i = self.next_spare;
                            self.next_spare = (i + 1) % self.spares.len();
                            // Both levels, so a held Shift doesn't matter.
                            self.conn.change_keyboard_mapping(1, self.spares[i], 2, &[sym, sym])?;
                            self.conn.get_input_focus()?.reply()?;
                            // Apps refresh their keymap when they see the
                            // MappingNotify; give them a moment before the
                            // keystroke that needs the new symbol.
                            std::thread::sleep(std::time::Duration::from_millis(20));
                            self.lent[i] = Some(ch);
                            (self.spares[i], false)
                        }
                    },
                };
                if shift {
                    self.fake(KEY_PRESS, SHIFT, 0, 0)?;
                }
                self.fake(KEY_PRESS, kc, 0, 0)?;
                self.fake(KEY_RELEASE, kc, 0, 0)?;
                if shift {
                    self.fake(KEY_RELEASE, SHIFT, 0, 0)?;
                }
                self.conn.get_input_focus()?.reply()?;
            }
            Ok(())
        }

        fn name(&self) -> &'static str {
            "xtest"
        }
    }

    /// The size of `display`'s first screen.
    pub fn screen_size(display: &str) -> Result<(u16, u16)> {
        let (conn, screen) = x11rb::connect(Some(display))?;
        let s = &conn.setup().roots[screen];
        Ok((s.width_in_pixels, s.height_in_pixels))
    }
}

/// Per-session input state: held keys/buttons (released when the session
/// ends or the stream breaks) and the last applied motion totals.
#[derive(Default)]
pub struct InputState {
    held_keys: HashSet<u16>,
    held_buttons: HashSet<u8>,
    motion_seq: Option<u32>,
    rel: (i64, i64),
    pad_seq: std::collections::HashMap<u8, u32>,
}

pub struct Input {
    pub injector: Mutex<Box<dyn Injector>>,
    pub state: Mutex<InputState>,
    /// Where each stream's picture sits on the desktop: absolute positions
    /// are stream pixels plus this.
    origins: Mutex<std::collections::HashMap<u8, (i32, i32)>>,
}

impl Input {
    pub fn new(injector: Box<dyn Injector>) -> Self {
        Self { injector: Mutex::new(injector), state: Mutex::default(), origins: Mutex::default() }
    }

    /// Records where stream `stream`'s monitor sits on the desktop.
    pub fn set_origin(&self, stream: u8, origin: (i32, i32)) {
        self.origins.lock().expect("lock").insert(stream, origin);
    }

    /// A stream position on the desktop.
    fn place(&self, stream: u8, x: u16, y: u16) -> (u16, u16) {
        let (ox, oy) = self.origins.lock().expect("lock").get(&stream).copied().unwrap_or((0, 0));
        ((x as i32 + ox).clamp(0, u16::MAX as i32) as u16, (y as i32 + oy).clamp(0, u16::MAX as i32) as u16)
    }

    pub fn event(&self, ev: &InputEvent) {
        let mut inj = self.injector.lock().expect("lock");
        let mut st = self.state.lock().expect("lock");
        let r = match *ev {
            InputEvent::Key { code, down } => {
                if down {
                    st.held_keys.insert(code);
                } else {
                    st.held_keys.remove(&code);
                }
                inj.key(code, down)
            }
            InputEvent::Button { button, down } => {
                if down {
                    st.held_buttons.insert(button);
                } else {
                    st.held_buttons.remove(&button);
                }
                inj.button(button, down)
            }
            InputEvent::Abs { stream, x, y } => {
                let (x, y) = self.place(stream, x, y);
                inj.abs(x, y)
            }
            InputEvent::Wheel { dx, dy } => inj.wheel(dx as i32, dy as i32),
            InputEvent::Text { ref text } => inj.text(text),
            InputEvent::ReleaseAll => {
                drop(st);
                drop(inj);
                self.release_all();
                return;
            }
            InputEvent::Pen { x, y, pressure, tilt_x, tilt_y, flags, .. } => {
                inj.pen(x, y, pressure, (tilt_x, tilt_y), flags)
            }
        };
        if let Err(e) = r {
            tracing::debug!("input injection failed: {e:#}");
        }
        if qr_net::trace::enabled() {
            qr_net::trace::event(&trace_label(ev));
        }
    }

    /// Applies a motion datagram: relative motion as the difference of the
    /// running totals, absolute position as is; older datagrams are skipped.
    pub fn motion(&self, m: &Motion) {
        let mut st = self.state.lock().expect("lock");
        if st.motion_seq.is_some_and(|s| m.seq.wrapping_sub(s) > u32::MAX / 2 || m.seq == s) {
            return;
        }
        st.motion_seq = Some(m.seq);
        let (dx, dy) = (m.rel_x - st.rel.0, m.rel_y - st.rel.1);
        st.rel = (m.rel_x, m.rel_y);
        drop(st);
        let mut inj = self.injector.lock().expect("lock");
        let r = if dx != 0 || dy != 0 {
            inj.rel(dx as i32, dy as i32)
        } else if m.has_abs {
            let (x, y) = self.place(m.stream, m.abs_x, m.abs_y);
            inj.abs(x, y)
        } else {
            Ok(())
        };
        if let Err(e) = r {
            tracing::debug!("motion injection failed: {e:#}");
        }
    }

    /// Applies a pad snapshot, newest by `seq` only.
    pub fn pad(&self, p: &PadState) {
        {
            let mut st = self.state.lock().expect("lock");
            let last = st.pad_seq.entry(p.idx).or_insert(0);
            if *last != 0 && p.seq.wrapping_sub(*last) > u32::MAX / 2 {
                return;
            }
            *last = p.seq;
        }
        if let Err(e) = self.injector.lock().expect("lock").pad(p.idx, p.buttons, p.axes) {
            tracing::debug!("pad injection failed: {e:#}");
        }
    }

    pub fn pad_detach(&self, idx: u8) {
        self.state.lock().expect("lock").pad_seq.remove(&idx);
        let _ = self.injector.lock().expect("lock").pad_detach(idx);
    }

    /// Releases every held key and button (session start and end).
    pub fn release_all(&self) {
        let mut st = self.state.lock().expect("lock");
        let keys: Vec<u16> = st.held_keys.drain().collect();
        let buttons: Vec<u8> = st.held_buttons.drain().collect();
        drop(st);
        let mut inj = self.injector.lock().expect("lock");
        for k in keys {
            let _ = inj.key(k, false);
        }
        for b in buttons {
            let _ = inj.button(b, false);
        }
    }

    /// A new session: forget motion totals.
    pub fn reset(&self) {
        self.release_all();
        let mut st = self.state.lock().expect("lock");
        st.motion_seq = None;
        st.rel = (0, 0);
    }
}

/// The label both sides use for an event in the input trace.
pub fn trace_label(ev: &InputEvent) -> String {
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

pub fn datagram(ctx: &HostCtx, d: &[u8]) {
    match d.first() {
        Some(&kind::MOTION) => {
            if let Ok(m) = Motion::decode(d) {
                ctx.input.motion(&m);
            }
        }
        Some(&kind::PAD) => {
            if let Ok(p) = PadState::decode(d) {
                ctx.input.pad(&p);
            }
        }
        _ => {}
    }
}

/// `main` for keys and pointer, plus uinput for pads and pen when allowed.
#[cfg(target_os = "linux")]
pub fn with_devices(main: Box<dyn Injector>, name: &'static str) -> Box<dyn Injector> {
    match crate::uinput::Uinput::open(7680, 4320) {
        Ok(u) => Box::new(Split { main, devices: Box::new(u), name }),
        Err(e) => {
            tracing::warn!("gamepads and pen are off: {e:#}");
            main
        }
    }
}

/// Builds the injector named by `--input`. `vd_socket`: the virtual
/// display's Wayland socket, when it is the source.
pub fn open(spec: &str, source: &str, vd_socket: Option<&std::path::Path>) -> Result<Box<dyn Injector>> {
    let (kind, arg) = spec.split_once(':').unwrap_or((spec, ""));
    Ok(match kind {
        "none" => Box::new(NoInput),
        // The virtual display: its compositor's virtual pointer and
        // keyboard, plus uinput for pads and pen where allowed.
        #[cfg(target_os = "linux")]
        "auto" | "wayland" if vd_socket.is_some() => {
            let wl = crate::wlinput::WlInput::open(vd_socket.expect("checked"))?;
            with_devices(Box::new(wl), "wayland+uinput")
        }
        "xtest" => Box::new(xtest::XTest::open(if arg.is_empty() { ":0" } else { arg })?),
        #[cfg(target_os = "linux")]
        "uinput" => {
            let (w, h) =
                arg.split_once('x').and_then(|(w, h)| Some((w.parse().ok()?, h.parse().ok()?))).unwrap_or((1920, 1080));
            Box::new(crate::uinput::Uinput::open(w, h)?)
        }
        #[cfg(windows)]
        "auto" | "windows" => Box::new(win::Win::new(source)),
        #[cfg(not(windows))]
        "auto" => match source.split_once(':') {
            Some(("x11", display)) => {
                let x = xtest::XTest::open(display)?;
                // Pads and pen need uinput; without it they're dropped.
                #[cfg(target_os = "linux")]
                {
                    let (w, h) = xtest::screen_size(display).unwrap_or((1920, 1080));
                    match crate::uinput::Uinput::open(w as i32, h as i32) {
                        Ok(u) => {
                            return Ok(Box::new(Split {
                                main: Box::new(x),
                                devices: Box::new(u),
                                name: "xtest+uinput",
                            }));
                        }
                        Err(e) => tracing::warn!("gamepads and pen are off: {e:#}"),
                    }
                }
                Box::new(x)
            }
            _ => Box::new(NoInput),
        },
        other => bail!("unknown --input backend {other} (none, xtest:<display>, uinput[:WxH])"),
    })
}

/// Windows: SendInput, synthetic pen, ViGEmBus pads (crate qr-win).
#[cfg(windows)]
mod win {
    use super::*;
    use qr_win::inject::{Injector as WinInjector, Rect};

    pub struct Win(WinInjector);

    impl Win {
        /// `source` is `d3d11:<monitor index>`; the monitor's rectangle maps
        /// stream pixels onto the virtual desktop.
        pub fn new(_source: &str) -> Self {
            // The primary monitor at the origin until monitor enumeration
            // (T4.3) supplies the rectangle.
            Win(WinInjector::new(Rect { x: 0, y: 0, w: 0, h: 0 }))
        }
    }

    impl Injector for Win {
        fn key(&mut self, code: u16, down: bool) -> Result<()> {
            self.0.key(code, down)
        }
        fn button(&mut self, b: u8, down: bool) -> Result<()> {
            self.0.button(b, down)
        }
        fn abs(&mut self, x: u16, y: u16) -> Result<()> {
            self.0.abs(x, y)
        }
        fn rel(&mut self, dx: i32, dy: i32) -> Result<()> {
            self.0.rel(dx, dy)
        }
        fn wheel(&mut self, dx: i32, dy: i32) -> Result<()> {
            self.0.wheel(dx, dy)
        }
        fn text(&mut self, text: &str) -> Result<()> {
            self.0.text(text)
        }
        fn pen(&mut self, x: u32, y: u32, pressure: u16, tilt: (i8, i8), flags: u8) -> Result<()> {
            self.0.pen(x, y, pressure, tilt, flags)
        }
        fn pad(&mut self, idx: u8, buttons: u32, axes: [i16; 6]) -> Result<()> {
            match self.0.pads.as_mut() {
                Some(p) => p.update(idx, buttons, axes),
                None => Ok(()),
            }
        }
        fn pad_detach(&mut self, idx: u8) -> Result<()> {
            match self.0.pads.as_mut() {
                Some(p) => p.detach(idx),
                None => Ok(()),
            }
        }
        fn take_rumble(&mut self) -> Vec<(u8, u16, u16)> {
            let Some(p) = self.0.pads.as_ref() else { return Vec::new() };
            let mut q = p.rumble.lock().expect("lock");
            q.drain(..).map(|(i, s, w)| (i, s as u16 * 257, w as u16 * 257)).collect()
        }
        fn name(&self) -> &'static str {
            "windows"
        }
    }
}

/// Keyboard, mouse and text through one backend (XTest), pads and pen
/// through another (uinput).
#[cfg(not(windows))]
struct Split {
    main: Box<dyn Injector>,
    devices: Box<dyn Injector>,
    name: &'static str,
}

#[cfg(not(windows))]
impl Injector for Split {
    fn key(&mut self, code: u16, down: bool) -> Result<()> {
        self.main.key(code, down)
    }
    fn button(&mut self, b: u8, down: bool) -> Result<()> {
        self.main.button(b, down)
    }
    fn abs(&mut self, x: u16, y: u16) -> Result<()> {
        self.main.abs(x, y)
    }
    fn rel(&mut self, dx: i32, dy: i32) -> Result<()> {
        self.main.rel(dx, dy)
    }
    fn wheel(&mut self, dx: i32, dy: i32) -> Result<()> {
        self.main.wheel(dx, dy)
    }
    fn text(&mut self, text: &str) -> Result<()> {
        self.main.text(text)
    }
    fn pen(&mut self, x: u32, y: u32, pressure: u16, tilt: (i8, i8), flags: u8) -> Result<()> {
        self.devices.pen(x, y, pressure, tilt, flags)
    }
    fn pad(&mut self, idx: u8, buttons: u32, axes: [i16; 6]) -> Result<()> {
        self.devices.pad(idx, buttons, axes)
    }
    fn pad_detach(&mut self, idx: u8) -> Result<()> {
        self.devices.pad_detach(idx)
    }
    fn take_rumble(&mut self) -> Vec<(u8, u16, u16)> {
        self.devices.take_rumble()
    }
    fn name(&self) -> &'static str {
        self.name
    }
}
