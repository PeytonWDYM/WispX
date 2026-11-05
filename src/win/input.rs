//! Pressing the paste keys. Windows lets any program send keys, so unlike on Plasma
//! there is nothing to ask for.

use super::ffi::*;
use crate::desktop::paste::{Injector, Keys};
use crate::desktop::{FocusInfo, SetupOutcome};
use crate::hotkey::{VK_CONTROL, VK_LWIN, VK_MENU, VK_RWIN, VK_SHIFT};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

const VK_INSERT: u16 = 0x2D;
const VK_V: u16 = 0x56;
const CLASS_LENGTH: usize = 256;
/// How long the fingers on the shortcut that ended dictation get to come off the keys:
/// Ctrl+V with Win still held would be some other shortcut altogether.
const MODIFIER_POLLS: u32 = 150;
const MODIFIER_POLL: Duration = Duration::from_millis(15);

#[derive(Debug)]
pub struct WinInjector;

impl WinInjector {
    pub fn restore_in_background(self: &Arc<Self>) {}

    pub fn setup(&self) -> Result<SetupOutcome, String> {
        Ok(SetupOutcome::Granted)
    }

    pub fn has_permission(&self) -> bool {
        true
    }
}

fn key_event(vk: u16, up: bool) -> Input {
    let mut flags = if up { KEYEVENTF_KEYUP } else { 0 };
    if vk == VK_INSERT {
        flags |= KEYEVENTF_EXTENDEDKEY;
    }
    Input {
        kind: INPUT_KEYBOARD,
        key: KeybdInput {
            vk,
            scan: 0,
            flags,
            time: 0,
            extra: 0,
        },
        padding: [0; 8],
    }
}

fn chord(keys: Keys) -> &'static [u16] {
    const CONTROL: u16 = VK_CONTROL as u16;
    const SHIFT: u16 = VK_SHIFT as u16;
    match keys {
        Keys::CtrlV => &[CONTROL, VK_V],
        Keys::CtrlShiftV => &[CONTROL, SHIFT, VK_V],
        Keys::ShiftInsert => &[SHIFT, VK_INSERT],
    }
}

/// Everything going down in order and coming up in reverse.
fn events(keys: &[u16]) -> Vec<Input> {
    keys.iter()
        .map(|vk| key_event(*vk, false))
        .chain(keys.iter().rev().map(|vk| key_event(*vk, true)))
        .collect()
}

impl Injector for Arc<WinInjector> {
    fn ready(&self) -> bool {
        true
    }

    fn current_focus(&self) -> Result<Option<FocusInfo>, String> {
        // SAFETY: the buffer's length is passed along. A window that has since gone just
        // leaves the name empty.
        unsafe {
            let window = GetForegroundWindow();
            if window.is_null() {
                return Ok(None);
            }
            let mut class = [0u16; CLASS_LENGTH];
            GetClassNameW(window, class.as_mut_ptr(), CLASS_LENGTH as i32);
            Ok(Some(FocusInfo {
                id: (window as usize).to_string(),
                class: from_wide(&class),
            }))
        }
    }

    fn prepare(&self) -> Result<(), String> {
        for _ in 0..MODIFIER_POLLS {
            if !modifiers_held() {
                return Ok(());
            }
            thread::sleep(MODIFIER_POLL);
        }
        Err("a modifier key was still held down".to_string())
    }

    fn send_paste(&self, keys: Keys) -> Result<(), String> {
        let events = events(chord(keys));
        // SAFETY: the slice and its length agree, and it outlives the call.
        let sent = unsafe {
            SendInput(
                events.len() as u32,
                events.as_ptr(),
                size_of::<Input>() as i32,
            )
        };
        if sent as usize == events.len() {
            Ok(())
        } else {
            Err(format!(
                "Windows would not send the keys: {}",
                std::io::Error::last_os_error()
            ))
        }
    }
}

fn modifiers_held() -> bool {
    [VK_CONTROL, VK_LWIN, VK_RWIN, VK_SHIFT, VK_MENU]
        .iter()
        // SAFETY: takes a plain value.
        .any(|vk| unsafe { GetAsyncKeyState(*vk as i32) } as u16 & 0x8000 != 0)
}
