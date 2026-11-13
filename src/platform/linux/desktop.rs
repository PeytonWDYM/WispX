//! Freedesktop integration: launcher entry, icon, sign-in autostart and Options styling.
//!
//! Portals identify an unsandboxed program by a desktop entry named after its
//! application ID, so the entry is what lets Plasma show "WispX" in its shortcut
//! and remote-control dialogs. Nothing here needs root.
use super::APP_ID;
use crate::ui::{Glyphs, OptionsWindow};
use slint::ComponentHandle;
use std::{
    fs, io,
    path::{Path, PathBuf},
};

const ICON: &[u8] = include_bytes!("../../../assets/wispx.png");

fn xdg(var: &str, fallback: &str) -> io::Result<PathBuf> {
    std::env::var_os(var)
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(fallback)))
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "HOME is not set"))
}
pub fn entry_path() -> io::Result<PathBuf> {
    Ok(xdg("XDG_DATA_HOME", ".local/share")?.join(format!("applications/{APP_ID}.desktop")))
}
pub fn icon_path() -> io::Result<PathBuf> {
    Ok(
        xdg("XDG_DATA_HOME", ".local/share")?
            .join(format!("icons/hicolor/32x32/apps/{APP_ID}.png")),
    )
}
pub fn autostart_path() -> io::Result<PathBuf> {
    Ok(xdg("XDG_CONFIG_HOME", ".config")?.join(format!("autostart/{APP_ID}.desktop")))
}

/// Quotes an Exec argument per the Desktop Entry spec, then escapes it as a string value.
pub fn exec_argument(path: &Path) -> String {
    let path = path.to_string_lossy();
    let mut quoted = String::from("\"");
    for ch in path.chars() {
        match ch {
            '"' | '`' | '$' | '\\' => {
                quoted.push('\\');
                quoted.push(ch);
            }
            '%' => quoted.push_str("%%"),
            _ => quoted.push(ch),
        }
    }
    quoted.push('"');
    // String values escape backslashes themselves, and cannot hold line breaks.
    quoted
        .replace('\\', "\\\\")
        .replace('\n', "\\n")
        .replace('\r', "\\r")
}
pub fn entry(exe: &Path, background: bool) -> String {
    format!(
        "[Desktop Entry]\nType=Application\nName=WispX\nGenericName=Dictation\n\
         Comment=Hold a shortcut to dictate with ChatGPT\nExec={}{}\nIcon={APP_ID}\n\
         Terminal=false\nCategories=Utility;Accessibility;\nStartupNotify=false\n{}",
        exec_argument(exe),
        if background { " --background" } else { "" },
        if background {
            "X-GNOME-Autostart-enabled=true\n"
        } else {
            ""
        },
    )
}
fn write_if_changed(path: &Path, bytes: &[u8]) -> io::Result<()> {
    if fs::read(path).is_ok_and(|current| current == bytes) {
        return Ok(());
    }
    crate::settings::atomic_write(path, bytes)
}
/// Writes the launcher entry and icon for this executable; `--uninstall` removes them.
pub fn integrate() -> io::Result<()> {
    let exe = std::env::current_exe()?;
    write_if_changed(&icon_path()?, ICON)?;
    write_if_changed(&entry_path()?, entry(&exe, false).as_bytes())?;
    // A moved or updated binary keeps starting at sign-in.
    if startup_enabled() {
        write_if_changed(&autostart_path()?, entry(&exe, true).as_bytes())?;
    }
    Ok(())
}
pub fn startup_enabled() -> bool {
    autostart_path().is_ok_and(|p| p.exists())
}
pub fn set_startup(enabled: bool) -> io::Result<()> {
    let path = autostart_path()?;
    if enabled {
        write_if_changed(&path, entry(&std::env::current_exe()?, true).as_bytes())
    } else {
        match fs::remove_file(path) {
            Err(error) if error.kind() != io::ErrorKind::NotFound => Err(error),
            _ => Ok(()),
        }
    }
}
/// Removes everything `integrate` and the startup toggle wrote. Settings stay.
pub fn remove() -> io::Result<Vec<PathBuf>> {
    let mut removed = Vec::new();
    for path in [entry_path()?, icon_path()?, autostart_path()?] {
        match fs::remove_file(&path) {
            Ok(()) => removed.push(path),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
    }
    Ok(removed)
}

/// Linux has no Segoe icon font and no nearby-text reader yet.
pub fn style_options(window: &OptionsWindow) {
    let glyphs = window.global::<Glyphs>();
    glyphs.set_account("☺".into());
    glyphs.set_shortcut("⌨".into());
    glyphs.set_audio("♪".into());
    glyphs.set_cleanup("✎".into());
    glyphs.set_stats("↗".into());
    glyphs.set_reset("↺".into());
    glyphs.set_general("⚙".into());
    glyphs.set_signed_in("✓".into());
    glyphs.set_signed_out("⚠".into());
    glyphs.set_language("⊕".into());
    glyphs.set_sounds("♫".into());
    glyphs.set_lower_audio("♩".into());
    glyphs.set_idle_pill("▬".into());
    glyphs.set_startup("↻".into());
    glyphs.set_context("¶".into());
    glyphs.set_dictionary("≡".into());
    window.set_startup_title("Start when you sign in".into());
    window.set_context_supported(false);
}

#[cfg(test)]
mod tests {
    // Failure modes: spaces or shell metacharacters split or expand Exec;
    // percent signs become field codes; autostart omits --background.
    use super::*;

    #[test]
    fn exec_paths_are_quoted_and_escaped() {
        assert_eq!(exec_argument(Path::new("/opt/WispX")), "\"/opt/WispX\"");
        assert_eq!(
            exec_argument(Path::new("/home/a b/$x`y\"%")),
            "\"/home/a b/\\\\$x\\\\`y\\\\\"%%\""
        );
    }

    #[test]
    fn autostart_entries_start_in_the_background() {
        let exe = Path::new("/usr/bin/WispX");
        assert!(entry(exe, true).contains("Exec=\"/usr/bin/WispX\" --background\n"));
        assert!(entry(exe, false).contains("Exec=\"/usr/bin/WispX\"\n"));
        assert!(entry(exe, false).contains(&format!("Icon={APP_ID}\n")));
    }
}
