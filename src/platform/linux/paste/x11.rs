//! X11 paste keyboard: XTEST with active-window and modifier checks.
use super::{Keyboard, Refused, Target};
use x11rb::{
    connection::Connection,
    protocol::{
        xproto::{self, ConnectionExt as _},
        xtest::ConnectionExt as _,
    },
    rust_connection::RustConnection,
};

pub(super) struct X11 {
    conn: RustConnection,
    root: xproto::Window,
    active: xproto::Atom,
}
impl X11 {
    pub(super) fn connect() -> Result<Self, Box<dyn std::error::Error>> {
        let (conn, screen) = x11rb::connect(None)?;
        let root = conn.setup().roots[screen].root;
        let active = conn
            .intern_atom(false, b"_NET_ACTIVE_WINDOW")?
            .reply()?
            .atom;
        Ok(Self { conn, root, active })
    }
    fn keycode(&self, keysym: u32) -> Option<u8> {
        let setup = self.conn.setup();
        let (min, max) = (setup.min_keycode, setup.max_keycode);
        let mapping = self
            .conn
            .get_keyboard_mapping(min, max - min + 1)
            .ok()?
            .reply()
            .ok()?;
        let per = usize::from(mapping.keysyms_per_keycode).max(1);
        mapping
            .keysyms
            .chunks(per)
            .position(|syms| syms.contains(&keysym))
            .and_then(|i| u8::try_from(usize::from(min) + i).ok())
    }
    fn send(&self, kind: u8, code: u8) -> Result<(), String> {
        self.conn
            .xtest_fake_input(kind, code, x11rb::CURRENT_TIME, self.root, 0, 0, 0)
            .map(drop)
            .map_err(|e| e.to_string())
    }
}
impl Keyboard for X11 {
    fn ready(&self) -> bool {
        true
    }
    fn describe(&self) -> String {
        "XTEST with _NET_ACTIVE_WINDOW focus checks".into()
    }
    fn focused(&self) -> Option<Target> {
        let window = self
            .conn
            .get_property(
                false,
                self.root,
                self.active,
                xproto::AtomEnum::WINDOW,
                0,
                1,
            )
            .ok()?
            .reply()
            .ok()?
            .value32()
            .and_then(|mut v| v.next())
            .filter(|&w| w != 0)
            .or_else(|| Some(self.conn.get_input_focus().ok()?.reply().ok()?.focus))?;
        let class = self
            .conn
            .get_property(
                false,
                window,
                xproto::AtomEnum::WM_CLASS,
                xproto::AtomEnum::STRING,
                0,
                256,
            )
            .ok()
            .and_then(|c| c.reply().ok())
            .map(|reply| {
                // WM_CLASS is "instance\0class\0"; the class names the application.
                let parts: Vec<_> = reply.value.split(|&b| b == 0).collect();
                String::from_utf8_lossy(parts.get(1).copied().unwrap_or_default()).into_owned()
            })
            .unwrap_or_default();
        Some(Target {
            id: window.to_string(),
            class,
        })
    }
    fn modifier_held(&self) -> Option<bool> {
        // Unknown key state counts as held: refusing beats typing a wrong chord.
        let Some(keys) = self
            .conn
            .query_keymap()
            .ok()
            .and_then(|c| c.reply().ok())
            .map(|r| r.keys)
        else {
            return Some(true);
        };
        // Ctrl, Shift, Alt and Super on both sides, as evdev codes plus 8.
        Some(
            [29_usize, 97, 42, 54, 56, 100, 125, 126]
                .iter()
                .map(|code| code + 8)
                .any(|code| keys[code / 8] & (1 << (code % 8)) != 0),
        )
    }
    fn paste(&self, keysyms: &[u32], target: &Target) -> Result<(), Refused> {
        let codes = keysyms
            .iter()
            .map(|&k| self.keycode(k).ok_or(format!("no key for keysym {k:#x}")))
            .collect::<Result<Vec<_>, _>>()
            .map_err(Refused::Failed)?;
        if self.modifier_held() != Some(false) {
            return Err(Refused::ModifiersHeld);
        }
        if self.focused().as_ref() != Some(target) {
            return Err(Refused::FocusChanged);
        }
        let mut pressed = Vec::new();
        let mut result = Ok(());
        for &code in &codes {
            if let Err(error) = self.send(xproto::KEY_PRESS_EVENT, code) {
                result = Err(Refused::Failed(error));
                break;
            }
            pressed.push(code);
        }
        // Released even after a failure, so no modifier stays stuck down.
        for &code in pressed.iter().rev() {
            let _ = self.send(xproto::KEY_RELEASE_EVENT, code);
        }
        let flushed = self.conn.flush().map_err(|e| e.to_string());
        result?;
        flushed.map_err(Refused::Failed)
    }
}
