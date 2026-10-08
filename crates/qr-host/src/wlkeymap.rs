//! The keysyms a Wayland desktop's keymap can type, kept current: mutter
//! types a keysym only when its keymap has it, so text input needs to know
//! which characters must go another way. Read as a plain client: the seat's
//! keyboard sends its keymap (and again on every change) with no surface.

use std::collections::HashMap;
use std::io::Read;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::sync::{Arc, RwLock};

use anyhow::{Context, Result, bail};
use wayland_client::protocol::{wl_keyboard, wl_registry, wl_seat};
use wayland_client::{Connection, Dispatch, QueueHandle, WEnum};

/// Each keysym's key (XKB keycode: evdev + 8) and shift level (0 plain,
/// 1 Shift, 2 AltGr, 3 both), the lowest level first layout found.
pub type Keysyms = Arc<RwLock<HashMap<u32, (u32, u32)>>>;

struct State {
    seat: Option<wl_seat::WlSeat>,
    keyboard: Option<wl_keyboard::WlKeyboard>,
    syms: Keysyms,
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
        if let wl_registry::Event::Global { name, interface, version } = ev
            && interface == "wl_seat"
            && st.seat.is_none()
        {
            st.seat = Some(reg.bind(name, version.min(5), qh, ()));
        }
    }
}

impl Dispatch<wl_seat::WlSeat, ()> for State {
    fn event(
        st: &mut Self,
        seat: &wl_seat::WlSeat,
        ev: wl_seat::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let wl_seat::Event::Capabilities { capabilities: WEnum::Value(c) } = ev
            && c.contains(wl_seat::Capability::Keyboard)
            && st.keyboard.is_none()
        {
            st.keyboard = Some(seat.get_keyboard(qh, ()));
        }
    }
}

impl Dispatch<wl_keyboard::WlKeyboard, ()> for State {
    fn event(
        st: &mut Self,
        _: &wl_keyboard::WlKeyboard,
        ev: wl_keyboard::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_keyboard::Event::Keymap { format: WEnum::Value(wl_keyboard::KeymapFormat::XkbV1), fd, size } = ev {
            let mut text = Vec::with_capacity(size as usize);
            let _ = std::fs::File::from(fd).take(size as u64).read_to_end(&mut text);
            match keysyms(&text) {
                Ok(s) => {
                    tracing::debug!(keysyms = s.len(), "desktop keymap");
                    *st.syms.write().unwrap() = s;
                }
                Err(e) => tracing::warn!("reading the desktop keymap: {e:#}"),
            }
        }
    }
}

/// Every keysym of an XKB keymap, with where it is.
fn keysyms(text: &[u8]) -> Result<HashMap<u32, (u32, u32)>> {
    use xkbcommon_dl::{xkb_context_flags, xkb_keymap_compile_flags, xkb_keymap_format};
    let x = xkbcommon_dl::xkbcommon_option().context("libxkbcommon is not installed")?;
    let text = text.split(|&b| b == 0).next().unwrap_or_default();
    let text = std::ffi::CString::new(text)?;
    let mut out: HashMap<u32, (u32, u32)> = HashMap::new();
    // SAFETY: libxkbcommon calls as documented; the keymap and context are
    // released, and the keysym arrays are read within their lengths.
    unsafe {
        let ctx = (x.xkb_context_new)(xkb_context_flags::XKB_CONTEXT_NO_FLAGS);
        if ctx.is_null() {
            bail!("libxkbcommon: no context");
        }
        let km = (x.xkb_keymap_new_from_string)(
            ctx,
            text.as_ptr(),
            xkb_keymap_format::XKB_KEYMAP_FORMAT_TEXT_V1,
            xkb_keymap_compile_flags::XKB_KEYMAP_COMPILE_NO_FLAGS,
        );
        (x.xkb_context_unref)(ctx);
        if km.is_null() {
            bail!("the keymap doesn't compile");
        }
        for kc in (x.xkb_keymap_min_keycode)(km)..=(x.xkb_keymap_max_keycode)(km) {
            for layout in 0..(x.xkb_keymap_num_layouts_for_key)(km, kc) {
                for level in 0..(x.xkb_keymap_num_levels_for_key)(km, kc, layout) {
                    let mut syms = std::ptr::null();
                    let n = (x.xkb_keymap_key_get_syms_by_level)(km, kc, layout, level, &mut syms);
                    if n > 0 && !syms.is_null() {
                        for &sym in std::slice::from_raw_parts(syms, n as usize) {
                            let better = out.get(&sym).is_none_or(|&(_, l)| level < l);
                            if layout == 0 && better || !out.contains_key(&sym) {
                                out.insert(sym, (kc, level));
                            }
                        }
                    }
                }
            }
        }
        (x.xkb_keymap_unref)(km);
    }
    Ok(out)
}

/// Follows the keymap of the desktop at `socket`, from a thread.
pub fn watch(socket: &Path) -> Result<Keysyms> {
    let conn = Connection::from_socket(
        UnixStream::connect(socket).with_context(|| format!("connecting to {}", socket.display()))?,
    )?;
    let mut q = conn.new_event_queue();
    let qh = q.handle();
    let _registry = conn.display().get_registry(&qh, ());
    let syms = Keysyms::default();
    let mut st = State { seat: None, keyboard: None, syms: syms.clone() };
    // Globals, then the seat's capabilities, then the keymap.
    for _ in 0..3 {
        q.roundtrip(&mut st)?;
    }
    if st.syms.read().unwrap().is_empty() {
        bail!("the desktop sent no keymap");
    }
    std::thread::Builder::new().name("qr-keymap".into()).spawn(move || {
        let _conn = conn;
        while q.blocking_dispatch(&mut st).is_ok() {}
    })?;
    Ok(syms)
}

/// The desktop's Wayland socket, from the environment.
pub fn socket() -> Option<std::path::PathBuf> {
    let name = std::env::var_os("WAYLAND_DISPLAY").unwrap_or_else(|| "wayland-0".into());
    let p = Path::new(&name);
    if p.is_absolute() {
        return Some(p.to_path_buf());
    }
    Some(Path::new(&std::env::var_os("XDG_RUNTIME_DIR")?).join(p))
}

#[cfg(test)]
mod tests {
    #[test]
    fn reads_a_keymap() {
        let Ok(text) = crate::wlinput::default_keymap() else { return };
        let syms = super::keysyms(text.as_bytes()).unwrap();
        // US layout: q is evdev 16 (XKB 24), Q its Shift level.
        assert_eq!(syms.get(&('q' as u32)), Some(&(24, 0)));
        assert_eq!(syms.get(&('Q' as u32)), Some(&(24, 1)));
        assert!(!syms.contains_key(&(0x0100_0000 | '漢' as u32)));
    }
}
