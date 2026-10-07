//! The desktop's monitors (`EnumDisplayMonitors`), positioned relative to
//! the virtual screen's top-left corner so every coordinate is
//! non-negative (monitors left of or above the primary have negative
//! desktop coordinates).

use windows::Win32::Foundation::{LPARAM, RECT, TRUE};
use windows::Win32::Graphics::Gdi::{EnumDisplayMonitors, GetMonitorInfoW, HDC, HMONITOR, MONITORINFOEXW};
use windows::Win32::UI::WindowsAndMessaging::{
    GetSystemMetrics, MONITORINFOF_PRIMARY, SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN,
};
use windows::core::BOOL;

#[derive(Debug, Clone)]
pub struct MonitorInfo {
    /// The GDI device name (`\\.\DISPLAY1`).
    pub name: String,
    /// HMONITOR, for `d3d11screencapturesrc monitor-handle`.
    pub handle: u64,
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
    pub primary: bool,
}

/// The virtual screen's top-left corner in desktop coordinates.
pub fn virtual_origin() -> (i32, i32) {
    // SAFETY: no preconditions.
    unsafe { (GetSystemMetrics(SM_XVIRTUALSCREEN), GetSystemMetrics(SM_YVIRTUALSCREEN)) }
}

unsafe extern "system" fn collect(m: HMONITOR, _dc: HDC, _r: *mut RECT, out: LPARAM) -> BOOL {
    // SAFETY: `out` is the Vec passed by `monitors`, alive for the call.
    let out = unsafe { &mut *(out.0 as *mut Vec<MonitorInfo>) };
    let mut info = MONITORINFOEXW::default();
    info.monitorInfo.cbSize = std::mem::size_of::<MONITORINFOEXW>() as u32;
    // SAFETY: a correctly sized MONITORINFOEXW.
    if unsafe { GetMonitorInfoW(m, &mut info.monitorInfo as *mut _ as *mut _) }.as_bool() {
        let r = info.monitorInfo.rcMonitor;
        let n = info.szDevice.iter().position(|&c| c == 0).unwrap_or(info.szDevice.len());
        let (vx, vy) = virtual_origin();
        out.push(MonitorInfo {
            name: String::from_utf16_lossy(&info.szDevice[..n]),
            handle: m.0 as u64,
            x: r.left - vx,
            y: r.top - vy,
            width: r.right - r.left,
            height: r.bottom - r.top,
            primary: info.monitorInfo.dwFlags & MONITORINFOF_PRIMARY != 0,
        });
    }
    TRUE
}

/// The monitors, primary first, then left to right.
pub fn monitors() -> Vec<MonitorInfo> {
    let mut out: Vec<MonitorInfo> = Vec::new();
    // SAFETY: the callback only touches `out` during the call.
    unsafe {
        let _ = EnumDisplayMonitors(None, None, Some(collect), LPARAM(&mut out as *mut _ as isize));
    }
    out.sort_by_key(|m| (!m.primary, m.x, m.y));
    out
}
