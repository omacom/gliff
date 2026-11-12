//! X keysyms for evdev key codes, as a US keyboard has them: what a VNC
//! server takes when it cannot take the key's position (QEMU's extended key
//! events) instead.

/// Letters, digits and symbols: the keysym unshifted and shifted, which for
/// these is the character itself.
fn printable(code: u32) -> Option<(char, char)> {
    Some(match code {
        2 => ('1', '!'),
        3 => ('2', '@'),
        4 => ('3', '#'),
        5 => ('4', '$'),
        6 => ('5', '%'),
        7 => ('6', '^'),
        8 => ('7', '&'),
        9 => ('8', '*'),
        10 => ('9', '('),
        11 => ('0', ')'),
        12 => ('-', '_'),
        13 => ('=', '+'),
        16 => ('q', 'Q'),
        17 => ('w', 'W'),
        18 => ('e', 'E'),
        19 => ('r', 'R'),
        20 => ('t', 'T'),
        21 => ('y', 'Y'),
        22 => ('u', 'U'),
        23 => ('i', 'I'),
        24 => ('o', 'O'),
        25 => ('p', 'P'),
        26 => ('[', '{'),
        27 => (']', '}'),
        30 => ('a', 'A'),
        31 => ('s', 'S'),
        32 => ('d', 'D'),
        33 => ('f', 'F'),
        34 => ('g', 'G'),
        35 => ('h', 'H'),
        36 => ('j', 'J'),
        37 => ('k', 'K'),
        38 => ('l', 'L'),
        39 => (';', ':'),
        40 => ('\'', '"'),
        41 => ('`', '~'),
        43 => ('\\', '|'),
        44 => ('z', 'Z'),
        45 => ('x', 'X'),
        46 => ('c', 'C'),
        47 => ('v', 'V'),
        48 => ('b', 'B'),
        49 => ('n', 'N'),
        50 => ('m', 'M'),
        51 => (',', '<'),
        52 => ('.', '>'),
        53 => ('/', '?'),
        57 => (' ', ' '),
        86 => ('<', '>'),
        _ => return None,
    })
}

/// Keys that type nothing, and the keypad.
fn function(code: u32) -> Option<u32> {
    Some(match code {
        1 => 0xff1b,                        // Escape
        14 => 0xff08,                       // BackSpace
        15 => 0xff09,                       // Tab
        28 => 0xff0d,                       // Return
        29 => 0xffe3,                       // Control_L
        42 => 0xffe1,                       // Shift_L
        54 => 0xffe2,                       // Shift_R
        55 => 0xffaa,                       // KP_Multiply
        56 => 0xffe9,                       // Alt_L
        58 => 0xffe5,                       // Caps_Lock
        59..=68 => 0xffbe + (code - 59),    // F1..F10
        69 => 0xff7f,                       // Num_Lock
        70 => 0xff14,                       // Scroll_Lock
        71 => 0xffb7,                       // KP_7
        72 => 0xffb8,                       // KP_8
        73 => 0xffb9,                       // KP_9
        74 => 0xffad,                       // KP_Subtract
        75 => 0xffb4,                       // KP_4
        76 => 0xffb5,                       // KP_5
        77 => 0xffb6,                       // KP_6
        78 => 0xffab,                       // KP_Add
        79 => 0xffb1,                       // KP_1
        80 => 0xffb2,                       // KP_2
        81 => 0xffb3,                       // KP_3
        82 => 0xffb0,                       // KP_0
        83 => 0xffae,                       // KP_Decimal
        87 => 0xffc8,                       // F11
        88 => 0xffc9,                       // F12
        96 => 0xff8d,                       // KP_Enter
        97 => 0xffe4,                       // Control_R
        98 => 0xffaf,                       // KP_Divide
        99 => 0xff61,                       // Print
        100 => 0xffea,                      // Alt_R
        102 => 0xff50,                      // Home
        103 => 0xff52,                      // Up
        104 => 0xff55,                      // Prior
        105 => 0xff51,                      // Left
        106 => 0xff53,                      // Right
        107 => 0xff57,                      // End
        108 => 0xff54,                      // Down
        109 => 0xff56,                      // Next
        110 => 0xff63,                      // Insert
        111 => 0xffff,                      // Delete
        113 => 0x1008_ff12,                 // XF86AudioMute
        114 => 0x1008_ff11,                 // XF86AudioLowerVolume
        115 => 0x1008_ff13,                 // XF86AudioRaiseVolume
        119 => 0xff13,                      // Pause
        125 => 0xffeb,                      // Super_L
        126 => 0xffec,                      // Super_R
        127 => 0xff67,                      // Menu
        183..=194 => 0xffca + (code - 183), // F13..F24
        _ => return None,
    })
}

/// The keysym evdev `code` types on a US keyboard, with Shift held or not
/// and Caps Lock on or not.
pub(super) fn keysym(code: u32, shift: bool, caps: bool) -> Option<u32> {
    if let Some((plain, shifted)) = printable(code) {
        let upper = if plain.is_ascii_alphabetic() {
            shift != caps
        } else {
            shift
        };
        return Some(u32::from(if upper { shifted } else { plain }));
    }
    function(code)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn letters_follow_shift_and_caps_lock() {
        assert_eq!(keysym(30, false, false), Some(u32::from('a')));
        assert_eq!(keysym(30, true, false), Some(u32::from('A')));
        assert_eq!(keysym(30, false, true), Some(u32::from('A')));
        assert_eq!(keysym(30, true, true), Some(u32::from('a')));
        assert_eq!(keysym(2, false, true), Some(u32::from('1')));
        assert_eq!(keysym(2, true, false), Some(u32::from('!')));
    }

    #[test]
    fn other_keys_have_their_own_keysyms() {
        assert_eq!(keysym(28, false, false), Some(0xff0d));
        assert_eq!(keysym(59, false, false), Some(0xffbe));
        assert_eq!(keysym(68, false, false), Some(0xffc7));
        assert_eq!(keysym(183, false, false), Some(0xffca));
        assert_eq!(keysym(240, false, false), None);
    }
}
