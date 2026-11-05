//! What makes WispX a Windows application. As with the original, the downloaded program
//! installs itself: it copies itself to `%LOCALAPPDATA%\Programs\WispX`, adds a Start menu
//! entry, and starts from there. Running a newer download later is the update.

use super::ffi::*;
use crate::app::instance;
use crate::paths;
use std::env;
use std::ffi::{OsString, c_void};
use std::fs;
use std::os::windows::ffi::OsStringExt;
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::ptr;
use std::thread;
use std::time::Duration;

const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
const VALUE_NAME: &str = "WispX";
const SHORTCUT_NAME: &str = "WispX.lnk";
const DESCRIPTION: &str = "WispX: hold Ctrl+Win to dictate";
const GUI_PROGRAM: &str = "wispx-gui.exe";
const CLI_PROGRAM: &str = "wispx.exe";
const COPY_TRIES: u32 = 20;
const COPY_PAUSE: Duration = Duration::from_millis(250);

/// `FOLDERID_Programs`.
static PROGRAMS_FOLDER: Guid = Guid(
    0xA77F_5D77,
    0x2E2B,
    0x44C3,
    [0xA6, 0xA2, 0xAB, 0xA6, 0x01, 0x05, 0x4A, 0x51],
);
static SHELL_LINK_CLASS: Guid = Guid(0x0002_1401, 0, 0, [0xC0, 0, 0, 0, 0, 0, 0, 0x46]);
static SHELL_LINK_INTERFACE: Guid = Guid(0x0002_14F9, 0, 0, [0xC0, 0, 0, 0, 0, 0, 0, 0x46]);
static PERSIST_FILE_INTERFACE: Guid = Guid(0x0000_010B, 0, 0, [0xC0, 0, 0, 0, 0, 0, 0, 0x46]);

fn programs_folder() -> Result<PathBuf, String> {
    let mut path = ptr::null_mut();
    // SAFETY: the GUID is valid, and the result is copied and then freed here.
    unsafe {
        let result = SHGetKnownFolderPath(
            &raw const PROGRAMS_FOLDER,
            0,
            ptr::null_mut(),
            &raw mut path,
        );
        if result != 0 {
            CoTaskMemFree(path.cast());
            return Err(format!(
                "cannot find the Start menu folder (error {result:#x})"
            ));
        }
        let mut length = 0;
        while *path.add(length) != 0 {
            length += 1;
        }
        let text = OsString::from_wide(std::slice::from_raw_parts(path, length));
        CoTaskMemFree(path.cast());
        Ok(PathBuf::from(text))
    }
}

pub fn menu_entry_path() -> Result<PathBuf, String> {
    Ok(programs_folder()?.join(SHORTCUT_NAME))
}

fn install_dir() -> Result<PathBuf, String> {
    let base = env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .ok_or("LOCALAPPDATA is not set")?;
    Ok(base.join("Programs").join("WispX"))
}

fn this_program() -> Result<PathBuf, String> {
    env::current_exe().map_err(|error| format!("cannot find this program: {error}"))
}

/// The windowed program, whichever of the two programs is asking.
fn gui_program() -> Result<PathBuf, String> {
    let exe = this_program()?;
    Ok(match exe.with_file_name(GUI_PROGRAM) {
        sibling if sibling.is_file() => sibling,
        _ => exe,
    })
}

fn same_path(first: &Path, second: &Path) -> bool {
    first.as_os_str().eq_ignore_ascii_case(second.as_os_str())
}

/// Whether this is the copy in the install folder.
pub fn is_installed_copy() -> bool {
    match (this_program(), install_dir()) {
        (Ok(exe), Ok(dir)) => exe.parent().is_some_and(|parent| same_path(parent, &dir)),
        _ => false,
    }
}

/// A COM object: a pointer to a table of function pointers, of which these are used.
type ComObject = *mut *const *const c_void;

/// Calls method `index` of the object's table, as a function of the given type.
macro_rules! com_call {
    ($object:expr, $index:expr, ($($kind:ty),*) $(, $value:expr)*) => {{
        let object: ComObject = $object;
        let method: unsafe extern "system" fn(ComObject $(, $kind)*) -> i32 =
            std::mem::transmute(*(*object).add($index));
        method(object $(, $value)*)
    }};
}

const COM_RELEASE: usize = 2;
const LINK_SET_DESCRIPTION: usize = 7;
const LINK_SET_WORKING_DIRECTORY: usize = 9;
const LINK_SET_ICON_LOCATION: usize = 17;
const LINK_SET_PATH: usize = 20;
const QUERY_INTERFACE: usize = 0;
const PERSIST_SAVE: usize = 6;

fn create_shortcut(shortcut: &Path, target: &Path) -> Result<(), String> {
    let path = wide_os(target.as_os_str());
    let dir = wide_os(target.parent().unwrap_or(target).as_os_str());
    let description = wide(DESCRIPTION);
    let file = wide_os(shortcut.as_os_str());
    let failed = |what: &str, code: i32| format!("cannot {what} the shortcut (error {code:#x})");
    // SAFETY: the method numbers are those of `IUnknown`, `IShellLinkW` and `IPersistFile`,
    // called with their signatures, on objects held until they are released here. COM
    // being set up already is fine, and an error doing so shows up in the call after.
    unsafe {
        CoInitializeEx(ptr::null_mut(), COINIT_APARTMENTTHREADED);
        let mut link = ptr::null_mut();
        let result = CoCreateInstance(
            &raw const SHELL_LINK_CLASS,
            ptr::null_mut(),
            CLSCTX_INPROC_SERVER,
            &raw const SHELL_LINK_INTERFACE,
            &raw mut link,
        );
        if result < 0 {
            return Err(failed("make", result));
        }
        let link: ComObject = link.cast();
        com_call!(link, LINK_SET_PATH, (*const u16), path.as_ptr());
        com_call!(link, LINK_SET_WORKING_DIRECTORY, (*const u16), dir.as_ptr());
        com_call!(
            link,
            LINK_SET_DESCRIPTION,
            (*const u16),
            description.as_ptr()
        );
        com_call!(
            link,
            LINK_SET_ICON_LOCATION,
            (*const u16, i32),
            path.as_ptr(),
            0
        );
        let mut persist: *mut c_void = ptr::null_mut();
        let result = com_call!(
            link,
            QUERY_INTERFACE,
            (*const Guid, *mut *mut c_void),
            &raw const PERSIST_FILE_INTERFACE,
            &raw mut persist
        );
        if result < 0 {
            com_call!(link, COM_RELEASE, ());
            return Err(failed("save", result));
        }
        let persist: ComObject = persist.cast();
        let result = com_call!(persist, PERSIST_SAVE, (*const u16, i32), file.as_ptr(), 1);
        com_call!(persist, COM_RELEASE, ());
        com_call!(link, COM_RELEASE, ());
        if result < 0 {
            return Err(failed("save", result));
        }
    }
    Ok(())
}

pub fn install_menu_entry() -> Result<PathBuf, String> {
    let shortcut = menu_entry_path()?;
    if let Some(dir) = shortcut.parent() {
        fs::create_dir_all(dir).map_err(|error| format!("{}: {error}", dir.display()))?;
    }
    create_shortcut(&shortcut, &gui_program()?)?;
    Ok(shortcut)
}

/// The icon is inside the program, so there is nothing to install.
pub fn install_icons() {}

fn run_value() -> Option<String> {
    let key = wide(RUN_KEY);
    let value = wide(VALUE_NAME);
    let mut data = [0u16; 2048];
    let mut size = size_of_val(&data) as u32;
    // SAFETY: the strings are NUL-terminated, and the buffer's size is passed along.
    let result = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            key.as_ptr(),
            value.as_ptr(),
            RRF_RT_REG_SZ,
            ptr::null_mut(),
            data.as_mut_ptr().cast(),
            &raw mut size,
        )
    };
    (result == 0).then(|| from_wide(&data))
}

pub fn autostart_enabled() -> bool {
    run_value().is_some()
}

/// Whether `command` quotes a program file that exists.
fn points_at_a_program(command: &str) -> bool {
    command
        .strip_prefix('"')
        .and_then(|rest| rest.split('"').next())
        .is_some_and(|path| Path::new(path).is_file())
}

/// What the registry value is, for messages: it has no file to point to.
fn run_entry_name() -> PathBuf {
    PathBuf::from(format!(r"HKEY_CURRENT_USER\{RUN_KEY}\{VALUE_NAME}"))
}

pub fn set_autostart(enabled: bool) -> Result<PathBuf, String> {
    let key = wide(RUN_KEY);
    let value = wide(VALUE_NAME);
    if !enabled {
        // SAFETY: the strings are NUL-terminated. It not being there is what is wanted.
        unsafe {
            RegDeleteKeyValueW(HKEY_CURRENT_USER, key.as_ptr(), value.as_ptr());
        }
        return Ok(run_entry_name());
    }
    let program = gui_program()?;
    let command = wide(&format!("\"{}\" --background", program.display()));
    // SAFETY: the strings are NUL-terminated, and the size is that of `command` in bytes.
    let result = unsafe {
        RegSetKeyValueW(
            HKEY_CURRENT_USER,
            key.as_ptr(),
            value.as_ptr(),
            REG_SZ,
            command.as_ptr().cast(),
            (command.len() * 2) as u32,
        )
    };
    if result != 0 {
        return Err(format!(
            "cannot turn on start at login: {}",
            std::io::Error::from_raw_os_error(result)
        ));
    }
    Ok(run_entry_name())
}

/// Puts back what makes this the installed app, if something took it away, and keeps
/// start at login from pointing to a copy that has gone.
pub fn repair() {
    if is_installed_copy() {
        let _ = install_menu_entry();
    }
    if run_value().is_some_and(|command| !points_at_a_program(&command)) {
        let _ = set_autostart(true);
    }
}

fn copy_retrying(from: &Path, to: &Path) -> std::io::Result<()> {
    let mut last = Ok(());
    for _ in 0..COPY_TRIES {
        match fs::copy(from, to) {
            Ok(_) => return Ok(()),
            // The old program takes a moment to end, and until then its file is locked.
            Err(error) => last = Err(error),
        }
        thread::sleep(COPY_PAUSE);
    }
    last
}

fn is_development_build(exe: &Path) -> bool {
    let text = exe.to_string_lossy().to_lowercase();
    text.contains(r"\bin\") || text.contains(r"\target\")
}

/// Copies this download into place and starts it from there. Returns `true` when that
/// happened, so this process should go. If it can't be done, WispX runs from where it is.
pub fn ensure_installed(portable: bool, background: bool) -> bool {
    let (Ok(exe), Ok(dir)) = (this_program(), install_dir()) else {
        return false;
    };
    if is_installed_copy() || portable || is_development_build(&exe) {
        return false;
    }
    // Ask a running copy to quit so its file can be replaced.
    let _ = instance::send("quit");
    let installed = dir.join(GUI_PROGRAM);
    let copied = fs::create_dir_all(&dir).is_ok() && copy_retrying(&exe, &installed).is_ok();
    if !copied {
        return false;
    }
    let cli = exe.with_file_name(CLI_PROGRAM);
    if cli.is_file() {
        let _ = copy_retrying(&cli, &dir.join(CLI_PROGRAM));
    }
    if let Ok(shortcut) = menu_entry_path() {
        let _ = create_shortcut(&shortcut, &installed);
    }
    let mut command = Command::new(&installed);
    command.current_dir(&dir);
    if background {
        command.arg("--background");
    }
    command.spawn().is_ok()
}

/// Removes the shortcut, start at login, settings, models and the install folder. The
/// Codex login is not WispX's to touch. A program can't delete itself while it runs, so
/// a short-lived command does that part once this has gone.
pub fn uninstall() -> Result<(), String> {
    if let Ok(shortcut) = menu_entry_path() {
        let _ = fs::remove_file(shortcut);
    }
    let _ = set_autostart(false);
    let dir = install_dir()?;
    let mut removing = format!("rmdir /s /q \"{}\"", dir.display());
    if let Ok(config) = paths::default_config_file()
        && let Some(config_dir) = config.parent()
    {
        removing.push_str(&format!(" & rmdir /s /q \"{}\"", config_dir.display()));
    }
    if let Ok(data) = paths::data_dir() {
        removing.push_str(&format!(" & rmdir /s /q \"{}\"", data.display()));
    }
    Command::new("cmd.exe")
        .raw_arg(format!("/c ping -n 3 127.0.0.1 >nul & {removing}"))
        .creation_flags(CREATE_NO_WINDOW)
        .spawn()
        .map(|_| ())
        .map_err(|error| format!("cannot remove {}: {error}", dir.display()))
}
