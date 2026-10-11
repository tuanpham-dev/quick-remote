//! Input into the virtual display through the compositor's virtual input
//! protocols (wlr-virtual-pointer, virtual-keyboard): no uinput, no device
//! permissions.
//!
//! Keys use the default keymap (libxkbcommon's, from the `XKB_DEFAULT_*`
//! environment, as the compositor's own keyboards do).
//! Text is typed the way `wtype` does it: a throwaway keymap holds one key
//! per character (as `U+XXXX` keysyms), then the normal keymap comes back.
//! Absolute positions are in output-layout pixels, mapped over the layout's
//! bounding box (from xdg-output: wlroots puts every wl_output at 0,0).

use std::io::Write as _;
use std::os::fd::{AsFd, AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::Instant;

use anyhow::{Context, Result, bail};
use wayland_client::protocol::{wl_keyboard, wl_output, wl_pointer, wl_registry, wl_seat};
use wayland_client::{Connection, Dispatch, EventQueue, QueueHandle, delegate_noop};
use wayland_protocols::xdg::xdg_output::zv1::client::zxdg_output_manager_v1::ZxdgOutputManagerV1;
use wayland_protocols::xdg::xdg_output::zv1::client::zxdg_output_v1::{self as xdg_output, ZxdgOutputV1};
use wayland_protocols_misc::zwp_virtual_keyboard_v1::client::zwp_virtual_keyboard_manager_v1::ZwpVirtualKeyboardManagerV1;
use wayland_protocols_misc::zwp_virtual_keyboard_v1::client::zwp_virtual_keyboard_v1::ZwpVirtualKeyboardV1;
use wayland_protocols_wlr::virtual_pointer::v1::client::zwlr_virtual_pointer_manager_v1::ZwlrVirtualPointerManagerV1;
use wayland_protocols_wlr::virtual_pointer::v1::client::zwlr_virtual_pointer_v1::ZwlrVirtualPointerV1;

use crate::input::Injector;

struct Output {
    output: wl_output::WlOutput,
    xdg: Option<ZxdgOutputV1>,
    x: i32,
    y: i32,
    w: i32,
    h: i32,
}

#[derive(Default)]
struct State {
    seat: Option<wl_seat::WlSeat>,
    pointer_mgr: Option<ZwlrVirtualPointerManagerV1>,
    keyboard_mgr: Option<ZwpVirtualKeyboardManagerV1>,
    xdg_mgr: Option<ZxdgOutputManagerV1>,
    outputs: Vec<Output>,
}

impl State {
    /// Asks for the layout position of outputs that don't have it yet.
    fn track_outputs(&mut self, qh: &QueueHandle<Self>) {
        let Some(m) = &self.xdg_mgr else { return };
        for (i, o) in self.outputs.iter_mut().enumerate() {
            if o.xdg.is_none() {
                o.xdg = Some(m.get_xdg_output(&o.output, qh, i));
            }
        }
    }
}

impl Dispatch<wl_registry::WlRegistry, ()> for State {
    fn event(
        st: &mut Self,
        reg: &wl_registry::WlRegistry,
        ev: wl_registry::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let wl_registry::Event::Global { name, interface, version } = ev {
            match interface.as_str() {
                "wl_seat" if st.seat.is_none() => st.seat = Some(reg.bind(name, version.min(5), qh, ())),
                "zwlr_virtual_pointer_manager_v1" => st.pointer_mgr = Some(reg.bind(name, version.min(2), qh, ())),
                "zwp_virtual_keyboard_manager_v1" => st.keyboard_mgr = Some(reg.bind(name, 1, qh, ())),
                "zxdg_output_manager_v1" => st.xdg_mgr = Some(reg.bind(name, version.min(3), qh, ())),
                "wl_output" => {
                    let output = reg.bind(name, version.min(2), qh, ());
                    st.outputs.push(Output { output, xdg: None, x: 0, y: 0, w: 0, h: 0 });
                }
                _ => {}
            }
            st.track_outputs(qh);
        }
    }
}

impl Dispatch<ZxdgOutputV1, usize> for State {
    fn event(st: &mut Self, _: &ZxdgOutputV1, ev: xdg_output::Event, i: &usize, _: &Connection, _: &QueueHandle<Self>) {
        let o = &mut st.outputs[*i];
        match ev {
            xdg_output::Event::LogicalPosition { x, y } => (o.x, o.y) = (x, y),
            xdg_output::Event::LogicalSize { width, height } => (o.w, o.h) = (width, height),
            _ => {}
        }
    }
}

delegate_noop!(State: ignore wl_seat::WlSeat);
delegate_noop!(State: ignore wl_output::WlOutput);
delegate_noop!(State: ZxdgOutputManagerV1);
delegate_noop!(State: ignore wl_pointer::WlPointer);
delegate_noop!(State: ZwlrVirtualPointerManagerV1);
delegate_noop!(State: ZwlrVirtualPointerV1);
delegate_noop!(State: ZwpVirtualKeyboardManagerV1);
delegate_noop!(State: ZwpVirtualKeyboardV1);

/// The layout the virtual keyboard's keymap is built with (`--keyboard-layout`).
static LAYOUT: std::sync::OnceLock<String> = std::sync::OnceLock::new();

/// Sets the XKB layout for every keymap built from now on (before the
/// first keyboard opens).
pub(crate) fn set_layout(layout: &str) {
    let _ = LAYOUT.set(layout.to_string());
}

/// The default keymap as text (libxkbcommon, loaded at run time): the
/// configured layout, else the `XKB_DEFAULT_*` environment or the system
/// default.
pub(crate) fn default_keymap() -> Result<String> {
    use xkbcommon_dl::{xkb_context_flags, xkb_keymap_compile_flags, xkb_keymap_format, xkb_rule_names};
    let layout = LAYOUT.get().map(|l| std::ffi::CString::new(l.as_str())).transpose()?;
    let x = xkbcommon_dl::xkbcommon_option().context("libxkbcommon is not installed")?;
    // SAFETY: libxkbcommon calls as documented; every object is released.
    unsafe {
        let ctx = (x.xkb_context_new)(xkb_context_flags::XKB_CONTEXT_NO_FLAGS);
        if ctx.is_null() {
            bail!("libxkbcommon: no context");
        }
        // No names: the XKB_DEFAULT_* environment, else the system default.
        let names = layout.as_ref().map(|l| xkb_rule_names {
            rules: std::ptr::null(),
            model: std::ptr::null(),
            layout: l.as_ptr(),
            variant: std::ptr::null(),
            options: std::ptr::null(),
        });
        let km = (x.xkb_keymap_new_from_names)(
            ctx,
            names.as_ref().map_or(std::ptr::null(), |n| n as *const _),
            xkb_keymap_compile_flags::XKB_KEYMAP_COMPILE_NO_FLAGS,
        );
        (x.xkb_context_unref)(ctx);
        if km.is_null() {
            bail!("compiling the default keymap failed");
        }
        let p = (x.xkb_keymap_get_as_string)(km, xkb_keymap_format::XKB_KEYMAP_FORMAT_TEXT_V1);
        (x.xkb_keymap_unref)(km);
        if p.is_null() {
            bail!("libxkbcommon returned no keymap text");
        }
        let text = std::ffi::CStr::from_ptr(p).to_string_lossy().into_owned();
        libc::free(p as *mut libc::c_void);
        Ok(text)
    }
}

/// A keymap in a memfd, as the protocol wants it.
fn keymap_fd(text: &str) -> Result<(OwnedFd, u32)> {
    // SAFETY: plain syscall; the fd is owned right away.
    let fd = unsafe {
        let raw = libc::memfd_create(c"qr-keymap".as_ptr(), libc::MFD_CLOEXEC);
        if raw < 0 {
            bail!("memfd_create: {}", std::io::Error::last_os_error());
        }
        OwnedFd::from_raw_fd(raw)
    };
    let mut f = std::fs::File::from(fd);
    f.write_all(text.as_bytes())?;
    f.write_all(&[0])?;
    Ok((f.into(), text.len() as u32 + 1))
}

/// A keymap with one key per character, keycodes from 9 (evdev 1) up.
fn text_keymap(chars: &[char]) -> String {
    let mut s = String::from("xkb_keymap {\nxkb_keycodes \"qr\" {\nminimum = 8;\n");
    s += &format!("maximum = {};\n", chars.len() + 9);
    for i in 0..chars.len() {
        s += &format!("<K{i}> = {};\n", i + 9);
    }
    s += "};\nxkb_types \"qr\" { include \"complete\" };\nxkb_compatibility \"qr\" { include \"complete\" };\nxkb_symbols \"qr\" {\n";
    for (i, c) in chars.iter().enumerate() {
        s += &format!("key <K{i}> {{[ U{:04X} ]}};\n", *c as u32);
    }
    s += "};\n};\n";
    s
}

pub struct WlInput {
    queue: EventQueue<State>,
    state: State,
    pointer: ZwlrVirtualPointerV1,
    keyboard: ZwpVirtualKeyboardV1,
    /// The normal keymap, restored after text.
    keymap: String,
    /// The keyboard's modifier state, which a virtual keyboard has to send
    /// itself (else Shift+7 types 7): libxkbcommon tracks it on the normal
    /// keymap. None when libxkbcommon is missing.
    xkb: Option<XkbState>,
    t0: Instant,
    _conn: Connection,
}

/// libxkbcommon's key state for a keymap.
struct XkbState {
    state: *mut xkbcommon_dl::xkb_state,
    /// (depressed, latched, locked, group) last sent.
    sent: (u32, u32, u32, u32),
}

impl XkbState {
    fn new(keymap: &str) -> Option<Self> {
        use xkbcommon_dl::{xkb_context_flags, xkb_keymap_compile_flags, xkb_keymap_format};
        let x = xkbcommon_dl::xkbcommon_option()?;
        let text = std::ffi::CString::new(keymap).ok()?;
        // SAFETY: libxkbcommon calls as documented; the state keeps its own
        // references to the keymap and context.
        unsafe {
            let ctx = (x.xkb_context_new)(xkb_context_flags::XKB_CONTEXT_NO_FLAGS);
            if ctx.is_null() {
                return None;
            }
            let km = (x.xkb_keymap_new_from_string)(
                ctx,
                text.as_ptr(),
                xkb_keymap_format::XKB_KEYMAP_FORMAT_TEXT_V1,
                xkb_keymap_compile_flags::XKB_KEYMAP_COMPILE_NO_FLAGS,
            );
            (x.xkb_context_unref)(ctx);
            if km.is_null() {
                return None;
            }
            let state = (x.xkb_state_new)(km);
            (x.xkb_keymap_unref)(km);
            (!state.is_null()).then_some(Self { state, sent: (0, 0, 0, 0) })
        }
    }

    /// Applies an evdev key change: the new modifiers when they changed.
    fn key(&mut self, code: u16, down: bool) -> Option<(u32, u32, u32, u32)> {
        use xkbcommon_dl::{xkb_key_direction, xkb_state_component as C};
        let x = xkbcommon_dl::xkbcommon_option()?;
        let dir = if down { xkb_key_direction::XKB_KEY_DOWN } else { xkb_key_direction::XKB_KEY_UP };
        // SAFETY: a live state; XKB keycodes are evdev + 8.
        let now = unsafe {
            (x.xkb_state_update_key)(self.state, code as u32 + 8, dir);
            (
                (x.xkb_state_serialize_mods)(self.state, C::XKB_STATE_MODS_DEPRESSED),
                (x.xkb_state_serialize_mods)(self.state, C::XKB_STATE_MODS_LATCHED),
                (x.xkb_state_serialize_mods)(self.state, C::XKB_STATE_MODS_LOCKED),
                (x.xkb_state_serialize_layout)(self.state, C::XKB_STATE_LAYOUT_EFFECTIVE),
            )
        };
        (now != self.sent).then(|| {
            self.sent = now;
            now
        })
    }
}

impl Drop for XkbState {
    fn drop(&mut self) {
        if let Some(x) = xkbcommon_dl::xkbcommon_option() {
            // SAFETY: our own reference.
            unsafe { (x.xkb_state_unref)(self.state) };
        }
    }
}

// SAFETY: the Wayland objects are only used through `&mut self`.
unsafe impl Send for WlInput {}

impl WlInput {
    /// Connects to the compositor at `socket`.
    pub fn open(socket: &Path) -> Result<Self> {
        let stream = UnixStream::connect(socket).with_context(|| format!("connecting to {}", socket.display()))?;
        let conn = Connection::from_socket(stream)?;
        let mut queue = conn.new_event_queue();
        let qh = queue.handle();
        let _registry = conn.display().get_registry(&qh, ());
        let mut state = State::default();
        queue.roundtrip(&mut state)?;
        queue.roundtrip(&mut state)?;
        let seat = state.seat.clone().context("the compositor has no seat")?;
        let pointer_mgr = state.pointer_mgr.clone().context("the compositor has no wlr-virtual-pointer")?;
        let keyboard_mgr = state.keyboard_mgr.clone().context("the compositor has no virtual-keyboard")?;

        let keymap = default_keymap()?;
        let pointer = pointer_mgr.create_virtual_pointer(Some(&seat), &qh, ());
        let keyboard = keyboard_mgr.create_virtual_keyboard(&seat, &qh, ());
        let (fd, size) = keymap_fd(&keymap)?;
        keyboard.keymap(wl_keyboard::KeymapFormat::XkbV1.into(), fd.as_fd(), size);
        queue.roundtrip(&mut state)?;
        let outputs: Vec<_> = state.outputs.iter().map(|o| (o.x, o.y, o.w, o.h)).collect();
        tracing::info!(?outputs, "virtual input ready");
        let xkb = XkbState::new(&keymap);
        if xkb.is_none() {
            tracing::warn!("no keyboard state (libxkbcommon): Shift and other modifiers won't apply");
        }
        Ok(Self { queue, state, pointer, keyboard, keymap, xkb, t0: Instant::now(), _conn: conn })
    }

    fn time(&self) -> u32 {
        self.t0.elapsed().as_millis() as u32
    }

    /// Sends what is queued and takes in output changes.
    fn flush(&mut self) -> Result<()> {
        self.queue.flush()?;
        if let Some(guard) = self.queue.prepare_read() {
            let mut pfd = libc::pollfd { fd: guard.connection_fd().as_raw_fd(), events: libc::POLLIN, revents: 0 };
            // SAFETY: one valid pollfd; no wait.
            if unsafe { libc::poll(&mut pfd, 1, 0) } > 0 {
                guard.read()?;
            }
        }
        self.queue.dispatch_pending(&mut self.state)?;
        Ok(())
    }

    /// The output layout's bounding box.
    fn extent(&self) -> (u32, u32) {
        let w = self.state.outputs.iter().map(|o| o.x + o.w).max().unwrap_or(1);
        let h = self.state.outputs.iter().map(|o| o.y + o.h).max().unwrap_or(1);
        (w.max(1) as u32, h.max(1) as u32)
    }

    fn set_keymap(&mut self, text: &str) -> Result<()> {
        let (fd, size) = keymap_fd(text)?;
        self.keyboard.keymap(wl_keyboard::KeymapFormat::XkbV1.into(), fd.as_fd(), size);
        Ok(())
    }
}

impl Injector for WlInput {
    fn key(&mut self, code: u16, down: bool) -> Result<()> {
        let t = self.time();
        self.keyboard.key(t, code as u32, down as u32);
        if let Some((depressed, latched, locked, group)) = self.xkb.as_mut().and_then(|x| x.key(code, down)) {
            self.keyboard.modifiers(depressed, latched, locked, group);
        }
        self.flush()
    }

    fn button(&mut self, button: u8, down: bool) -> Result<()> {
        // Linux BTN_LEFT, BTN_MIDDLE, BTN_RIGHT, BTN_SIDE, BTN_EXTRA.
        let code = match button {
            1 => 0x110,
            2 => 0x112,
            3 => 0x111,
            4 => 0x113,
            5 => 0x114,
            _ => return Ok(()),
        };
        let t = self.time();
        let state = if down { wl_pointer::ButtonState::Pressed } else { wl_pointer::ButtonState::Released };
        self.pointer.button(t, code, state);
        self.pointer.frame();
        self.flush()
    }

    fn abs(&mut self, x: u16, y: u16) -> Result<()> {
        // Take in layout changes first (a resize or scale since the last
        // event): the position maps over the layout as it is now.
        self.flush()?;
        let (w, h) = self.extent();
        let t = self.time();
        self.pointer.motion_absolute(t, (x as u32).min(w - 1), (y as u32).min(h - 1), w, h);
        self.pointer.frame();
        self.flush()
    }

    fn rel(&mut self, dx: i32, dy: i32) -> Result<()> {
        let t = self.time();
        self.pointer.motion(t, dx as f64, dy as f64);
        self.pointer.frame();
        self.flush()
    }

    fn wheel(&mut self, dx: i32, dy: i32) -> Result<()> {
        // 1/120 notches, positive up/right; Wayland axes are positive
        // down/right, 15 units a notch.
        let t = self.time();
        self.pointer.axis_source(wl_pointer::AxisSource::Wheel);
        for (axis, v) in [(wl_pointer::Axis::VerticalScroll, -dy), (wl_pointer::Axis::HorizontalScroll, dx)] {
            if v != 0 {
                let notches = v / 120;
                if notches != 0 {
                    self.pointer.axis_discrete(t, axis, v as f64 * 15.0 / 120.0, notches);
                } else {
                    self.pointer.axis(t, axis, v as f64 * 15.0 / 120.0);
                }
            }
        }
        self.pointer.frame();
        self.flush()
    }

    fn text(&mut self, text: &str) -> Result<()> {
        let mut chars: Vec<char> = text.chars().filter(|c| !c.is_control() || *c == '\n' || *c == '\t').collect();
        chars.sort_unstable();
        chars.dedup();
        if chars.is_empty() {
            return Ok(());
        }
        // Newline and tab as their own keysyms (Return, Tab), not U000A.
        let keymap = text_keymap(&chars).replace("U000A", "Return").replace("U0009", "Tab");
        self.set_keymap(&keymap)?;
        // Held modifiers would change what the text keys type.
        self.keyboard.modifiers(0, 0, 0, 0);
        for c in text.chars() {
            let Ok(i) = chars.binary_search(&c) else { continue };
            let t = self.time();
            self.keyboard.key(t, i as u32 + 1, 1);
            self.keyboard.key(t, i as u32 + 1, 0);
        }
        let normal = self.keymap.clone();
        self.set_keymap(&normal)?;
        if let Some((depressed, latched, locked, group)) = self.xkb.as_ref().map(|x| x.sent) {
            self.keyboard.modifiers(depressed, latched, locked, group);
        }
        self.flush()
    }

    fn name(&self) -> &'static str {
        "wayland"
    }
}
