//! Everything that touches the desktop session: clipboard, focus, paste and
//! feedback. KDE Plasma on Wayland is the primary target; other sessions get the
//! subset that can be delivered honestly.

pub mod clipboard;
pub mod kwin;
pub mod notify;
pub mod paste;
pub mod remote;

use std::env;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SessionKind {
    Wayland {
        plasma: bool,
        desktop: String,
    },
    /// Only used as a secondary compatibility path.
    X11,
    Unknown,
}

impl SessionKind {
    pub fn detect() -> Self {
        detect_from(|name| env::var(name).ok())
    }

    pub fn is_wayland(&self) -> bool {
        matches!(self, SessionKind::Wayland { .. })
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
            SessionKind::Unknown => "no graphical session found".to_string(),
        }
    }
}

/// `WAYLAND_DISPLAY` is checked first because Wayland sessions usually also set
/// `DISPLAY` for XWayland, which says nothing about the session itself.
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
