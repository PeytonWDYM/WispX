//! Temporary dictation clipboard contents stay out of Windows history and cloud sync.
use super::{files::wide, input};
use std::{thread, time::Duration};
use windows::{
    Win32::{
        Foundation::{GlobalFree, HANDLE, HGLOBAL, HWND},
        Graphics::Gdi::{CopyEnhMetaFileW, DeleteEnhMetaFile, HENHMETAFILE},
        System::{
            DataExchange::*,
            Memory::*,
            Ole::{
                CF_BITMAP, CF_DSPBITMAP, CF_DSPENHMETAFILE, CF_DSPMETAFILEPICT, CF_ENHMETAFILE,
                CF_METAFILEPICT, CF_PALETTE, CF_UNICODETEXT,
            },
        },
        UI::Input::KeyboardAndMouse::{VK_CONTROL, VK_V},
        UI::WindowsAndMessaging::{
            CreateWindowExW, DestroyWindow, GetForegroundWindow, WS_EX_TOOLWINDOW, WS_POPUP,
        },
    },
    core::{PCWSTR, w},
};

struct Open {
    owner: HWND,
}
impl Open {
    fn acquire() -> windows::core::Result<Self> {
        // SAFETY: STATIC is a registered system class; this hidden owner lives through our clipboard lock.
        let owner = unsafe {
            CreateWindowExW(
                WS_EX_TOOLWINDOW,
                w!("STATIC"),
                w!("WispX clipboard"),
                WS_POPUP,
                0,
                0,
                0,
                0,
                None,
                None,
                None,
                None,
            )?
        };
        for _ in 0..10 {
            // SAFETY: this thread holds the clipboard until Open is dropped.
            if unsafe { OpenClipboard(Some(owner)) }.is_ok() {
                return Ok(Self { owner });
            }
            thread::sleep(Duration::from_millis(30));
        }
        let error = windows::core::Error::from_thread();
        // SAFETY: no clipboard lock was acquired; release the hidden owner we created.
        unsafe {
            let _ = DestroyWindow(owner);
        }
        Err(error)
    }
}
impl Drop for Open {
    fn drop(&mut self) {
        // SAFETY: acquire opened this thread's clipboard and created its hidden owner.
        unsafe {
            let _ = CloseClipboard();
            let _ = DestroyWindow(self.owner);
        }
    }
}

fn put(format: u32, bytes: &[u8]) -> windows::core::Result<()> {
    // SAFETY: allocate and copy exactly bytes.len(); successful SetClipboardData transfers ownership.
    unsafe {
        let memory = GlobalAlloc(GMEM_MOVEABLE, bytes.len())?;
        let pointer = GlobalLock(memory);
        if pointer.is_null() {
            let _ = GlobalFree(Some(memory));
            return Err(windows::core::Error::from_thread());
        }
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), pointer.cast(), bytes.len());
        let _ = GlobalUnlock(memory);
        if let Err(error) = SetClipboardData(format, Some(HANDLE(memory.0))) {
            let _ = GlobalFree(Some(memory));
            return Err(error);
        }
        Ok(())
    }
}
fn data(format: u32) -> Option<Vec<u8>> {
    // SAFETY: only HGLOBAL-backed formats with a valid GlobalSize/GlobalLock are copied.
    unsafe {
        let handle = HGLOBAL(GetClipboardData(format).ok()?.0);
        let size = GlobalSize(handle);
        if size == 0 {
            return None;
        }
        let pointer = GlobalLock(handle);
        if pointer.is_null() {
            return None;
        }
        let bytes = std::slice::from_raw_parts(pointer.cast::<u8>(), size).to_vec();
        let _ = GlobalUnlock(handle);
        Some(bytes)
    }
}
/// Owns a cloned handle until it is transferred to the clipboard.
struct EnhancedMetafile(HENHMETAFILE);
impl Drop for EnhancedMetafile {
    fn drop(&mut self) {
        // SAFETY: this handle is our CopyEnhMetaFileW copy, never the clipboard's original.
        unsafe {
            let _ = DeleteEnhMetaFile(Some(self.0));
        }
    }
}
enum ClipboardData {
    Global(u32, Vec<u8>),
    Metafile(EnhancedMetafile),
}
fn backup() -> Option<Vec<ClipboardData>> {
    let _open = Open::acquire().ok()?;
    let mut result = Vec::new();
    let mut format = 0;
    loop {
        // SAFETY: clipboard is open, and format is the previous enumeration result.
        format = unsafe { EnumClipboardFormats(format) };
        if format == 0 {
            break;
        }
        if format == u32::from(CF_ENHMETAFILE.0) {
            // SAFETY: the clipboard is locked; clone its handle before it can be emptied.
            // Windows also exposes this format for CF_METAFILEPICT-only clipboards.
            unsafe {
                if let Ok(handle) = GetClipboardData(format) {
                    let copy = CopyEnhMetaFileW(HENHMETAFILE(handle.0), None);
                    if !copy.is_invalid() {
                        result.push(ClipboardData::Metafile(EnhancedMetafile(copy)));
                    }
                }
            }
            continue;
        }
        // Windows synthesizes CF_METAFILEPICT from the enhanced copy and bitmaps from DIBs.
        // These other GDI handles aren't HGLOBAL memory blocks.
        let gdi_handle = [
            CF_BITMAP,
            CF_METAFILEPICT,
            CF_PALETTE,
            CF_ENHMETAFILE,
            CF_DSPBITMAP,
            CF_DSPMETAFILEPICT,
            CF_DSPENHMETAFILE,
        ]
        .iter()
        .any(|f| u32::from(f.0) == format);
        if gdi_handle {
            continue;
        }
        if let Some(bytes) = data(format) {
            result.push(ClipboardData::Global(format, bytes));
        }
    }
    (!result.is_empty()).then_some(result)
}
pub fn set_text(text: &str, exclude: bool) -> windows::core::Result<()> {
    let _open = Open::acquire()?;
    // SAFETY: our clipboard lock permits clearing and publishing formats.
    unsafe {
        EmptyClipboard()?;
    }
    let bytes: Vec<_> = wide(text).iter().flat_map(|c| c.to_le_bytes()).collect();
    put(CF_UNICODETEXT.0.into(), &bytes)?;
    if exclude {
        for (name, value) in [
            ("ExcludeClipboardContentFromMonitorProcessing", 1_u32),
            ("CanIncludeInClipboardHistory", 0),
            ("CanUploadToCloudClipboard", 0),
        ] {
            let name = wide(name);
            // SAFETY: name is a valid NUL-terminated format identifier.
            let format = unsafe { RegisterClipboardFormatW(PCWSTR(name.as_ptr())) };
            put(format, &value.to_le_bytes())?;
        }
    }
    Ok(())
}
pub fn paste(text: &str, target: isize) -> bool {
    for _ in 0..150 {
        if !input::any_modifier_held() {
            break;
        }
        thread::sleep(Duration::from_millis(15));
    }
    // SAFETY: foreground lookup doesn't dereference the expected window handle.
    if unsafe { GetForegroundWindow() } != HWND(target as *mut _) {
        let _ = set_text(text, false);
        return false;
    }
    let backup = backup();
    if set_text(text, true).is_err() {
        return false;
    }
    thread::sleep(Duration::from_millis(25));
    input::send_keys(&[
        (VK_CONTROL.0, false),
        (VK_V.0, false),
        (VK_V.0, true),
        (VK_CONTROL.0, true),
    ]);
    thread::sleep(Duration::from_millis(500));
    if let Some(backup) = backup
        && let Ok(_open) = Open::acquire()
    {
        let ours: Vec<_> = wide(text).iter().flat_map(|c| c.to_le_bytes()).collect();
        if data(CF_UNICODETEXT.0.into()).is_some_and(|current| current.starts_with(&ours)) {
            // SAFETY: restore only while locked and only if our text still owns the clipboard.
            unsafe {
                let _ = EmptyClipboard();
            }
            for entry in backup {
                match entry {
                    ClipboardData::Global(format, bytes) => {
                        let _ = put(format, &bytes);
                    }
                    ClipboardData::Metafile(copy) => {
                        // SAFETY: on success the clipboard owns our cloned handle; on failure Drop deletes it.
                        if unsafe {
                            SetClipboardData(CF_ENHMETAFILE.0.into(), Some(HANDLE(copy.0.0)))
                        }
                        .is_ok()
                        {
                            std::mem::forget(copy);
                        }
                    }
                }
            }
        }
    }
    true
}
