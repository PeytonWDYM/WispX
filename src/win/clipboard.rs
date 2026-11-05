//! The clipboard. Unlike Linux's tools, it has to be opened first, and fails if another
//! program happens to have it open just then.

use super::ffi::*;
use crate::desktop::SessionKind;
use crate::desktop::clipboard::Clipboard;
use crate::process::Toolbox;
use std::ptr;
use std::slice;
use std::thread;
use std::time::Duration;

const OPEN_TRIES: u32 = 10;
const OPEN_PAUSE: Duration = Duration::from_millis(30);
/// Larger clipboards are left alone rather than half-restored.
const MAX_BACKUP_UNITS: usize = 4 * 1024 * 1024 / 2;
const STATIC_CLASS: &str = "STATIC";

/// What `transient` text carries, so clipboard history and cloud sync leave it out.
const TRANSIENT_FORMATS: [(&str, u32); 3] = [
    ("ExcludeClipboardContentFromMonitorProcessing", 1),
    ("CanIncludeInClipboardHistory", 0),
    ("CanUploadToCloudClipboard", 0),
];

#[derive(Debug)]
pub struct WinClipboard;

impl WinClipboard {
    pub fn detect(_session: &SessionKind, _tools: &Toolbox) -> Result<Self, String> {
        Ok(WinClipboard)
    }

    pub fn tool_name(&self) -> &'static str {
        "the Windows clipboard"
    }
}

/// The clipboard, open. Writing to it needs a window to own the result, and the system
/// says a null one can make that fail, so each opening gets a hidden one.
struct Open {
    owner: Hwnd,
}

impl Open {
    fn new() -> Result<Open, String> {
        let class = wide(STATIC_CLASS);
        // SAFETY: the class is a system one and the name is NUL-terminated.
        let owner = unsafe {
            CreateWindowExW(
                0,
                class.as_ptr(),
                class.as_ptr(),
                0,
                0,
                0,
                0,
                0,
                HWND_MESSAGE,
                ptr::null_mut(),
                ptr::null_mut(),
                ptr::null(),
            )
        };
        if owner.is_null() {
            return Err(format!(
                "cannot make a window to own the clipboard: {}",
                std::io::Error::last_os_error()
            ));
        }
        for attempt in 0..OPEN_TRIES {
            // SAFETY: `owner` was just made on this thread.
            if unsafe { OpenClipboard(owner) } != 0 {
                return Ok(Open { owner });
            }
            if attempt + 1 < OPEN_TRIES {
                thread::sleep(OPEN_PAUSE);
            }
        }
        // SAFETY: as above, and the window is not used after this.
        unsafe {
            DestroyWindow(owner);
        }
        Err("another program has kept the clipboard open".to_string())
    }
}

impl Drop for Open {
    fn drop(&mut self) {
        // SAFETY: the clipboard is open and the window belongs to this thread. What was
        // placed on the clipboard stays there without its owner.
        unsafe {
            CloseClipboard();
            DestroyWindow(self.owner);
        }
    }
}

fn set_transient_flags() {
    for (name, value) in TRANSIENT_FORMATS {
        let name = wide(name);
        // SAFETY: the name is NUL-terminated. The new memory is filled while locked, and
        // the clipboard owns it once `SetClipboardData` has taken it.
        unsafe {
            let format = RegisterClipboardFormatW(name.as_ptr());
            if format == 0 {
                continue;
            }
            let memory = GlobalAlloc(GMEM_MOVEABLE, size_of::<u32>());
            if memory.is_null() {
                continue;
            }
            let target = GlobalLock(memory).cast::<u32>();
            if target.is_null() {
                GlobalFree(memory);
                continue;
            }
            target.write_unaligned(value);
            GlobalUnlock(memory);
            if SetClipboardData(format, memory).is_null() {
                GlobalFree(memory);
            }
        }
    }
}

impl Clipboard for WinClipboard {
    fn read_text(&self) -> Result<Option<String>, String> {
        // SAFETY: nothing is needed for this query.
        if unsafe { IsClipboardFormatAvailable(CF_UNICODETEXT) } == 0 {
            return Ok(None);
        }
        let _open = Open::new()?;
        // SAFETY: the clipboard is open, which keeps its memory valid and unchanged. The
        // read stops at the terminating NUL, or the end of the block if there is none.
        unsafe {
            let memory = GetClipboardData(CF_UNICODETEXT);
            if memory.is_null() {
                return Ok(None);
            }
            let units = GlobalSize(memory) / 2;
            let start = GlobalLock(memory).cast::<u16>();
            if start.is_null() {
                return Ok(None);
            }
            let block = slice::from_raw_parts(start, units);
            let length = block.iter().position(|unit| *unit == 0).unwrap_or(units);
            let text =
                (length <= MAX_BACKUP_UNITS).then(|| String::from_utf16_lossy(&block[..length]));
            GlobalUnlock(memory);
            Ok(text)
        }
    }

    fn write_text(&self, text: &str, transient: bool) -> Result<(), String> {
        let units = wide(text);
        let _open = Open::new()?;
        let failed = |what: &str| format!("{what}: {}", std::io::Error::last_os_error());
        // SAFETY: the clipboard is open. The new memory is large enough for `units`, and
        // the clipboard owns it once `SetClipboardData` has taken it.
        unsafe {
            if EmptyClipboard() == 0 {
                return Err(failed("cannot empty the clipboard"));
            }
            let memory = GlobalAlloc(GMEM_MOVEABLE, units.len() * 2);
            if memory.is_null() {
                return Err(failed("no memory for the clipboard"));
            }
            let target = GlobalLock(memory).cast::<u16>();
            if target.is_null() {
                GlobalFree(memory);
                return Err(failed("cannot lock memory for the clipboard"));
            }
            ptr::copy_nonoverlapping(units.as_ptr(), target, units.len());
            GlobalUnlock(memory);
            if SetClipboardData(CF_UNICODETEXT, memory).is_null() {
                let message = failed("cannot set the clipboard");
                GlobalFree(memory);
                return Err(message);
            }
        }
        if transient {
            set_transient_flags();
        }
        Ok(())
    }
}
