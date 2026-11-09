//! Which display protocol and desktop this process runs under.
use std::ffi::OsStr;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Protocol {
    Wayland,
    X11,
    /// No display server is reachable, as in a plain SSH or CI shell.
    None,
}
#[derive(Clone, Debug)]
pub struct Session {
    pub protocol: Protocol,
    /// `XDG_CURRENT_DESKTOP`, such as `KDE` or `GNOME:GNOME-Classic`.
    pub desktop: String,
}
impl Session {
    pub fn detect() -> Self {
        Self::from_env(|name| std::env::var_os(name))
    }
    /// Wayland wins whenever a compositor socket is advertised, matching winit's choice.
    pub fn from_env(var: impl Fn(&str) -> Option<std::ffi::OsString>) -> Self {
        let set = |name: &str| var(name).is_some_and(|v| !v.is_empty());
        let protocol = if set("WAYLAND_DISPLAY") || set("WAYLAND_SOCKET") {
            Protocol::Wayland
        } else if set("DISPLAY") {
            Protocol::X11
        } else {
            Protocol::None
        };
        Self {
            protocol,
            desktop: var("XDG_CURRENT_DESKTOP")
                .as_deref()
                .and_then(OsStr::to_str)
                .unwrap_or_default()
                .to_owned(),
        }
    }
    pub fn kde(&self) -> bool {
        self.desktop
            .split(':')
            .any(|part| part.eq_ignore_ascii_case("KDE"))
    }
}

#[cfg(test)]
mod tests {
    // Failure modes: an XWayland DISPLAY hides Wayland; empty variables count as
    // set; a compound XDG_CURRENT_DESKTOP misses KDE.
    use super::*;
    use std::{collections::HashMap, ffi::OsString};

    fn session(vars: &[(&str, &str)]) -> Session {
        let vars: HashMap<String, OsString> = vars
            .iter()
            .map(|&(k, v)| (k.to_owned(), v.into()))
            .collect();
        Session::from_env(|name| vars.get(name).cloned())
    }

    #[test]
    fn protocol_prefers_wayland_and_ignores_empty_values() {
        let both = session(&[("WAYLAND_DISPLAY", "wayland-0"), ("DISPLAY", ":0")]);
        assert_eq!(both.protocol, Protocol::Wayland);
        let x11 = session(&[("WAYLAND_DISPLAY", ""), ("DISPLAY", ":1")]);
        assert_eq!(x11.protocol, Protocol::X11);
        assert_eq!(session(&[]).protocol, Protocol::None);
    }

    #[test]
    fn desktop_matches_any_component() {
        assert!(session(&[("XDG_CURRENT_DESKTOP", "KDE")]).kde());
        assert!(session(&[("XDG_CURRENT_DESKTOP", "foo:kde")]).kde());
        assert!(!session(&[("XDG_CURRENT_DESKTOP", "GNOME")]).kde());
    }
}
