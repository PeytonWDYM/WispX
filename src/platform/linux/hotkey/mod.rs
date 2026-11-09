//! Global shortcuts for the session's display protocol, behind the Windows hook's API.
pub mod keymap;
pub mod portal;
pub mod x11;

use super::session::{Protocol, Session};
use crate::{hotkey::Hotkey, hotkey_machine::HotkeyEvent, ui::DesktopShortcut};
use std::sync::Arc;

pub enum HookMessage {
    Dictation(HotkeyEvent),
    Captured {
        generation: u64,
        key: Option<Hotkey>,
    },
    /// The desktop bound, rebound or couldn't provide the portal shortcuts.
    Desktop(DesktopStatus),
    /// Another app holds a key WispX wanted to claim (X11 grabs are exclusive).
    Conflict(String),
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DesktopStatus {
    Bound {
        hold: String,
        hands_free: String,
        cancel: String,
    },
    Unavailable(String),
}
impl DesktopStatus {
    pub fn shortcut(&self) -> DesktopShortcut {
        match self {
            Self::Bound {
                hold,
                hands_free,
                cancel,
            } => DesktopShortcut {
                keys: hold.split('+').map(|k| k.trim().to_owned()).collect(),
                hint: format!(
                    "Hold to talk · {hands_free} for hands-free · {cancel} cancels. \
                     Your desktop owns these keys; Change… opens its shortcut settings."
                ),
            },
            Self::Unavailable(reason) => DesktopShortcut {
                keys: vec!["Unavailable".into()],
                hint: format!("Global shortcuts aren't available: {reason}."),
            },
        }
    }
}
pub type Sink = Arc<dyn Fn(HookMessage) + Send + Sync>;

pub enum Hotkeys {
    X11(x11::Hook),
    Portal(portal::Portal),
    None,
}
impl Hotkeys {
    /// X11 matches the Windows hook; Wayland asks the desktop's portal instead.
    pub fn start(session: &Session, hotkey: Hotkey, sink: Sink) -> Self {
        match session.protocol {
            Protocol::Wayland => Self::Portal(portal::Portal::start(&hotkey, sink)),
            Protocol::X11 => match x11::Hook::start(hotkey, sink.clone()) {
                Ok(hook) => Self::X11(hook),
                Err(error) => {
                    sink(HookMessage::Desktop(DesktopStatus::Unavailable(
                        error.to_string(),
                    )));
                    Self::None
                }
            },
            Protocol::None => Self::None,
        }
    }
    pub fn set_hotkey(&self, key: Hotkey) {
        if let Self::X11(hook) = self {
            hook.set_hotkey(key);
        }
    }
    pub fn force_idle(&self) {
        match self {
            Self::X11(hook) => hook.force_idle(),
            Self::Portal(portal) => portal.force_idle(),
            Self::None => {}
        }
    }
    /// In-app capture only exists where WispX sees the keyboard; generation 0 never completes.
    pub fn begin_capture(&self) -> u64 {
        match self {
            Self::X11(hook) => hook.begin_capture(),
            _ => 0,
        }
    }
    pub fn cancel_capture(&self) {
        if let Self::X11(hook) = self {
            hook.cancel_capture();
        }
    }
    pub fn desktop_owned(&self) -> bool {
        matches!(self, Self::Portal(_))
    }
    pub fn configure(&self) {
        if let Self::Portal(portal) = self {
            portal.configure();
        }
    }
}
