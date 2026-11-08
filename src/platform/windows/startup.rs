use super::files::wide;
use windows::{
    Win32::System::Registry::*,
    core::{PCWSTR, w},
};
const RUN_KEY: PCWSTR = w!("Software\\Microsoft\\Windows\\CurrentVersion\\Run");
pub fn enabled() -> bool {
    // SAFETY: RegGetValueW checks existence without requesting the stored data.
    unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            RUN_KEY,
            w!("WispX"),
            RRF_RT_REG_SZ,
            None,
            None,
            None,
        )
        .is_ok()
    }
}
pub fn set(enabled: bool) -> windows::core::Result<()> {
    // SAFETY: the opened key is closed before returning; registry data is UTF-16 including NUL.
    unsafe {
        let mut key = HKEY::default();
        RegCreateKeyExW(
            HKEY_CURRENT_USER,
            RUN_KEY,
            None,
            None,
            REG_OPTION_NON_VOLATILE,
            KEY_SET_VALUE,
            None,
            &mut key,
            None,
        )
        .ok()?;
        let result = if enabled {
            let exe = std::env::current_exe().map_err(|_| windows::core::Error::from_thread())?;
            let command = wide(format!("\"{}\" --background", exe.display()));
            let bytes =
                std::slice::from_raw_parts(command.as_ptr().cast::<u8>(), command.len() * 2);
            RegSetValueExW(key, w!("WispX"), None, REG_SZ, Some(bytes)).ok()
        } else {
            let status = RegDeleteValueW(key, w!("WispX"));
            if status == windows::Win32::Foundation::ERROR_FILE_NOT_FOUND {
                Ok(())
            } else {
                status.ok()
            }
        };
        let _ = RegCloseKey(key);
        result
    }
}
