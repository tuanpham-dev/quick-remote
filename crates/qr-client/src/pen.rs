//! Windows pen input: winit reports no pressure or tilt, so the window's
//! procedure is subclassed to read `WM_POINTER*` messages for pens
//! (`GetPointerPenInfo`). Samples go to the event loop in window-client
//! pixels; the app maps them into the stream.

use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, WPARAM};
use windows::Win32::Graphics::Gdi::ScreenToClient;
use windows::Win32::UI::Input::Pointer::{
    GetPointerPenInfo, GetPointerType, POINTER_FLAG_INCONTACT, POINTER_FLAG_INRANGE, POINTER_PEN_INFO,
};
use windows::Win32::UI::Shell::{DefSubclassProc, SetWindowSubclass};
use windows::Win32::UI::WindowsAndMessaging::{
    PEN_FLAG_BARREL, PEN_FLAG_ERASER, PEN_FLAG_INVERTED, PT_PEN, WM_POINTERDOWN, WM_POINTERUP, WM_POINTERUPDATE,
};
use winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};

/// One pen sample in window-client pixels.
#[derive(Debug, Clone, Copy)]
pub struct PenSample {
    pub x: f64,
    pub y: f64,
    /// 0–65535.
    pub pressure: u16,
    pub tilt_x: i8,
    pub tilt_y: i8,
    /// 1 in range, 2 contact, 4 eraser, 8 barrel (protocol flags).
    pub flags: u8,
}

type Sink = Box<dyn Fn(PenSample)>;

unsafe extern "system" fn proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM, _id: usize, data: usize) -> LRESULT {
    if matches!(msg, WM_POINTERDOWN | WM_POINTERUPDATE | WM_POINTERUP) {
        let id = (wp.0 & 0xffff) as u32;
        let mut kind = Default::default();
        // SAFETY: plain Win32 queries for the pointer in this message.
        unsafe {
            if GetPointerType(id, &mut kind).is_ok() && kind == PT_PEN {
                let mut info = POINTER_PEN_INFO::default();
                if GetPointerPenInfo(id, &mut info).is_ok() {
                    let mut p: POINT = info.pointerInfo.ptPixelLocation;
                    let _ = ScreenToClient(hwnd, &mut p);
                    let pf = info.pointerInfo.pointerFlags;
                    let mut flags = 0u8;
                    if pf.contains(POINTER_FLAG_INRANGE) {
                        flags |= 1;
                    }
                    if pf.contains(POINTER_FLAG_INCONTACT) {
                        flags |= 2;
                    }
                    if info.penFlags & (PEN_FLAG_ERASER | PEN_FLAG_INVERTED) != 0 {
                        flags |= 4;
                    }
                    if info.penFlags & PEN_FLAG_BARREL != 0 {
                        flags |= 8;
                    }
                    let sink = &*(data as *const Sink);
                    sink(PenSample {
                        x: p.x as f64,
                        y: p.y as f64,
                        // Windows reports 0–1024.
                        pressure: (info.pressure.min(1024) * 64).min(65535) as u16,
                        tilt_x: info.tiltX.clamp(-90, 90) as i8,
                        tilt_y: info.tiltY.clamp(-90, 90) as i8,
                        flags,
                    });
                    return LRESULT(0);
                }
            }
        }
    }
    // SAFETY: the default handling for everything else.
    unsafe { DefSubclassProc(hwnd, msg, wp, lp) }
}

/// Hooks `window`'s pen messages; `sink` runs on the UI thread.
pub fn hook(window: &winit::window::Window, sink: impl Fn(PenSample) + 'static) -> anyhow::Result<()> {
    let RawWindowHandle::Win32(h) = window.window_handle()?.as_raw() else {
        anyhow::bail!("not a Win32 window");
    };
    let hwnd = HWND(h.hwnd.get() as *mut _);
    // The sink lives as long as the window (leaked: one per window).
    let data = Box::into_raw(Box::new(Box::new(sink) as Sink)) as usize;
    // SAFETY: a live window owned by this thread.
    if !unsafe { SetWindowSubclass(hwnd, Some(proc), 0x5152, data) }.as_bool() {
        anyhow::bail!("SetWindowSubclass failed");
    }
    Ok(())
}
