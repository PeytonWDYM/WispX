use std::{
    io::{self, Read},
    path::Path,
};
use windows::{
    Win32::Storage::FileSystem::{MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW},
    core::PCWSTR,
};

pub fn wide(value: impl AsRef<std::ffi::OsStr>) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;
    value.as_ref().encode_wide().chain([0]).collect()
}
/// Codex can replace auth.json while this read is in progress.
pub fn read_shared(path: &Path) -> io::Result<String> {
    use std::os::windows::fs::OpenOptionsExt;
    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .share_mode(1 | 2 | 4)
        .open(path)?;
    let mut text = String::new();
    file.read_to_string(&mut text)?;
    Ok(text)
}

/// Windows rename must explicitly allow replacement of an existing settings/auth file.
pub fn replace(from: &Path, to: &Path) -> io::Result<()> {
    let from = wide(from);
    let to = wide(to);
    // SAFETY: both paths are NUL-terminated and remain alive for the call.
    unsafe {
        MoveFileExW(
            PCWSTR(from.as_ptr()),
            PCWSTR(to.as_ptr()),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    }
    .map_err(io::Error::other)
}

/// Asks where to save Listen notes with the standard Save dialog, modal to `owner`.
/// Returns `None` when the person cancels. Runs a modal loop on the calling UI thread.
pub fn choose_save_path(
    owner: windows::Win32::Foundation::HWND,
    name: &str,
) -> Result<Option<std::path::PathBuf>, String> {
    use windows::{
        Win32::{
            Foundation::ERROR_CANCELLED,
            System::Com::{
                CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED, CoCreateInstance, CoInitializeEx,
                CoTaskMemFree, CoUninitialize,
            },
            UI::Shell::{
                Common::COMDLG_FILTERSPEC, FOS_FORCEFILESYSTEM, FOS_OVERWRITEPROMPT,
                FileSaveDialog, IFileSaveDialog, SIGDN_FILESYSPATH,
            },
        },
        core::{HRESULT, w},
    };
    // SAFETY: COM is initialized (or already was) on this thread for the dialog's lifetime;
    // strings outlive the calls that read them, and the returned path is freed once copied.
    unsafe {
        let initialized = CoInitializeEx(None, COINIT_APARTMENTTHREADED).is_ok();
        let result = (|| -> windows::core::Result<Option<std::path::PathBuf>> {
            let dialog: IFileSaveDialog =
                CoCreateInstance(&FileSaveDialog, None, CLSCTX_INPROC_SERVER)?;
            dialog.SetFileTypes(&[
                COMDLG_FILTERSPEC {
                    pszName: w!("Markdown"),
                    pszSpec: w!("*.md"),
                },
                COMDLG_FILTERSPEC {
                    pszName: w!("Text"),
                    pszSpec: w!("*.txt"),
                },
            ])?;
            dialog.SetDefaultExtension(w!("md"))?;
            dialog.SetTitle(w!("Save notes"))?;
            let name = wide(name);
            dialog.SetFileName(PCWSTR(name.as_ptr()))?;
            dialog.SetOptions(dialog.GetOptions()? | FOS_OVERWRITEPROMPT | FOS_FORCEFILESYSTEM)?;
            match dialog.Show(Some(owner)) {
                Err(error) if error.code() == HRESULT::from_win32(ERROR_CANCELLED.0) => {
                    return Ok(None);
                }
                result => result?,
            }
            let path = dialog.GetResult()?.GetDisplayName(SIGDN_FILESYSPATH)?;
            let text = path.to_string();
            CoTaskMemFree(Some(path.0 as *const _));
            Ok(Some(std::path::PathBuf::from(text.map_err(|e| {
                windows::core::Error::new(windows::core::HRESULT(-1), e.to_string())
            })?)))
        })();
        if initialized {
            CoUninitialize();
        }
        result.map_err(|e| e.message())
    }
}
