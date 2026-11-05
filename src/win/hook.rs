//! The keyboard hook. The logic is in `crate::hotkey`, and this feeds it.

use super::ffi::*;
use crate::hotkey::{Fired, Hotkeys, Key, VK_MENU, VK_SHIFT};
use crate::session::Input as Dictation;
use crate::shortcuts::{Event, Sink, Status};
use std::ptr;
use std::sync::Mutex;
use std::thread;

/// A key with no meaning of its own, which stops Windows from reading a Win key that
/// is let go as a request for the Start menu.
const VK_MASK: u16 = 0xE8;
const WHAT_THE_KEYS_ARE: &str = "cancel: Esc, dictate: Ctrl+Win, hands-free: Ctrl+Win then Space";

struct State {
    keys: Hotkeys,
    sink: Sink,
}

/// A hook function takes no argument to hang state on, and there is only ever one.
static STATE: Mutex<Option<State>> = Mutex::new(None);

fn input_for(fired: Fired) -> Dictation {
    match fired {
        Fired::Start => Dictation::Press,
        Fired::Release => Dictation::Release,
        Fired::Lock => Dictation::Toggle,
        Fired::StopLocked => Dictation::Stop,
        Fired::Cancel => Dictation::Cancel,
        Fired::CancelSilent => Dictation::CancelSilent,
    }
}

fn is_down(vk: u32) -> bool {
    // SAFETY: takes a plain value.
    unsafe { GetAsyncKeyState(vk as i32) as u16 & 0x8000 != 0 }
}

fn mask_start_menu() {
    let key = |flags: u32| Input {
        kind: INPUT_KEYBOARD,
        key: KeybdInput {
            vk: VK_MASK,
            scan: 0,
            flags,
            time: 0,
            extra: 0,
        },
        padding: [0; 8],
    };
    let keys = [key(0), key(KEYEVENTF_KEYUP)];
    // SAFETY: the array and its length agree, and it outlives the call.
    unsafe {
        SendInput(keys.len() as u32, keys.as_ptr(), size_of::<Input>() as i32);
    }
}

/// Windows drops a hook that takes too long, so this only updates a little state and
/// hands anything longer to the daemon's thread, through `sink`.
unsafe extern "system" fn on_key(code: i32, wparam: usize, lparam: isize) -> isize {
    if code >= 0 {
        let message = wparam as u32;
        let down = matches!(message, WM_KEYDOWN | WM_SYSKEYDOWN);
        let up = matches!(message, WM_KEYUP | WM_SYSKEYUP);
        if down || up {
            // SAFETY: for these messages, `lparam` points to this structure.
            let info = unsafe { &*(lparam as *const KbdLlHook) };
            let key = Key {
                vk: info.vk_code,
                down,
                injected: info.flags & LLKHF_INJECTED != 0,
                other_modifier: is_down(VK_SHIFT) || is_down(VK_MENU),
            };
            let result = STATE.lock().ok().and_then(|mut guard| {
                let state = guard.as_mut()?;
                Some((state.keys.key(key), state.sink.clone()))
            });
            if let Some((outcome, sink)) = result {
                if outcome.mask_start_menu {
                    mask_start_menu();
                }
                if let Some(fired) = outcome.fired {
                    sink(Event::Input(input_for(fired)));
                }
                if outcome.swallow {
                    return 1;
                }
            }
        }
    }
    // SAFETY: passes on what it was given, as a hook is meant to.
    unsafe { CallNextHookEx(ptr::null_mut(), code, wparam, lparam) }
}

/// Runs the hook on its own thread for the life of the daemon. A hook is called on the
/// thread that installed it, between messages, so this thread only pumps them.
pub fn spawn(sink: Sink) {
    thread::spawn(move || {
        sink(Event::Status(Status::Starting));
        if let Ok(mut state) = STATE.lock() {
            *state = Some(State {
                keys: Hotkeys::default(),
                sink: sink.clone(),
            });
        }
        // SAFETY: the function lives as long as the program, and this module's handle
        // is its own.
        let hook =
            unsafe { SetWindowsHookExW(WH_KEYBOARD_LL, on_key, GetModuleHandleW(ptr::null()), 0) };
        if hook.is_null() {
            let error = std::io::Error::last_os_error();
            sink(Event::Status(Status::Unavailable(format!(
                "Windows would not install the keyboard hook: {error}"
            ))));
            return;
        }
        sink(Event::Status(Status::Bound(WHAT_THE_KEYS_ARE.to_string())));
        // SAFETY: a zeroed message is only a place for the system to write to.
        unsafe {
            let mut message: Msg = std::mem::zeroed();
            while GetMessageW(&raw mut message, ptr::null_mut(), 0, 0) > 0 {
                TranslateMessage(&raw const message);
                DispatchMessageW(&raw const message);
            }
        }
    });
}
