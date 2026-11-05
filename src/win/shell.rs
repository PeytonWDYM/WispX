//! Asking the desktop to open something.

use super::ffi::*;
use std::path::Path;
use std::process::Command;
use std::ptr;

/// `ShellExecuteW` says it worked by returning more than this.
const SHELL_SUCCESS: usize = 32;

/// Opens a text file in whatever the user has for it. `.json` often has nothing, so
/// Notepad is what is left.
pub fn open_text_file(path: &Path) -> Result<(), String> {
    let verb = wide("open");
    let file = wide_os(path.as_os_str());
    // SAFETY: both strings are NUL-terminated, and the other pointers may be null.
    let result = unsafe {
        ShellExecuteW(
            ptr::null_mut(),
            verb.as_ptr(),
            file.as_ptr(),
            ptr::null(),
            ptr::null(),
            SW_SHOWNORMAL,
        )
    };
    if result as usize > SHELL_SUCCESS {
        return Ok(());
    }
    Command::new("notepad.exe")
        .arg(path)
        .spawn()
        .map(|_| ())
        .map_err(|error| format!("cannot open {}: {error}", path.display()))
}
