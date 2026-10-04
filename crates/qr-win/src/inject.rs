//! Input injection on Windows.

use std::sync::{Arc, Mutex};

use anyhow::{Context, Result, bail};
use windows::Win32::Foundation::{HWND, POINT};
use windows::Win32::UI::Controls::{
    CreateSyntheticPointerDevice, HSYNTHETICPOINTERDEVICE, POINTER_FEEDBACK_DEFAULT, POINTER_TYPE_INFO,
    POINTER_TYPE_INFO_0,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    INPUT, INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE, KEYBDINPUT, KEYEVENTF_EXTENDEDKEY, KEYEVENTF_KEYUP,
    KEYEVENTF_SCANCODE, KEYEVENTF_UNICODE, MOUSE_EVENT_FLAGS, MOUSEEVENTF_ABSOLUTE, MOUSEEVENTF_HWHEEL,
    MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP, MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP, MOUSEEVENTF_MOVE,
    MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP, MOUSEEVENTF_VIRTUALDESK, MOUSEEVENTF_WHEEL, MOUSEEVENTF_XDOWN,
    MOUSEEVENTF_XUP, MOUSEINPUT, SendInput, VIRTUAL_KEY,
};
use windows::Win32::UI::Input::Pointer::{
    InjectSyntheticPointerInput, POINTER_FLAG_DOWN, POINTER_FLAG_INCONTACT, POINTER_FLAG_INRANGE, POINTER_FLAG_UP,
    POINTER_FLAG_UPDATE, POINTER_INFO, POINTER_PEN_INFO,
};
use windows::Win32::UI::WindowsAndMessaging::{
    GetSystemMetrics, PEN_FLAG_BARREL, PEN_FLAG_ERASER, PEN_MASK_PRESSURE, PEN_MASK_TILT_X, PEN_MASK_TILT_Y, PT_PEN,
    SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN,
};

fn send(inputs: &[INPUT]) -> Result<()> {
    // SAFETY: `inputs` is a valid slice of INPUT structures.
    let n = unsafe { SendInput(inputs, std::mem::size_of::<INPUT>() as i32) };
    if n as usize != inputs.len() {
        bail!("SendInput injected {n} of {} events (blocked by UIPI or a secure desktop?)", inputs.len());
    }
    Ok(())
}

fn key_input(scan: u16, flags: windows::Win32::UI::Input::KeyboardAndMouse::KEYBD_EVENT_FLAGS) -> INPUT {
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT { wVk: VIRTUAL_KEY(0), wScan: scan, dwFlags: flags, time: 0, dwExtraInfo: 0 },
        },
    }
}

fn mouse_input(dx: i32, dy: i32, data: i32, flags: MOUSE_EVENT_FLAGS) -> INPUT {
    INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT { dx, dy, mouseData: data as u32, dwFlags: flags, time: 0, dwExtraInfo: 0 },
        },
    }
}

/// The streamed monitor's rectangle on the virtual desktop.
#[derive(Debug, Clone, Copy)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

pub struct Injector {
    monitor: Rect,
    pen: Option<HSYNTHETICPOINTERDEVICE>,
    pen_down: bool,
    pub pads: Option<Pads>,
}

// SAFETY: the pen device handle is only used from the owning thread through
// `&mut self`.
unsafe impl Send for Injector {}

impl Injector {
    pub fn new(monitor: Rect) -> Self {
        let pads = match Pads::open() {
            Ok(p) => Some(p),
            Err(e) => {
                tracing::warn!("gamepads are off: {e:#} (install ViGEmBus)");
                None
            }
        };
        Self { monitor, pen: None, pen_down: false, pads }
    }

    pub fn key(&mut self, evdev: u16, down: bool) -> Result<()> {
        let Some((scan, ext)) = crate::scancode(evdev) else { return Ok(()) };
        let mut f = KEYEVENTF_SCANCODE;
        if ext {
            f |= KEYEVENTF_EXTENDEDKEY;
        }
        if !down {
            f |= KEYEVENTF_KEYUP;
        }
        send(&[key_input(scan, f)])
    }

    pub fn button(&mut self, b: u8, down: bool) -> Result<()> {
        let (flag, data) = match (b, down) {
            (1, true) => (MOUSEEVENTF_LEFTDOWN, 0),
            (1, false) => (MOUSEEVENTF_LEFTUP, 0),
            (2, true) => (MOUSEEVENTF_MIDDLEDOWN, 0),
            (2, false) => (MOUSEEVENTF_MIDDLEUP, 0),
            (3, true) => (MOUSEEVENTF_RIGHTDOWN, 0),
            (3, false) => (MOUSEEVENTF_RIGHTUP, 0),
            (4, d) => (if d { MOUSEEVENTF_XDOWN } else { MOUSEEVENTF_XUP }, 1),
            (5, d) => (if d { MOUSEEVENTF_XDOWN } else { MOUSEEVENTF_XUP }, 2),
            _ => return Ok(()),
        };
        send(&[mouse_input(0, 0, data, flag)])
    }

    /// Absolute position in stream pixels of the streamed monitor.
    pub fn abs(&mut self, x: u16, y: u16) -> Result<()> {
        // SAFETY: GetSystemMetrics has no preconditions.
        let (vx, vy, vw, vh) = unsafe {
            (
                GetSystemMetrics(SM_XVIRTUALSCREEN),
                GetSystemMetrics(SM_YVIRTUALSCREEN),
                GetSystemMetrics(SM_CXVIRTUALSCREEN).max(1),
                GetSystemMetrics(SM_CYVIRTUALSCREEN).max(1),
            )
        };
        let px = self.monitor.x + x as i32;
        let py = self.monitor.y + y as i32;
        let nx = ((px - vx) as i64 * 65535 / (vw - 1).max(1) as i64) as i32;
        let ny = ((py - vy) as i64 * 65535 / (vh - 1).max(1) as i64) as i32;
        send(&[mouse_input(nx, ny, 0, MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK)])
    }

    pub fn rel(&mut self, dx: i32, dy: i32) -> Result<()> {
        send(&[mouse_input(dx, dy, 0, MOUSEEVENTF_MOVE)])
    }

    /// 1/120-notch units, positive up/right (Windows uses the same sign).
    pub fn wheel(&mut self, dx: i32, dy: i32) -> Result<()> {
        let mut v = Vec::new();
        if dy != 0 {
            v.push(mouse_input(0, 0, dy, MOUSEEVENTF_WHEEL));
        }
        if dx != 0 {
            v.push(mouse_input(0, 0, dx, MOUSEEVENTF_HWHEEL));
        }
        if v.is_empty() { Ok(()) } else { send(&v) }
    }

    /// Types text whatever the keyboard layout (KEYEVENTF_UNICODE).
    pub fn text(&mut self, text: &str) -> Result<()> {
        let mut v = Vec::new();
        for u in text.replace("\r\n", "\n").encode_utf16() {
            let u = if u == u16::from(b'\n') { u16::from(b'\r') } else { u };
            v.push(key_input(u, KEYEVENTF_UNICODE));
            v.push(key_input(u, KEYEVENTF_UNICODE | KEYEVENTF_KEYUP));
        }
        send(&v)
    }

    /// Pen sample: position in stream pixels ×16, pressure 0..=65535,
    /// tilt in degrees, protocol pen flags.
    pub fn pen(&mut self, x: u32, y: u32, pressure: u16, tilt: (i8, i8), flags: u8) -> Result<()> {
        if self.pen.is_none() {
            // SAFETY: creating a synthetic pen device has no preconditions.
            let dev = unsafe { CreateSyntheticPointerDevice(PT_PEN, 1, POINTER_FEEDBACK_DEFAULT) }
                .context("CreateSyntheticPointerDevice (needs Windows 10 1809+)")?;
            self.pen = Some(dev);
        }
        let in_range = flags & 1 != 0;
        let contact = flags & 2 != 0;
        let mut pf = if in_range { POINTER_FLAG_INRANGE } else { Default::default() };
        if contact {
            pf |= POINTER_FLAG_INCONTACT;
        }
        pf |= match (self.pen_down, contact) {
            (false, true) => POINTER_FLAG_DOWN,
            (true, false) => POINTER_FLAG_UP,
            _ => POINTER_FLAG_UPDATE,
        };
        self.pen_down = contact;
        let mut pen_flags: u32 = 0;
        if flags & 4 != 0 {
            pen_flags |= PEN_FLAG_ERASER;
        }
        if flags & 8 != 0 {
            pen_flags |= PEN_FLAG_BARREL;
        }
        let pt = POINT { x: self.monitor.x + (x / 16) as i32, y: self.monitor.y + (y / 16) as i32 };
        let info = POINTER_PEN_INFO {
            pointerInfo: POINTER_INFO {
                pointerType: PT_PEN,
                pointerId: 0,
                pointerFlags: pf,
                ptPixelLocation: pt,
                ..Default::default()
            },
            penFlags: pen_flags,
            penMask: PEN_MASK_PRESSURE | PEN_MASK_TILT_X | PEN_MASK_TILT_Y,
            pressure: (pressure as u32 * 1024 / 65535).min(1024),
            tiltX: tilt.0 as i32,
            tiltY: tilt.1 as i32,
            ..Default::default()
        };
        let p = POINTER_TYPE_INFO { r#type: PT_PEN, Anonymous: POINTER_TYPE_INFO_0 { penInfo: info } };
        // SAFETY: the device was created above; `p` is a valid pen record.
        unsafe { InjectSyntheticPointerInput(self.pen.expect("created"), &[p]) }
            .context("InjectSyntheticPointerInput")?;
        let _ = HWND::default();
        Ok(())
    }
}

/// XUSB report (ViGEm's view of an Xbox 360 pad).
#[repr(C)]
#[derive(Default, Clone, Copy)]
struct XusbReport {
    buttons: u16,
    left_trigger: u8,
    right_trigger: u8,
    thumb_lx: i16,
    thumb_ly: i16,
    thumb_rx: i16,
    thumb_ry: i16,
}

type Client = *mut std::ffi::c_void;
type Target = *mut std::ffi::c_void;
/// Rumble callback: client, target, large motor, small motor, LED, user data.
type Notify = unsafe extern "C" fn(Client, Target, u8, u8, u8, *mut std::ffi::c_void);

/// Virtual Xbox 360 pads through ViGEmBus (ViGEmClient.dll).
pub struct Pads {
    lib: libloading::Library,
    client: Client,
    targets: [Option<Target>; 4],
    /// Rumble requests from games: (pad, strong 0–255, weak 0–255).
    pub rumble: Arc<Mutex<Vec<(u8, u8, u8)>>>,
}

// SAFETY: ViGEm client handles are thread-safe per the ViGEm documentation;
// all calls go through `&mut self`.
unsafe impl Send for Pads {}

static RUMBLE: Mutex<Option<Arc<Mutex<Vec<(u8, u8, u8)>>>>> = Mutex::new(None);
static TARGETS: Mutex<Vec<(usize, u8)>> = Mutex::new(Vec::new());

unsafe extern "C" fn on_rumble(_c: Client, t: Target, large: u8, small: u8, _led: u8, _u: *mut std::ffi::c_void) {
    let idx = TARGETS.lock().ok().and_then(|v| v.iter().find(|(p, _)| *p == t as usize).map(|(_, i)| *i));
    if let (Some(idx), Ok(r)) = (idx, RUMBLE.lock())
        && let Some(r) = r.as_ref()
        && let Ok(mut q) = r.lock()
    {
        q.push((idx, large, small));
    }
}

impl Pads {
    pub fn open() -> Result<Self> {
        // SAFETY: loading a DLL by name; its exports are used with the
        // signatures documented by ViGEmClient.
        let lib = unsafe { libloading::Library::new("ViGEmClient.dll") }.context("ViGEmClient.dll not found")?;
        unsafe {
            let alloc: libloading::Symbol<unsafe extern "C" fn() -> Client> = lib.get(b"vigem_alloc")?;
            let connect: libloading::Symbol<unsafe extern "C" fn(Client) -> u32> = lib.get(b"vigem_connect")?;
            let client = alloc();
            let r = connect(client);
            if r != 0x2000_0000 {
                bail!("the ViGEmBus driver isn't running (error {r:#x})");
            }
            let rumble = Arc::new(Mutex::new(Vec::new()));
            *RUMBLE.lock().expect("lock") = Some(rumble.clone());
            Ok(Self { lib, client, targets: [None; 4], rumble })
        }
    }

    pub fn attach(&mut self, idx: u8) -> Result<()> {
        let i = idx as usize;
        if i >= 4 || self.targets[i].is_some() {
            return Ok(());
        }
        // SAFETY: documented ViGEmClient calls on a connected client.
        unsafe {
            let talloc: libloading::Symbol<unsafe extern "C" fn() -> Target> =
                self.lib.get(b"vigem_target_x360_alloc")?;
            let add: libloading::Symbol<unsafe extern "C" fn(Client, Target) -> u32> =
                self.lib.get(b"vigem_target_add")?;
            let reg: libloading::Symbol<unsafe extern "C" fn(Client, Target, Notify, *mut std::ffi::c_void) -> u32> =
                self.lib.get(b"vigem_target_x360_register_notification")?;
            let t = talloc();
            let r = add(self.client, t);
            if r != 0x2000_0000 {
                bail!("adding a virtual pad failed (error {r:#x})");
            }
            reg(self.client, t, on_rumble, std::ptr::null_mut());
            TARGETS.lock().expect("lock").push((t as usize, idx));
            self.targets[i] = Some(t);
        }
        Ok(())
    }

    pub fn detach(&mut self, idx: u8) -> Result<()> {
        let Some(t) = self.targets.get_mut(idx as usize).and_then(Option::take) else { return Ok(()) };
        // SAFETY: `t` was added to this client.
        unsafe {
            let remove: libloading::Symbol<unsafe extern "C" fn(Client, Target) -> u32> =
                self.lib.get(b"vigem_target_remove")?;
            let free: libloading::Symbol<unsafe extern "C" fn(Target)> = self.lib.get(b"vigem_target_free")?;
            remove(self.client, t);
            free(t);
        }
        TARGETS.lock().expect("lock").retain(|(p, _)| *p != t as usize);
        Ok(())
    }

    /// Protocol pad state: buttons bitmask and axes LX LY RX RY LT RT.
    pub fn update(&mut self, idx: u8, buttons: u32, axes: [i16; 6]) -> Result<()> {
        if self.targets.get(idx as usize).is_none_or(Option::is_none) {
            self.attach(idx)?;
        }
        let Some(Some(t)) = self.targets.get(idx as usize).copied() else { return Ok(()) };
        let r = XusbReport {
            buttons: crate::xusb_buttons(buttons),
            left_trigger: (axes[4].max(0) / 128) as u8,
            right_trigger: (axes[5].max(0) / 128) as u8,
            thumb_lx: axes[0],
            // XInput's +Y is up; the protocol's is down.
            thumb_ly: axes[1].saturating_neg(),
            thumb_rx: axes[2],
            thumb_ry: axes[3].saturating_neg(),
        };
        // SAFETY: documented ViGEmClient call; XusbReport matches XUSB_REPORT.
        unsafe {
            let upd: libloading::Symbol<unsafe extern "C" fn(Client, Target, XusbReport) -> u32> =
                self.lib.get(b"vigem_target_x360_update")?;
            upd(self.client, t, r);
        }
        Ok(())
    }
}

impl Drop for Pads {
    fn drop(&mut self) {
        for i in 0..4 {
            let _ = self.detach(i);
        }
        // SAFETY: documented teardown of a connected client.
        unsafe {
            if let Ok(d) = self.lib.get::<unsafe extern "C" fn(Client)>(b"vigem_disconnect") {
                d(self.client);
            }
            if let Ok(f) = self.lib.get::<unsafe extern "C" fn(Client)>(b"vigem_free") {
                f(self.client);
            }
        }
    }
}
