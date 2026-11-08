//! Self-install and shell links; development entry points bypass this module.
use super::{files::wide, instance::QUIT_NAME, startup};
use std::{
    fs, io,
    path::PathBuf,
    process::Command,
    thread,
    time::{Duration, Instant},
};
use windows::{
    Win32::{
        Foundation::{CloseHandle, HWND},
        Globalization::{CSTR_EQUAL, CompareStringOrdinal},
        System::{
            Com::{
                CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED, CoCreateInstance, CoInitializeEx,
                CoUninitialize, IPersistFile,
            },
            Threading::{CREATE_NO_WINDOW, EVENT_MODIFY_STATE, OpenEventW, SetEvent},
        },
        UI::Shell::{IShellLinkW, ShellExecuteW, ShellLink},
    },
    core::{Interface, PCWSTR, w},
};
fn local_dir() -> io::Result<PathBuf> {
    std::env::var_os("LOCALAPPDATA")
        .map(|d| PathBuf::from(d).join("Programs").join("WispX"))
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "LOCALAPPDATA is unavailable"))
}
fn shortcut_path() -> io::Result<PathBuf> {
    std::env::var_os("APPDATA")
        .map(|d| {
            PathBuf::from(d)
                .join("Microsoft")
                .join("Windows")
                .join("Start Menu")
                .join("Programs")
                .join("WispX.lnk")
        })
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "APPDATA is unavailable"))
}
fn shortcut() -> Result<(), Box<dyn std::error::Error>> {
    let dir = local_dir()?;
    let exe = wide(dir.join("WispX.exe"));
    let dir = wide(dir);
    let link_path = wide(shortcut_path()?);
    // SAFETY: COM initialization is balanced, and all shell-link arguments remain alive.
    unsafe {
        CoInitializeEx(None, COINIT_APARTMENTTHREADED).ok()?;
        let result = (|| {
            let link: IShellLinkW = CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER)?;
            link.SetPath(PCWSTR(exe.as_ptr()))?;
            link.SetWorkingDirectory(PCWSTR(dir.as_ptr()))?;
            link.SetIconLocation(PCWSTR(exe.as_ptr()), 0)?;
            link.SetDescription(w!("WispX: hold Ctrl+Win to dictate with ChatGPT"))?;
            let persist: IPersistFile = link.cast()?;
            persist.Save(PCWSTR(link_path.as_ptr()), true)
        })();
        CoUninitialize();
        result?;
    }
    Ok(())
}
pub fn ensure_installed(portable: bool) -> Result<bool, Box<dyn std::error::Error>> {
    if portable {
        return Ok(false);
    }
    let current = std::env::current_exe()?;
    let dir = local_dir()?;
    let installed = dir.join("WispX.exe");
    // Canonicalization resolves separators, junctions and extended-length path prefixes.
    // The destination need not exist yet on a first install.
    let current_path = wide(fs::canonicalize(&current)?);
    let already_installed = match fs::canonicalize(&installed) {
        Ok(path) => {
            let installed_path = wide(path);
            // SAFETY: both slices contain valid UTF-16 paths; Windows performs ordinal case folding.
            unsafe { CompareStringOrdinal(&current_path, &installed_path, true) == CSTR_EQUAL }
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => false,
        Err(error) => return Err(error.into()),
    };
    if already_installed {
        let _ = shortcut();
        return Ok(false);
    }
    if cfg!(debug_assertions) || current.to_string_lossy().contains("\\bin\\") {
        return Ok(false);
    }
    // SAFETY: signal only WispX's cooperative update event; never terminate a user process.
    unsafe {
        if let Ok(event) = OpenEventW(EVENT_MODIFY_STATE, false, QUIT_NAME) {
            let _ = SetEvent(event);
            let _ = CloseHandle(event);
        }
    }
    if fs::create_dir_all(&dir).is_err() {
        return Ok(false);
    }
    let start = Instant::now();
    loop {
        match fs::copy(&current, &installed) {
            Ok(_) => break,
            Err(_) if start.elapsed() < Duration::from_secs(4) => {
                thread::sleep(Duration::from_millis(50))
            }
            Err(_) => return Ok(false),
        }
    }
    let _ = shortcut();
    let exe = wide(installed);
    let directory = wide(dir);
    // SAFETY: file and working-directory strings are NUL-terminated; ShellExecute owns launch setup.
    let launched = unsafe {
        ShellExecuteW(
            Some(HWND::default()),
            w!("open"),
            PCWSTR(exe.as_ptr()),
            None,
            PCWSTR(directory.as_ptr()),
            windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL,
        )
    };
    Ok(launched.0 as isize > 32)
}

pub fn uninstall() -> io::Result<()> {
    let dir = local_dir()?;
    // These paths are fixed children of known per-user directories.
    if dir.file_name().and_then(|n| n.to_str()) != Some("WispX") {
        return Err(io::Error::other("Invalid installation directory"));
    }
    let _ = fs::remove_file(shortcut_path()?);
    let _ = startup::set(false);
    if let Some(appdata) = std::env::var_os("APPDATA") {
        let _ = fs::remove_dir_all(PathBuf::from(appdata).join("WispX"));
    }
    // PowerShell waits for this exe to exit, then removes only the verified install directory.
    let escaped = dir.to_string_lossy().replace('\'', "''");
    let script =
        format!("Start-Sleep -Seconds 2; Remove-Item -LiteralPath '{escaped}' -Recurse -Force");
    use std::os::windows::process::CommandExt;
    Command::new("powershell.exe")
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-WindowStyle",
            "Hidden",
            "-Command",
            &script,
        ])
        .creation_flags(CREATE_NO_WINDOW.0)
        .spawn()?;
    Ok(())
}
