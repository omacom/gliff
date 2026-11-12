//! Key codes: gliff sends evdev codes, the position of a key on the
//! keyboard whatever its layout. RDP, and RustDesk driving Windows, take
//! the PC scan code of that position; RustDesk driving a Mac takes the Mac
//! virtual key code, and driving Linux the X key code.

/// The PC (set 1) scan code for evdev `code`, `0xE0xx` for an extended key,
/// as both RDP and Windows take it. `None` for a key with no scan code.
pub fn scancode(code: u32) -> Option<u16> {
    // Up to F12 the evdev codes were taken from the scan codes, except for
    // the gaps.
    if (1..=83).contains(&code) || (86..=88).contains(&code) {
        return Some(code as u16);
    }
    let ext = |c: u16| Some(0xE000 | c);
    match code {
        89 => Some(0x73),                              // RO
        92 => Some(0x79),                              // HENKAN
        93 => Some(0x70),                              // KATAKANAHIRAGANA
        94 => Some(0x7B),                              // MUHENKAN
        96 => ext(0x1C),                               // KPENTER
        97 => ext(0x1D),                               // RIGHTCTRL
        98 => ext(0x35),                               // KPSLASH
        99 => ext(0x37),                               // SYSRQ (Print Screen)
        100 => ext(0x38),                              // RIGHTALT
        102 => ext(0x47),                              // HOME
        103 => ext(0x48),                              // UP
        104 => ext(0x49),                              // PAGEUP
        105 => ext(0x4B),                              // LEFT
        106 => ext(0x4D),                              // RIGHT
        107 => ext(0x4F),                              // END
        108 => ext(0x50),                              // DOWN
        109 => ext(0x51),                              // PAGEDOWN
        110 => ext(0x52),                              // INSERT
        111 => ext(0x53),                              // DELETE
        113 => ext(0x20),                              // MUTE
        114 => ext(0x2E),                              // VOLUMEDOWN
        115 => ext(0x30),                              // VOLUMEUP
        116 => ext(0x5E),                              // POWER
        117 => Some(0x59),                             // KPEQUAL
        121 => Some(0x7E),                             // KPCOMMA
        122 => Some(0x72),                             // HANGEUL
        123 => Some(0x71),                             // HANJA
        124 => Some(0x7D),                             // YEN
        125 => ext(0x5B),                              // LEFTMETA
        126 => ext(0x5C),                              // RIGHTMETA
        127 => ext(0x5D),                              // COMPOSE (Menu)
        140 => ext(0x21),                              // CALC
        142 => ext(0x5F),                              // SLEEP
        143 => ext(0x63),                              // WAKEUP
        155 => ext(0x6C),                              // MAIL
        158 => ext(0x6A),                              // BACK
        159 => ext(0x69),                              // FORWARD
        163 => ext(0x19),                              // NEXTSONG
        164 => ext(0x22),                              // PLAYPAUSE
        165 => ext(0x10),                              // PREVIOUSSONG
        166 => ext(0x24),                              // STOPCD
        172 => ext(0x32),                              // HOMEPAGE
        183..=193 => Some(0x64 + (code - 183) as u16), // F13..F23
        194 => Some(0x76),                             // F24
        217 => ext(0x65),                              // SEARCH
        _ => None,
    }
}

/// The Mac virtual key code for evdev `code`, as an ANSI keyboard has it.
pub fn mac_keycode(code: u32) -> Option<u16> {
    let k = match code {
        1 => 0x35,   // ESC
        2 => 0x12,   // 1
        3 => 0x13,   // 2
        4 => 0x14,   // 3
        5 => 0x15,   // 4
        6 => 0x17,   // 5
        7 => 0x16,   // 6
        8 => 0x1A,   // 7
        9 => 0x1C,   // 8
        10 => 0x19,  // 9
        11 => 0x1D,  // 0
        12 => 0x1B,  // MINUS
        13 => 0x18,  // EQUAL
        14 => 0x33,  // BACKSPACE
        15 => 0x30,  // TAB
        16 => 0x0C,  // Q
        17 => 0x0D,  // W
        18 => 0x0E,  // E
        19 => 0x0F,  // R
        20 => 0x11,  // T
        21 => 0x10,  // Y
        22 => 0x20,  // U
        23 => 0x22,  // I
        24 => 0x1F,  // O
        25 => 0x23,  // P
        26 => 0x21,  // LEFTBRACE
        27 => 0x1E,  // RIGHTBRACE
        28 => 0x24,  // ENTER
        29 => 0x3B,  // LEFTCTRL
        30 => 0x00,  // A
        31 => 0x01,  // S
        32 => 0x02,  // D
        33 => 0x03,  // F
        34 => 0x05,  // G
        35 => 0x04,  // H
        36 => 0x26,  // J
        37 => 0x28,  // K
        38 => 0x25,  // L
        39 => 0x29,  // SEMICOLON
        40 => 0x27,  // APOSTROPHE
        41 => 0x32,  // GRAVE
        42 => 0x38,  // LEFTSHIFT
        43 => 0x2A,  // BACKSLASH
        44 => 0x06,  // Z
        45 => 0x07,  // X
        46 => 0x08,  // C
        47 => 0x09,  // V
        48 => 0x0B,  // B
        49 => 0x2D,  // N
        50 => 0x2E,  // M
        51 => 0x2B,  // COMMA
        52 => 0x2F,  // DOT
        53 => 0x2C,  // SLASH
        54 => 0x3C,  // RIGHTSHIFT
        55 => 0x43,  // KPASTERISK
        56 => 0x3A,  // LEFTALT (Option)
        57 => 0x31,  // SPACE
        58 => 0x39,  // CAPSLOCK
        59 => 0x7A,  // F1
        60 => 0x78,  // F2
        61 => 0x63,  // F3
        62 => 0x76,  // F4
        63 => 0x60,  // F5
        64 => 0x61,  // F6
        65 => 0x62,  // F7
        66 => 0x64,  // F8
        67 => 0x65,  // F9
        68 => 0x6D,  // F10
        69 => 0x47,  // NUMLOCK (Clear)
        71 => 0x59,  // KP7
        72 => 0x5B,  // KP8
        73 => 0x5C,  // KP9
        74 => 0x4E,  // KPMINUS
        75 => 0x56,  // KP4
        76 => 0x57,  // KP5
        77 => 0x58,  // KP6
        78 => 0x45,  // KPPLUS
        79 => 0x53,  // KP1
        80 => 0x54,  // KP2
        81 => 0x55,  // KP3
        82 => 0x52,  // KP0
        83 => 0x41,  // KPDOT
        86 => 0x0A,  // 102ND (ISO section)
        87 => 0x67,  // F11
        88 => 0x6F,  // F12
        89 => 0x5E,  // RO (JIS underscore)
        96 => 0x4C,  // KPENTER
        97 => 0x3E,  // RIGHTCTRL
        98 => 0x4B,  // KPSLASH
        100 => 0x3D, // RIGHTALT (right Option)
        102 => 0x73, // HOME
        103 => 0x7E, // UP
        104 => 0x74, // PAGEUP
        105 => 0x7B, // LEFT
        106 => 0x7C, // RIGHT
        107 => 0x77, // END
        108 => 0x7D, // DOWN
        109 => 0x79, // PAGEDOWN
        110 => 0x72, // INSERT (Help)
        111 => 0x75, // DELETE (forward delete)
        113 => 0x4A, // MUTE
        114 => 0x49, // VOLUMEDOWN
        115 => 0x48, // VOLUMEUP
        117 => 0x51, // KPEQUAL
        121 => 0x5F, // KPCOMMA (JIS)
        124 => 0x5D, // YEN
        125 => 0x37, // LEFTMETA (Command)
        126 => 0x36, // RIGHTMETA (right Command)
        183 => 0x69, // F13
        184 => 0x6B, // F14
        185 => 0x71, // F15
        186 => 0x6A, // F16
        187 => 0x40, // F17
        188 => 0x4F, // F18
        189 => 0x50, // F19
        190 => 0x5A, // F20
        _ => return None,
    };
    Some(k)
}

/// The X key code for evdev `code`, as Linux machines take it.
pub fn x_keycode(code: u32) -> Option<u32> {
    (code > 0 && code < 248).then_some(code + 8)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scancodes_follow_the_pc_keyboard() {
        assert_eq!(scancode(1), Some(0x01)); // Esc
        assert_eq!(scancode(30), Some(0x1E)); // A
        assert_eq!(scancode(88), Some(0x58)); // F12
        assert_eq!(scancode(97), Some(0xE01D)); // right Ctrl
        assert_eq!(scancode(125), Some(0xE05B)); // left Super
        assert_eq!(scancode(103), Some(0xE048)); // Up
        assert_eq!(scancode(194), Some(0x76)); // F24
        assert_eq!(scancode(0), None);
        assert_eq!(scancode(84), None);
        assert_eq!(scancode(119), None); // Pause has none
    }

    #[test]
    fn mac_codes_follow_the_ansi_keyboard() {
        assert_eq!(mac_keycode(30), Some(0x00)); // A
        assert_eq!(mac_keycode(125), Some(0x37)); // Command
        assert_eq!(mac_keycode(28), Some(0x24)); // Return
        assert_eq!(mac_keycode(111), Some(0x75)); // forward delete
        assert_eq!(mac_keycode(0), None);
    }

    #[test]
    fn every_scancode_is_distinct() {
        let mut seen = std::collections::BTreeMap::new();
        for code in 0..256 {
            if let Some(s) = scancode(code) {
                assert!(seen.insert(s, code).is_none(), "{code} repeats {s:#x}");
            }
        }
        let mut seen = std::collections::BTreeMap::new();
        for code in 0..256 {
            if let Some(m) = mac_keycode(code) {
                assert!(seen.insert(m, code).is_none(), "{code} repeats {m:#x}");
            }
        }
    }

    #[test]
    fn x_codes_are_evdev_plus_eight() {
        assert_eq!(x_keycode(30), Some(38));
        assert_eq!(x_keycode(0), None);
    }
}
