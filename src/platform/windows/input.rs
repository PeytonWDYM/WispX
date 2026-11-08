//! Small SendInput boundary shared by menu suppression and clipboard paste.
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetAsyncKeyState, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYEVENTF_KEYUP, SendInput,
    VIRTUAL_KEY, VK_CONTROL, VK_LWIN, VK_MENU, VK_RWIN, VK_SHIFT,
};

pub fn send_keys(keys: &[(u16, bool)]) {
    let inputs: Vec<_> = keys
        .iter()
        .map(|&(key, up)| INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 {
                ki: KEYBDINPUT {
                    wVk: VIRTUAL_KEY(key),
                    dwFlags: if up {
                        KEYEVENTF_KEYUP
                    } else {
                        Default::default()
                    },
                    ..Default::default()
                },
            },
        })
        .collect();
    // SAFETY: every input is initialized and the slice lives through SendInput.
    unsafe {
        SendInput(&inputs, size_of::<INPUT>() as i32);
    }
}
pub fn any_modifier_held() -> bool {
    [VK_CONTROL, VK_LWIN, VK_RWIN, VK_SHIFT, VK_MENU]
        .iter()
        .any(|key| {
            // SAFETY: GetAsyncKeyState accepts any virtual-key number.
            unsafe { GetAsyncKeyState(key.0.into()) as u16 & 0x8000 != 0 }
        })
}
