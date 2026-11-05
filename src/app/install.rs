//! What makes WispX a desktop application: its menu entry, autostart entry and icon.

use super::launch;
use crate::paths;
use crate::portal;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};

/// The original WispX icon, one file per size so it stays sharp in a panel.
const ICONS: [(u32, &[u8]); 6] = [
    (16, include_bytes!("../../packaging/icons/wispx-16.png")),
    (24, include_bytes!("../../packaging/icons/wispx-24.png")),
    (32, include_bytes!("../../packaging/icons/wispx-32.png")),
    (48, include_bytes!("../../packaging/icons/wispx-48.png")),
    (64, include_bytes!("../../packaging/icons/wispx-64.png")),
    (256, include_bytes!("../../packaging/icons/wispx-256.png")),
];

pub const ICON_PNG: &[u8] = include_bytes!("../../packaging/icons/wispx-256.png");

pub fn menu_entry_path() -> Result<PathBuf, String> {
    Ok(paths::applications_dir()?.join(format!("{}.desktop", portal::APP_ID)))
}

pub fn autostart_path() -> Result<PathBuf, String> {
    Ok(paths::autostart_dir()?.join(format!("{}.desktop", portal::APP_ID)))
}

/// The `wispx` program, whichever of the two programs is asking.
fn program() -> Result<String, String> {
    let exe = env::current_exe().map_err(|error| format!("cannot find this program: {error}"))?;
    let exe = match exe.with_file_name("wispx") {
        sibling if sibling.is_file() => sibling,
        _ => exe,
    };
    exe.to_str()
        .map(str::to_string)
        .ok_or_else(|| "this program's path is not valid UTF-8".to_string())
}

/// What an entry runs: the app, which starts the daemon itself and stops it again
/// when it quits. A build without the app has only the daemon to offer.
fn arguments(has_app: bool, background: bool) -> &'static str {
    match (has_app, background) {
        (true, false) => "app",
        (true, true) => "app --background",
        (false, _) => "daemon --quiet",
    }
}

/// The entry for the applications menu or for autostart.
pub fn entry_text(exe: &str, arguments: &str) -> Result<String, String> {
    Ok(format!(
        "[Desktop Entry]\n\
         Type=Application\n\
         Name=WispX\n\
         GenericName=Dictation\n\
         Comment=Hold a key, speak, and your words appear\n\
         Exec={} {}\n\
         Icon={}\n\
         Terminal=false\n\
         StartupNotify=false\n\
         Categories=Utility;Accessibility;\n\
         Keywords=dictation;speech;voice;whisper;transcribe;\n",
        quote_exec(exe)?,
        arguments,
        portal::APP_ID,
    ))
}

/// Quotes a program path by the Desktop Entry rules: `%` is doubled, and within
/// double quotes `"`, `` ` ``, `$` and `\` take a backslash. That text then goes
/// through the file format's own escaping, which doubles each backslash.
pub fn quote_exec(path: &str) -> Result<String, String> {
    if path.contains(['\n', '\r']) {
        return Err("this program's path contains a line break".to_string());
    }
    let mut quoted = String::with_capacity(path.len() + 2);
    quoted.push('"');
    for character in path.chars() {
        match character {
            '"' | '`' | '$' => {
                quoted.push_str("\\\\");
                quoted.push(character);
            }
            '\\' => quoted.push_str("\\\\\\\\"),
            '%' => quoted.push_str("%%"),
            other => quoted.push(other),
        }
    }
    quoted.push('"');
    Ok(quoted)
}

pub fn install_menu_entry() -> Result<PathBuf, String> {
    let path = menu_entry_path()?;
    paths::atomic_write(
        &path,
        entry_text(&program()?, arguments(launch::app_available(), false))?.as_bytes(),
    )?;
    set_readable(&path);
    Ok(path)
}

/// Turns starting at login on or off, returning the entry's path.
pub fn set_autostart(enabled: bool) -> Result<PathBuf, String> {
    let path = autostart_path()?;
    if enabled {
        paths::atomic_write(
            &path,
            entry_text(&program()?, arguments(launch::app_available(), true))?.as_bytes(),
        )?;
        set_readable(&path);
    } else {
        match fs::remove_file(&path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(format!("cannot remove {}: {error}", path.display())),
        }
    }
    Ok(path)
}

pub fn autostart_enabled() -> bool {
    autostart_path().is_ok_and(|path| path.exists())
}

/// Desktop entries are not secrets, and `atomic_write` makes every file private.
fn set_readable(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o644));
}

/// Best effort: a missing icon is cosmetic.
pub fn install_icons() {
    let Ok(data) = paths::data_dir() else {
        return;
    };
    let Some(share) = data.parent() else {
        return;
    };
    for (size, bytes) in ICONS {
        let path = share
            .join("icons/hicolor")
            .join(format!("{size}x{size}/apps"))
            .join(format!("{}.png", portal::APP_ID));
        if fs::read(&path).is_ok_and(|existing| existing == bytes) {
            continue;
        }
        if paths::atomic_write(&path, bytes).is_ok() {
            set_readable(&path);
        }
    }
}

fn system_entry_exists() -> bool {
    let dirs = env::var("XDG_DATA_DIRS").unwrap_or_else(|_| "/usr/local/share:/usr/share".into());
    dirs.split(':').filter(|dir| !dir.is_empty()).any(|dir| {
        Path::new(dir)
            .join("applications")
            .join(format!("{}.desktop", portal::APP_ID))
            .exists()
    })
}

/// Run when the app starts: puts WispX in the applications menu, and keeps both
/// entries pointing at this program if it has moved. A packaged system entry
/// already covers the menu, so nothing is added over it.
pub fn repair() {
    install_icons();
    let Ok(exe) = program() else {
        return;
    };
    if let Ok(path) = menu_entry_path()
        && let Ok(wanted) = entry_text(&exe, arguments(true, false))
        && fs::read_to_string(&path).ok().as_deref() != Some(wanted.as_str())
        && (path.exists() || !system_entry_exists())
    {
        let _ = paths::atomic_write(&path, wanted.as_bytes());
        set_readable(&path);
    }
    if let Ok(path) = autostart_path()
        && path.exists()
        && let Ok(wanted) = entry_text(&exe, arguments(true, true))
        && fs::read_to_string(&path).ok().as_deref() != Some(wanted.as_str())
    {
        let _ = paths::atomic_write(&path, wanted.as_bytes());
        set_readable(&path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exec_paths_are_quoted_by_the_desktop_entry_rules() {
        assert_eq!(quote_exec("/usr/bin/wispx").unwrap(), r#""/usr/bin/wispx""#);
        assert_eq!(quote_exec("/a b/wispx").unwrap(), r#""/a b/wispx""#);
        assert_eq!(quote_exec("/100%/w").unwrap(), r#""/100%%/w""#);
        assert_eq!(quote_exec("/a$b").unwrap(), r#""/a\\$b""#);
        assert_eq!(quote_exec("/a\"b").unwrap(), r#""/a\\"b""#);
        assert_eq!(quote_exec("/a`b").unwrap(), r#""/a\\`b""#);
        assert_eq!(quote_exec(r"/a\b").unwrap(), r#""/a\\\\b""#);
        assert!(quote_exec("/a\nb").is_err());
    }

    #[test]
    fn the_menu_entry_opens_the_visible_app_with_the_original_icon() {
        let entry = entry_text("/opt/my apps/wispx", arguments(true, false)).unwrap();
        assert!(entry.starts_with("[Desktop Entry]\n"));
        assert!(
            entry.contains("Exec=\"/opt/my apps/wispx\" app\n"),
            "{entry}"
        );
        assert!(entry.contains("Icon=io.github.PeytonWDYM.WispX\n"));
        assert!(entry.contains("Type=Application\n"));
        assert!(!entry.contains("NoDisplay"), "{entry}");
        assert!(!entry.contains("daemon"), "{entry}");
    }

    #[test]
    fn autostart_opens_the_app_in_the_background() {
        let entry = entry_text("/usr/bin/wispx", arguments(true, true)).unwrap();
        assert!(entry.contains("Exec=\"/usr/bin/wispx\" app --background\n"));
    }

    #[test]
    fn a_build_without_the_app_starts_the_daemon_instead() {
        for background in [false, true] {
            let entry = entry_text("/usr/bin/wispx", arguments(false, background)).unwrap();
            assert!(
                entry.contains("Exec=\"/usr/bin/wispx\" daemon --quiet\n"),
                "{entry}"
            );
        }
    }

    #[test]
    fn the_icons_are_real_pngs_of_the_size_they_claim() {
        for (size, bytes) in ICONS {
            assert_eq!(&bytes[..8], b"\x89PNG\r\n\x1a\n", "{size}");
            let width = u32::from_be_bytes(bytes[16..20].try_into().unwrap());
            assert_eq!(width, size);
        }
    }
}
