//! Windows host input: keyboard and mouse through `SendInput`, pen through
//! `InjectSyntheticPointerInput`, and Xbox 360 gamepads through ViGEmBus
//! (loaded at run time; the user installs the driver once).
//!
//! The scancode table and the evdev mapping are portable and tested on every
//! platform; the injection code builds only on Windows.

/// Linux evdev key code → PC/AT set-1 scancode, `(scancode, extended)`.
/// For the main block evdev codes equal set-1 scancodes; the E0-prefixed
/// keys are listed explicitly.
pub fn scancode(evdev: u16) -> Option<(u16, bool)> {
    let ext = |s: u16| Some((s, true));
    match evdev {
        1..=83 | 86 => Some((evdev, false)),
        87 => Some((0x57, false)),                        // F11
        88 => Some((0x58, false)),                        // F12
        96 => ext(0x1c),                                  // keypad Enter
        97 => ext(0x1d),                                  // right Ctrl
        98 => ext(0x35),                                  // keypad /
        99 => ext(0x37),                                  // Print Screen
        100 => ext(0x38),                                 // right Alt
        102 => ext(0x47),                                 // Home
        103 => ext(0x48),                                 // Up
        104 => ext(0x49),                                 // Page Up
        105 => ext(0x4b),                                 // Left
        106 => ext(0x4d),                                 // Right
        107 => ext(0x4f),                                 // End
        108 => ext(0x50),                                 // Down
        109 => ext(0x51),                                 // Page Down
        110 => ext(0x52),                                 // Insert
        111 => ext(0x53),                                 // Delete
        113 => ext(0x20),                                 // Mute
        114 => ext(0x2e),                                 // Volume down
        115 => ext(0x30),                                 // Volume up
        117 => Some((0x59, false)),                       // keypad =
        121 => Some((0x7e, false)),                       // keypad , (Brazilian)
        122 => Some((0x72, false)),                       // Hangul/Lang1
        123 => Some((0x71, false)),                       // Hanja/Lang2
        124 => Some((0x7d, false)),                       // Yen
        125 => ext(0x5b),                                 // left Windows
        126 => ext(0x5c),                                 // right Windows
        127 => ext(0x5d),                                 // Menu
        163 => ext(0x19),                                 // Next track
        164 => ext(0x22),                                 // Play/Pause
        165 => ext(0x10),                                 // Previous track
        166 => ext(0x24),                                 // Stop
        183..=193 => Some((0x64 + (evdev - 183), false)), // F13–F23
        194 => Some((0x76, false)),                       // F24
        _ => None,
    }
}

/// Protocol pad buttons (`qr_proto::datagram::pad`) → XUSB `wButtons`.
pub fn xusb_buttons(b: u32) -> u16 {
    const MAP: [(u32, u16); 15] = [
        (1 << 0, 0x1000),  // A
        (1 << 1, 0x2000),  // B
        (1 << 2, 0x4000),  // X
        (1 << 3, 0x8000),  // Y
        (1 << 4, 0x0100),  // LB
        (1 << 5, 0x0200),  // RB
        (1 << 6, 0x0020),  // Back
        (1 << 7, 0x0010),  // Start
        (1 << 8, 0x0400),  // Guide
        (1 << 9, 0x0040),  // LS
        (1 << 10, 0x0080), // RS
        (1 << 11, 0x0001), // Up
        (1 << 12, 0x0002), // Down
        (1 << 13, 0x0004), // Left
        (1 << 14, 0x0008), // Right
    ];
    MAP.iter().filter(|(p, _)| b & p != 0).fold(0, |acc, (_, x)| acc | x)
}

#[cfg(windows)]
pub mod inject;
#[cfg(windows)]
pub mod monitors;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scancodes() {
        assert_eq!(scancode(30), Some((0x1e, false))); // A
        assert_eq!(scancode(28), Some((0x1c, false))); // Enter
        assert_eq!(scancode(96), Some((0x1c, true))); // keypad Enter
        assert_eq!(scancode(103), Some((0x48, true))); // Up
        assert_eq!(scancode(193), Some((0x6e, false))); // F23
        assert_eq!(scancode(240), None);
    }

    #[test]
    fn pad_buttons() {
        assert_eq!(xusb_buttons(0b1), 0x1000);
        assert_eq!(xusb_buttons((1 << 11) | (1 << 7)), 0x0011);
    }
}
