//! Physical Linux key codes (evdev) mapped to the Windows virtual-key numbers that
//! settings.json stores, so one shortcut file works on both systems. Letters and
//! punctuation follow US key positions, as scancodes do.
use crate::hotkey::*;

const LETTERS: [(u16, &[u8]); 3] = [(16, b"QWERTYUIOP"), (30, b"ASDFGHJKL"), (44, b"ZXCVBNM")];
const TABLE: &[(u16, u8)] = &[
    (1, ESCAPE),
    (11, b'0'),
    (12, 0xbd),
    (13, 0xbb),
    (14, 0x08),
    (15, 0x09),
    (26, 0xdb),
    (27, 0xdd),
    (28, 0x0d),
    (29, LCTRL),
    (39, 0xba),
    (40, 0xde),
    (41, 0xc0),
    (42, LSHIFT),
    (43, 0xdc),
    (51, 0xbc),
    (52, 0xbe),
    (53, 0xbf),
    (54, RSHIFT),
    (55, 0x6a),
    (56, LALT),
    (57, SPACE),
    (58, 0x14),
    (69, 0x90),
    (70, 0x91),
    (71, 0x67),
    (72, 0x68),
    (73, 0x69),
    (74, 0x6d),
    (75, 0x64),
    (76, 0x65),
    (77, 0x66),
    (78, 0x6b),
    (79, 0x61),
    (80, 0x62),
    (81, 0x63),
    (82, 0x60),
    (83, 0x6e),
    (86, 0xe2),
    (87, 0x7a),
    (88, 0x7b),
    (97, RCTRL),
    (98, 0x6f),
    (99, 0x2c),
    (100, RALT),
    (102, 0x24),
    (103, 0x26),
    (104, 0x21),
    (105, 0x25),
    (106, 0x27),
    (107, 0x23),
    (108, 0x28),
    (109, 0x22),
    (110, 0x2d),
    (111, 0x2e),
    (113, 0xad),
    (114, 0xae),
    (115, 0xaf),
    (119, 0x13),
    (125, LWIN),
    (126, RWIN),
    (127, 0x5d),
    (163, 0xb0),
    (164, 0xb3),
    (165, 0xb1),
    (166, 0xb2),
];

/// Virtual key for an evdev code; X11 key codes are evdev codes plus 8.
pub fn vk(code: u16) -> Option<u8> {
    match code {
        2..=10 => return Some(b'1' + (code - 2) as u8),
        59..=68 => return Some(0x70 + (code - 59) as u8),
        183..=194 => return Some(0x7c + (code - 183) as u8),
        _ => {}
    }
    for (start, row) in LETTERS {
        if let Some(&letter) = code
            .checked_sub(start)
            .and_then(|i| row.get(usize::from(i)))
        {
            return Some(letter);
        }
    }
    TABLE.iter().find(|&&(c, _)| c == code).map(|&(_, v)| v)
}
/// The first evdev code producing `vk`; the keypad Enter maps back to the main one.
pub fn evdev(vk_code: u8) -> Option<u16> {
    (1..=194).find(|&code| vk(code) == Some(vk_code))
}

/// An XDG shortcuts-spec trigger such as `CTRL+LOGO+d`, or `None` for chords the
/// spec can't express (modifier-only shortcuts like the default Ctrl + Win).
pub fn trigger(hotkey: &Hotkey) -> Option<String> {
    let main = hotkey.main_key()?;
    let mut parts: Vec<&str> = Vec::new();
    for &key in hotkey.keys() {
        let name = match key {
            LCTRL | RCTRL => "CTRL",
            LALT | RALT => "ALT",
            LSHIFT | RSHIFT => "SHIFT",
            LWIN | RWIN => "LOGO",
            _ => continue,
        };
        if !parts.contains(&name) {
            parts.push(name);
        }
    }
    let key = keysym_name(main)?;
    Some(
        parts
            .into_iter()
            .chain([key.as_str()])
            .collect::<Vec<_>>()
            .join("+"),
    )
}
fn keysym_name(key: u8) -> Option<String> {
    Some(match key {
        b'A'..=b'Z' => char::from(key.to_ascii_lowercase()).to_string(),
        b'0'..=b'9' => char::from(key).to_string(),
        0x70..=0x87 => format!("F{}", key - 0x70 + 1),
        0x60..=0x69 => format!("KP_{}", key - 0x60),
        _ => match key {
            0x08 => "BackSpace",
            0x09 => "Tab",
            0x0d => "Return",
            0x13 => "Pause",
            0x21 => "Prior",
            0x22 => "Next",
            0x23 => "End",
            0x24 => "Home",
            0x25 => "Left",
            0x26 => "Up",
            0x27 => "Right",
            0x28 => "Down",
            0x2c => "Print",
            0x2d => "Insert",
            0x2e => "Delete",
            0xba => "semicolon",
            0xbb => "equal",
            0xbc => "comma",
            0xbd => "minus",
            0xbe => "period",
            0xbf => "slash",
            0xc0 => "grave",
            0xdb => "bracketleft",
            0xdc => "backslash",
            0xdd => "bracketright",
            0xde => "apostrophe",
            _ => return None,
        }
        .to_owned(),
    })
}

#[cfg(test)]
mod tests {
    // Failure modes: rows shift by one; X11's +8 offset is applied twice;
    // modifier sides collapse; reverse lookup disagrees with the forward table;
    // a modifier-only chord is sent to a portal that can't bind it.
    use super::*;

    #[test]
    fn evdev_codes_map_to_settings_keys() {
        for (code, key) in [
            (1, ESCAPE),
            (2, b'1'),
            (11, b'0'),
            (16, b'Q'),
            (25, b'P'),
            (30, b'A'),
            (32, b'D'),
            (50, b'M'),
            (57, SPACE),
            (59, 0x70),
            (88, 0x7b),
            (183, 0x7c),
            (29, LCTRL),
            (97, RCTRL),
            (125, LWIN),
            (126, RWIN),
            (100, RALT),
        ] {
            assert_eq!(vk(code), Some(key), "evdev {code}");
            assert_eq!(evdev(key), Some(code), "vk {key:#x}");
        }
        assert_eq!(vk(0), None);
        assert_eq!(vk(240), None);
    }

    #[test]
    fn portal_triggers_follow_the_shortcuts_spec() {
        let parse = |s: &str| s.parse::<Hotkey>().expect(s);
        assert_eq!(
            trigger(&parse("Ctrl+Shift+D")).as_deref(),
            Some("CTRL+SHIFT+d")
        );
        assert_eq!(
            trigger(&parse("Win+Alt+F9")).as_deref(),
            Some("LOGO+ALT+F9")
        );
        assert_eq!(trigger(&parse("F8")).as_deref(), Some("F8"));
        assert_eq!(trigger(&parse("Ctrl+Win")), None);
        assert_eq!(trigger(&parse("RightCtrl")), None);
    }
}
