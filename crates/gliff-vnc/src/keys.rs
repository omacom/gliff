//! Typing what a VNC client sends. RFB key events carry X keysyms (the
//! character or function), not key positions, so the server types them on
//! a keymap of its own where each keysym has a key. Keysyms that are not
//! on it yet take a free key, and the keymap is sent again.

use std::collections::HashMap;

use xkbcommon::xkb;

/// The lowest and highest xkb keycodes; X11 clients cannot see above 255.
const FIRST: u32 = 9;
const LAST: u32 = 255;

/// Modifiers keep fixed keys, so a held modifier never moves.
const MODIFIERS: &[(u32, &str)] = &[
    (0xffe1, "Shift"),   // Shift_L
    (0xffe2, "Shift"),   // Shift_R
    (0xffe3, "Control"), // Control_L
    (0xffe4, "Control"), // Control_R
    (0xffe9, "Mod1"),    // Alt_L
    (0xffea, "Mod1"),    // Alt_R
    (0xffe7, "Mod1"),    // Meta_L
    (0xffe8, "Mod1"),    // Meta_R
    (0xffeb, "Mod4"),    // Super_L
    (0xffec, "Mod4"),    // Super_R
    (0xfe03, "Mod5"),    // ISO_Level3_Shift (AltGr)
];

/// Keysyms put on the keymap from the start, so that typing seldom has to
/// send a new keymap: printable ASCII and the usual function keys.
fn initial() -> impl Iterator<Item = u32> {
    let functions = [
        0xff08, 0xff09, 0xff0d, 0xff1b, 0xffff, // BackSpace Tab Return Escape Delete
        0xff50, 0xff51, 0xff52, 0xff53, 0xff54, // Home Left Up Right Down
        0xff55, 0xff56, 0xff57, 0xff63, // Prior Next End Insert
    ];
    let f_keys = 0xffbe..=0xffc9; // F1..F12
    (0x20..=0x7e).chain(functions).chain(f_keys)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Slot {
    keysym: u32,
    /// When it was last typed, to choose which key to reuse.
    used: u64,
}

/// A press or release to send, with an updated keymap to send first when
/// the keysym had no key.
#[derive(Debug, PartialEq, Eq)]
pub struct Stroke {
    pub keymap: Option<String>,
    /// evdev keycode (xkb keycode minus 8).
    pub code: u32,
    pub pressed: bool,
}

pub struct Keys {
    slots: Vec<Option<Slot>>,
    by_keysym: HashMap<u32, u32>,
    /// Keys held down, by the keysym that pressed them.
    held: HashMap<u32, u32>,
    clock: u64,
}

impl Default for Keys {
    fn default() -> Self {
        Self::new()
    }
}

impl Keys {
    pub fn new() -> Self {
        let mut keys = Self {
            slots: vec![None; (LAST - FIRST + 1) as usize],
            by_keysym: HashMap::new(),
            held: HashMap::new(),
            clock: 0,
        };
        for keysym in MODIFIERS.iter().map(|&(k, _)| k).chain(initial()) {
            keys.place(keysym);
        }
        keys
    }

    /// The keymap to start with.
    pub fn keymap(&self) -> String {
        let mut keycodes = String::new();
        let mut symbols = String::new();
        for (i, slot) in self.slots.iter().enumerate() {
            let code = FIRST + i as u32;
            keycodes.push_str(&format!("    <K{code}> = {code};\n"));
            if let Some(slot) = slot {
                let name = xkb::keysym_get_name(xkb::Keysym::new(slot.keysym));
                symbols.push_str(&format!("    key <K{code}> {{ [ {name} ] }};\n"));
            }
        }
        for (keysym, modifier) in MODIFIERS {
            let code = self.by_keysym[keysym];
            symbols.push_str(&format!("    modifier_map {modifier} {{ <K{code}> }};\n"));
        }
        format!(
            "xkb_keymap {{\n\
             xkb_keycodes \"gliff-vnc\" {{\n    minimum = {FIRST};\n    maximum = {LAST};\n{keycodes}}};\n\
             xkb_types \"gliff-vnc\" {{ include \"complete\" }};\n\
             xkb_compatibility \"gliff-vnc\" {{ include \"complete\" }};\n\
             xkb_symbols \"gliff-vnc\" {{\n{symbols}}};\n\
             }};\n"
        )
    }

    /// What to send for a client's key event.
    pub fn event(&mut self, keysym: u32, pressed: bool) -> Option<Stroke> {
        self.clock += 1;
        if !pressed {
            let code = self.held.remove(&keysym)?;
            return Some(Stroke {
                keymap: None,
                code: code - 8,
                pressed: false,
            });
        }
        if let Some(&code) = self.held.get(&keysym) {
            // Key repeat: the client presses again without a release.
            return Some(Stroke {
                keymap: None,
                code: code - 8,
                pressed: true,
            });
        }
        let (code, changed) = match self.by_keysym.get(&keysym) {
            Some(&code) => (code, false),
            None => (self.place(keysym)?, true),
        };
        if let Some(slot) = &mut self.slots[(code - FIRST) as usize] {
            slot.used = self.clock;
        }
        self.held.insert(keysym, code);
        Some(Stroke {
            keymap: changed.then(|| self.keymap()),
            code: code - 8,
            pressed: true,
        })
    }

    /// Release every held key, as when a client goes away.
    pub fn release_all(&mut self) -> Vec<Stroke> {
        self.held
            .drain()
            .map(|(_, code)| Stroke {
                keymap: None,
                code: code - 8,
                pressed: false,
            })
            .collect()
    }

    /// Give `keysym` a key: a free one, else the least recently typed one
    /// that is not a modifier or held. `None` if every key is held.
    fn place(&mut self, keysym: u32) -> Option<u32> {
        let free = self.slots.iter().position(Option::is_none);
        let index = free.or_else(|| {
            let held: Vec<u32> = self.held.values().copied().collect();
            self.slots
                .iter()
                .enumerate()
                .filter_map(|(i, s)| s.map(|s| (i, s)))
                .filter(|(i, s)| {
                    !MODIFIERS.iter().any(|&(k, _)| k == s.keysym)
                        && !held.contains(&(FIRST + *i as u32))
                })
                .min_by_key(|(_, s)| s.used)
                .map(|(i, _)| i)
        })?;
        if let Some(old) = self.slots[index] {
            self.by_keysym.remove(&old.keysym);
        }
        let code = FIRST + index as u32;
        self.slots[index] = Some(Slot {
            keysym,
            used: self.clock,
        });
        self.by_keysym.insert(keysym, code);
        Some(code)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn compile(text: &str) -> xkb::Keymap {
        let ctx = xkb::Context::new(xkb::CONTEXT_NO_FLAGS);
        xkb::Keymap::new_from_string(
            &ctx,
            text.to_string(),
            xkb::KEYMAP_FORMAT_TEXT_V1,
            xkb::KEYMAP_COMPILE_NO_FLAGS,
        )
        .expect("the keymap compiles")
    }

    fn typed(keymap: &xkb::Keymap, evdev: u32) -> u32 {
        let state = xkb::State::new(keymap);
        state.key_get_one_sym(xkb::Keycode::new(evdev + 8)).raw()
    }

    #[test]
    fn the_keymap_compiles_and_types_what_was_sent() {
        let mut keys = Keys::new();
        let keymap = compile(&keys.keymap());
        for keysym in [0x61u32, 0x41, 0x7e, 0xff0d, 0xffbe] {
            let s = keys.event(keysym, true).unwrap();
            assert!(s.keymap.is_none(), "{keysym:#x} was on the first keymap");
            assert_eq!(typed(&keymap, s.code), keysym);
            assert!(!keys.event(keysym, false).unwrap().pressed);
        }
    }

    #[test]
    fn modifiers_act_as_modifiers() {
        let keys = Keys::new();
        let keymap = compile(&keys.keymap());
        let mut state = xkb::State::new(&keymap);
        let shift = keys.by_keysym[&0xffe1];
        state.update_key(xkb::Keycode::new(shift), xkb::KeyDirection::Down);
        assert!(state.mod_name_is_active(xkb::MOD_NAME_SHIFT, xkb::STATE_MODS_EFFECTIVE));
        let ctrl = keys.by_keysym[&0xffe3];
        state.update_key(xkb::Keycode::new(ctrl), xkb::KeyDirection::Down);
        assert!(state.mod_name_is_active(xkb::MOD_NAME_CTRL, xkb::STATE_MODS_EFFECTIVE));
    }

    #[test]
    fn a_new_keysym_gets_a_key_and_a_new_keymap() {
        let mut keys = Keys::new();
        // ñ, then a Unicode keysym past Latin-1 (€).
        for keysym in [0xf1u32, 0x0100_20ac] {
            let s = keys.event(keysym, true).unwrap();
            let keymap = compile(s.keymap.as_deref().expect("a new keymap"));
            assert_eq!(typed(&keymap, s.code), keysym);
            keys.event(keysym, false);
            // Typed again, it keeps its key.
            assert!(keys.event(keysym, true).unwrap().keymap.is_none());
            keys.event(keysym, false);
        }
    }

    #[test]
    fn full_keymaps_reuse_the_least_recent_key_but_never_a_held_one() {
        let mut keys = Keys::new();
        let held = keys.event(0x61, true).unwrap().code;
        // Far more distinct keysyms than keys.
        for cp in 0x400u32..0x600 {
            let keysym = 0x0100_0000 | cp;
            let s = keys.event(keysym, true).unwrap();
            assert_ne!(s.code, held, "a held key was taken");
            keys.event(keysym, false);
        }
        let keymap = compile(&keys.keymap());
        assert_eq!(typed(&keymap, held), 0x61);
        assert_eq!(keys.event(0x61, false).unwrap().code, held);
        // Modifiers never moved.
        assert!(keys.by_keysym.contains_key(&0xffe1));
    }

    #[test]
    fn a_release_without_a_press_is_ignored_and_repeats_reuse_the_key() {
        let mut keys = Keys::new();
        assert!(keys.event(0x62, false).is_none());
        let a = keys.event(0x62, true).unwrap();
        let b = keys.event(0x62, true).unwrap();
        assert_eq!(a.code, b.code);
        assert_eq!(keys.release_all().len(), 1);
        assert!(keys.event(0x62, false).is_none());
    }
}
