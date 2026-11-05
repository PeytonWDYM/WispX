//! Everything that touches the desktop session: clipboard, focus, paste and
//! feedback. On Linux, KDE Plasma on Wayland is the primary target and other
//! sessions get the subset that can be delivered honestly. Windows gets all of it.

pub mod clipboard;
#[cfg(unix)]
pub mod kwin;
pub mod notify;
pub mod paste;
#[cfg(unix)]
pub mod remote;

use std::path::Path;
use std::sync::Arc;

#[cfg(unix)]
pub use remote::PlasmaInjector as PlatformInjector;

#[cfg(windows)]
pub use crate::win::input::WinInjector as PlatformInjector;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FocusInfo {
    /// Unique for each window, unlike its title or class.
    pub id: String,
    pub class: String,
}

/// How a request for permission to paste ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SetupOutcome {
    Granted,
    Declined,
}

/// The way to paste automatically in this session, if it has one.
#[cfg(unix)]
pub fn platform_injector(session: &SessionKind, runtime: &Path) -> Option<Arc<PlatformInjector>> {
    if !session.is_plasma() {
        return None;
    }
    match remote::token_path() {
        Ok(token_file) => Some(PlatformInjector::new(runtime.to_path_buf(), token_file)),
        Err(error) => {
            eprintln!("wispx: automatic paste is off: {error}");
            None
        }
    }
}

/// Windows lets any program send keys, so there is nothing to set up.
#[cfg(windows)]
pub fn platform_injector(_session: &SessionKind, _runtime: &Path) -> Option<Arc<PlatformInjector>> {
    Some(Arc::new(PlatformInjector))
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SessionKind {
    Wayland {
        plasma: bool,
        desktop: String,
    },
    /// Only used as a secondary compatibility path.
    X11,
    Windows,
    Unknown,
}

impl SessionKind {
    #[cfg(unix)]
    pub fn detect() -> Self {
        detect_from(|name| std::env::var(name).ok())
    }

    #[cfg(windows)]
    pub fn detect() -> Self {
        SessionKind::Windows
    }

    pub fn is_wayland(&self) -> bool {
        matches!(self, SessionKind::Wayland { .. })
    }

    pub fn is_windows(&self) -> bool {
        *self == SessionKind::Windows
    }

    pub fn is_plasma(&self) -> bool {
        matches!(self, SessionKind::Wayland { plasma: true, .. })
    }

    pub fn describe(&self) -> String {
        match self {
            SessionKind::Wayland { plasma: true, .. } => "KDE Plasma on Wayland".to_string(),
            SessionKind::Wayland { desktop, .. } if desktop.is_empty() => {
                "Wayland (desktop unknown)".to_string()
            }
            SessionKind::Wayland { desktop, .. } => format!("{desktop} on Wayland"),
            SessionKind::X11 => "X11".to_string(),
            SessionKind::Windows => "Windows".to_string(),
            SessionKind::Unknown => "no graphical session found".to_string(),
        }
    }
}

/// `WAYLAND_DISPLAY` is checked first because Wayland sessions usually also set
/// `DISPLAY` for XWayland, which says nothing about the session itself.
#[cfg_attr(windows, allow(dead_code))]
fn detect_from(get: impl Fn(&str) -> Option<String>) -> SessionKind {
    let set = |name: &str| get(name).is_some_and(|value| !value.is_empty());
    let desktop = get("XDG_CURRENT_DESKTOP").unwrap_or_default();
    if set("WAYLAND_DISPLAY") || get("XDG_SESSION_TYPE").as_deref() == Some("wayland") {
        let plasma = desktop
            .split(':')
            .any(|part| part.eq_ignore_ascii_case("KDE"));
        return SessionKind::Wayland { plasma, desktop };
    }
    if set("DISPLAY") {
        SessionKind::X11
    } else {
        SessionKind::Unknown
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn detect(vars: &[(&str, &str)]) -> SessionKind {
        detect_from(|name| {
            vars.iter()
                .find(|(key, _)| *key == name)
                .map(|(_, value)| value.to_string())
        })
    }

    #[test]
    fn plasma_on_wayland_is_recognized_even_with_xwayland_running() {
        let kind = detect(&[
            ("WAYLAND_DISPLAY", "wayland-1"),
            ("DISPLAY", ":1"),
            ("XDG_CURRENT_DESKTOP", "KDE"),
        ]);
        assert!(kind.is_plasma());
        assert_eq!(kind.describe(), "KDE Plasma on Wayland");
    }

    #[test]
    fn other_wayland_desktops_are_not_mistaken_for_plasma() {
        let kind = detect(&[
            ("WAYLAND_DISPLAY", "wayland-0"),
            ("XDG_CURRENT_DESKTOP", "ubuntu:GNOME"),
        ]);
        assert!(kind.is_wayland());
        assert!(!kind.is_plasma());
        assert_eq!(kind.describe(), "ubuntu:GNOME on Wayland");
    }

    #[test]
    fn x11_and_nothing_are_told_apart() {
        assert_eq!(detect(&[("DISPLAY", ":0")]), SessionKind::X11);
        assert_eq!(detect(&[]), SessionKind::Unknown);
        assert_eq!(detect(&[("WAYLAND_DISPLAY", "")]), SessionKind::Unknown);
    }
}
