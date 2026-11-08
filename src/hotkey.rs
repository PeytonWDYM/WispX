//! Shortcut values use Windows virtual-key numbers without depending on Win32.
use serde::{Deserialize, Deserializer, Serialize, Serializer, de::Error};
use std::{collections::BTreeSet, fmt, str::FromStr};

pub const LCTRL: u8 = 0xa2;
pub const RCTRL: u8 = 0xa3;
pub const LWIN: u8 = 0x5b;
pub const RWIN: u8 = 0x5c;
pub const LALT: u8 = 0xa4;
pub const RALT: u8 = 0xa5;
pub const LSHIFT: u8 = 0xa0;
pub const RSHIFT: u8 = 0xa1;
pub const ESCAPE: u8 = 0x1b;
pub const SPACE: u8 = 0x20;
const MODIFIERS: [u8; 8] = [LCTRL, RCTRL, LWIN, RWIN, LALT, RALT, LSHIFT, RSHIFT];

/// Canonically ordered modifiers and at most one main key, saved as readable JSON.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Hotkey {
    keys: Vec<u8>,
    modifiers: Vec<u8>,
    main: Option<u8>,
}

#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct HotkeyError(&'static str);

impl Default for Hotkey {
    fn default() -> Self {
        Self {
            keys: vec![LCTRL, LWIN],
            modifiers: vec![LCTRL, LWIN],
            main: None,
        }
    }
}

/// Converts generic hook VKs to the left-side variant, like the C# hook.
pub fn normalize(key: u8) -> u8 {
    match key {
        0x10 => LSHIFT,
        0x11 => LCTRL,
        0x12 => LALT,
        _ => key,
    }
}
pub fn is_modifier(key: u8) -> bool {
    MODIFIERS.contains(&key)
}
fn group(key: u8) -> Option<usize> {
    MODIFIERS.iter().position(|&k| k == key).map(|i| i / 2)
}

impl Hotkey {
    /// Constructs even unsupported chords so capture can display its validation error.
    pub fn new(keys: impl IntoIterator<Item = u8>) -> Result<Self, HotkeyError> {
        let keys: BTreeSet<_> = keys.into_iter().collect();
        let main: Vec<_> = keys.iter().copied().filter(|&k| !is_modifier(k)).collect();
        if keys.is_empty() || keys.contains(&0) || main.len() > 1 {
            return Err(HotkeyError("Use modifiers and at most one main key."));
        }
        let mut modifiers: Vec<_> = keys.iter().copied().filter(|&k| is_modifier(k)).collect();
        modifiers.sort_by_key(|&k| (group(k), k));
        let sided = main.is_empty() && modifiers.len() == 1;
        if !sided {
            modifiers = modifiers
                .into_iter()
                .map(|k| MODIFIERS[group(k).expect("known modifier") * 2])
                .collect();
            modifiers.dedup();
        }
        Ok(Self {
            keys: modifiers.iter().chain(&main).copied().collect(),
            modifiers,
            main: main.first().copied(),
        })
    }
    pub fn keys(&self) -> &[u8] {
        &self.keys
    }
    pub fn main_key(&self) -> Option<u8> {
        self.main
    }
    fn sided(&self) -> bool {
        self.main.is_none() && self.modifiers.len() == 1
    }
    pub fn is_valid(&self) -> bool {
        match self.main {
            Some(k) => {
                ![ESCAPE, SPACE].contains(&k)
                    && (!self.modifiers.is_empty() || (0x70..=0x87).contains(&k))
            }
            None => {
                self.modifiers.len() >= 2
                    || self.modifiers.len() == 1 && [RCTRL, RALT].contains(&self.modifiers[0])
            }
        }
    }
    pub fn matches(&self, held: &BTreeSet<u8>) -> bool {
        if self.sided() {
            held.contains(&self.modifiers[0])
        } else {
            self.modifiers
                .iter()
                .all(|&k| held.iter().any(|&h| group(h) == group(k)))
        }
    }
    pub fn has_extra_modifiers(&self, held: &BTreeSet<u8>) -> bool {
        held.iter().any(|&h| {
            if self.sided() {
                h != self.modifiers[0]
            } else {
                !self.modifiers.iter().any(|&k| group(h) == group(k))
            }
        })
    }
    pub fn masks_menu(&self) -> bool {
        self.modifiers
            .iter()
            .any(|&k| matches!(group(k), Some(1 | 2)))
    }
}

// The stable System.Windows.Forms.Keys names used by the existing settings file.
const NAMES: &[(u8, &str)] = &[
    (1, "LButton"),
    (2, "RButton"),
    (3, "Cancel"),
    (4, "MButton"),
    (5, "XButton1"),
    (6, "XButton2"),
    (8, "Back"),
    (9, "Tab"),
    (10, "LineFeed"),
    (12, "Clear"),
    (13, "Enter"),
    (16, "ShiftKey"),
    (17, "ControlKey"),
    (18, "Menu"),
    (19, "Pause"),
    (20, "Capital"),
    (21, "KanaMode"),
    (23, "JunjaMode"),
    (24, "FinalMode"),
    (25, "KanjiMode"),
    (27, "Escape"),
    (28, "IMEConvert"),
    (29, "IMENonconvert"),
    (30, "IMEAceept"),
    (31, "IMEModeChange"),
    (32, "Space"),
    (33, "PageUp"),
    (34, "Next"),
    (35, "End"),
    (36, "Home"),
    (37, "Left"),
    (38, "Up"),
    (39, "Right"),
    (40, "Down"),
    (41, "Select"),
    (42, "Print"),
    (43, "Execute"),
    (44, "PrintScreen"),
    (45, "Insert"),
    (46, "Delete"),
    (47, "Help"),
    (93, "Apps"),
    (95, "Sleep"),
    (106, "Multiply"),
    (107, "Add"),
    (108, "Separator"),
    (109, "Subtract"),
    (110, "Decimal"),
    (111, "Divide"),
    (144, "NumLock"),
    (145, "Scroll"),
    (166, "BrowserBack"),
    (167, "BrowserForward"),
    (168, "BrowserRefresh"),
    (169, "BrowserStop"),
    (170, "BrowserSearch"),
    (171, "BrowserFavorites"),
    (172, "BrowserHome"),
    (173, "VolumeMute"),
    (174, "VolumeDown"),
    (175, "VolumeUp"),
    (176, "MediaNextTrack"),
    (177, "MediaPreviousTrack"),
    (178, "MediaStop"),
    (179, "MediaPlayPause"),
    (180, "LaunchMail"),
    (181, "SelectMedia"),
    (182, "LaunchApplication1"),
    (183, "LaunchApplication2"),
    (186, "OemSemicolon"),
    (187, "Oemplus"),
    (188, "Oemcomma"),
    (189, "OemMinus"),
    (190, "OemPeriod"),
    (191, "Oem2"),
    (192, "Oemtilde"),
    (219, "Oem4"),
    (220, "OemPipe"),
    (221, "Oem6"),
    (222, "OemQuotes"),
    (223, "Oem8"),
    (226, "Oem102"),
    (229, "ProcessKey"),
    (231, "Packet"),
    (246, "Attn"),
    (247, "Crsel"),
    (248, "Exsel"),
    (249, "EraseEof"),
    (250, "Play"),
    (251, "Zoom"),
    (252, "NoName"),
    (253, "Pa1"),
    (254, "OemClear"),
];

fn key_name(key: u8) -> String {
    if key.is_ascii_digit() || key.is_ascii_uppercase() {
        return char::from(key).to_string();
    }
    if (0x70..=0x87).contains(&key) {
        return format!("F{}", key - 0x70 + 1);
    }
    if (0x60..=0x69).contains(&key) {
        return format!("NumPad{}", key - 0x60);
    }
    NAMES
        .iter()
        .find(|&&(k, _)| k == key)
        .map_or_else(|| key.to_string(), |&(_, name)| name.to_owned())
}

impl fmt::Display for Hotkey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (i, &key) in self.keys.iter().enumerate() {
            if i > 0 {
                f.write_str(" + ")?;
            }
            if let Some(g) = group(key) {
                if self.sided() && [RCTRL, RWIN, RALT, RSHIFT].contains(&key) {
                    f.write_str("Right ")?;
                }
                f.write_str(["Ctrl", "Win", "Alt", "Shift"][g])?;
            } else {
                f.write_str(&key_name(key))?;
            }
        }
        Ok(())
    }
}
impl FromStr for Hotkey {
    type Err = HotkeyError;
    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let keys = value
            .split('+')
            .map(|s| {
                let name = s.replace(' ', "");
                let key = match name.as_str() {
                    "Ctrl" | "LControlKey" => LCTRL,
                    "RightCtrl" | "RControlKey" => RCTRL,
                    "Win" | "LWin" => LWIN,
                    "RightWin" | "RWin" => RWIN,
                    "Alt" | "LMenu" => LALT,
                    "RightAlt" | "RMenu" => RALT,
                    "Shift" | "LShiftKey" => LSHIFT,
                    "RightShift" | "RShiftKey" => RSHIFT,
                    "Return" => 13,
                    "CapsLock" => 20,
                    "Prior" => 33,
                    "PageDown" => 34,
                    "Snapshot" => 44,
                    "HanguelMode" | "HangulMode" => 21,
                    "HanjaMode" => 25,
                    "IMEAccept" => 30,
                    "Oem1" => 186,
                    "OemQuestion" => 191,
                    "Oem3" => 192,
                    "OemOpenBrackets" => 219,
                    "Oem5" => 220,
                    "OemCloseBrackets" => 221,
                    "Oem7" => 222,
                    "OemBackslash" => 226,
                    _ if name.len() == 1
                        && (name.as_bytes()[0].is_ascii_digit()
                            || name.as_bytes()[0].is_ascii_uppercase()) =>
                    {
                        name.as_bytes()[0]
                    }
                    _ => {
                        if let Some(k) = NAMES.iter().find(|&&(_, n)| n == name).map(|&(k, _)| k) {
                            k
                        } else if let Some(n) = name
                            .strip_prefix('D')
                            .and_then(|n| n.parse::<u8>().ok())
                            .filter(|&n| n < 10)
                        {
                            b'0' + n
                        } else if let Some(n) = name
                            .strip_prefix('F')
                            .and_then(|n| n.parse::<u8>().ok())
                            .filter(|n| (1..=24).contains(n))
                        {
                            0x70 + n - 1
                        } else if let Some(n) = name
                            .strip_prefix("NumPad")
                            .and_then(|n| n.parse::<u8>().ok())
                            .filter(|&n| n < 10)
                        {
                            0x60 + n
                        } else {
                            name.parse::<u8>()
                                .ok()
                                .filter(|&k| k != 0)
                                .ok_or(HotkeyError("Unknown key."))?
                        }
                    }
                };
                Ok(key)
            })
            .collect::<Result<Vec<_>, HotkeyError>>()?;
        let chord = Self::new(keys)?;
        if !chord.is_valid() {
            return Err(HotkeyError("Unsupported shortcut."));
        }
        Ok(chord)
    }
}
impl Serialize for Hotkey {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.to_string().replace(' ', ""))
    }
}
impl<'de> Deserialize<'de> for Hotkey {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        String::deserialize(deserializer)?
            .parse()
            .map_err(D::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    // Failure modes: invalid VKs/empty/multiple main keys; reserved Esc/Space;
    // unsupported lone keys/modifiers; unstable ordering/duplicate groups;
    // loss of right-side-only semantics; missing/extra modifiers; unreadable
    // digit/function/OEM names; incompatible settings JSON; wrong menu mask.
    use super::*;

    #[test]
    fn canonical_order_and_json_round_trip() {
        let key = Hotkey::new([RSHIFT, RCTRL, LCTRL, b'D']).expect("chord");
        assert_eq!(key.to_string(), "Ctrl + Shift + D");
        let json = serde_json::to_string(&key).expect("serialize");
        assert_eq!(json, "\"Ctrl+Shift+D\"");
        assert_eq!(serde_json::from_str::<Hotkey>(&json).expect("parse"), key);
        for name in ["F9", "RightCtrl", "RightAlt", "Ctrl+1", "Alt+OemPeriod"] {
            assert_eq!(
                name.parse::<Hotkey>()
                    .expect(name)
                    .to_string()
                    .replace(' ', ""),
                name
            );
        }
    }

    #[test]
    fn validation_matches_the_original_shortcut_policy() {
        for keys in [vec![], vec![0], vec![b'A', b'B']] {
            assert!(Hotkey::new(keys).is_err());
        }
        for name in [
            "A",
            "Ctrl",
            "Win",
            "Shift",
            "RightShift",
            "Ctrl+Escape",
            "Ctrl+Space",
            "Unknown",
        ] {
            assert!(name.parse::<Hotkey>().is_err(), "{name}");
        }
        for name in [
            "Ctrl+Win",
            "RightCtrl",
            "RightAlt",
            "F1",
            "F24",
            "Ctrl+Shift+D",
        ] {
            assert!(name.parse::<Hotkey>().is_ok(), "{name}");
        }
    }

    #[test]
    fn groups_match_either_side_except_a_lone_right_modifier() {
        let held = [RCTRL, RWIN].into();
        assert!(Hotkey::default().matches(&held));
        assert!(!Hotkey::default().has_extra_modifiers(&held));
        assert!(Hotkey::default().has_extra_modifiers(&[RCTRL, RWIN, LSHIFT].into()));
        let right: Hotkey = "RightCtrl".parse().expect("right ctrl");
        assert!(!right.matches(&[LCTRL].into()));
        assert!(right.matches(&[RCTRL].into()));
        assert!(right.has_extra_modifiers(&[RCTRL, LCTRL].into()));
        assert!(Hotkey::default().masks_menu());
        assert!("Alt+F9".parse::<Hotkey>().expect("alt").masks_menu());
        assert!(!right.masks_menu());
    }
}
