//! The hook thread performs only state transitions and posts work to the UI.
use super::input;
use crate::{
    hotkey::Hotkey,
    hotkey_machine::{HotkeyEvent, HotkeyMachine, KeyEvent},
};
use std::{
    cell::RefCell,
    sync::{Arc, Mutex, mpsc},
    thread::{self, JoinHandle},
};
use windows::Win32::{
    Foundation::{HINSTANCE, LPARAM, LRESULT, WPARAM},
    System::{LibraryLoader::GetModuleHandleW, Threading::GetCurrentThreadId},
    UI::WindowsAndMessaging::*,
};

pub enum HookMessage {
    Dictation(HotkeyEvent),
    Captured {
        generation: u64,
        key: Option<Hotkey>,
    },
}
type Sink = Arc<dyn Fn(HookMessage) + Send + Sync>;
struct HookState {
    machine: Arc<Mutex<Control>>,
    sink: Sink,
}
struct Control {
    keys: HotkeyMachine,
    generation: u64,
}
thread_local! { static STATE: RefCell<Option<HookState>> = const { RefCell::new(None) }; }
pub struct Hook {
    machine: Arc<Mutex<Control>>,
    sink: Sink,
    thread_id: u32,
    thread: Option<JoinHandle<()>>,
}
impl Hook {
    pub fn start(hotkey: Hotkey, sink: Sink) -> windows::core::Result<Self> {
        let machine = Arc::new(Mutex::new(Control {
            keys: HotkeyMachine::new(hotkey),
            generation: 0,
        }));
        let state = HookState {
            machine: machine.clone(),
            sink: sink.clone(),
        };
        let (tx, rx) = mpsc::sync_channel(1);
        let thread = thread::spawn(move || {
            STATE.with(|s| *s.borrow_mut() = Some(state));
            // SAFETY: callback has static lifetime; this thread pumps messages until unhooked.
            unsafe {
                let module = GetModuleHandleW(None).map(|m| HINSTANCE(m.0));
                let hook = module
                    .and_then(|m| SetWindowsHookExW(WH_KEYBOARD_LL, Some(callback), Some(m), 0));
                match hook {
                    Ok(hook) => {
                        // Ensure PostThreadMessage can see this thread's queue before publishing it.
                        let mut msg = MSG::default();
                        let _ = PeekMessageW(&mut msg, None, 0, 0, PM_NOREMOVE);
                        let _ = tx.send(Ok(GetCurrentThreadId()));
                        while GetMessageW(&mut msg, None, 0, 0).0 > 0 {
                            let _ = TranslateMessage(&msg);
                            DispatchMessageW(&msg);
                        }
                        let _ = UnhookWindowsHookEx(hook);
                    }
                    Err(error) => {
                        let _ = tx.send(Err(error));
                    }
                }
            }
            STATE.with(|s| s.borrow_mut().take());
        });
        let thread_id = rx
            .recv()
            .map_err(|_| windows::core::Error::from_thread())??;
        Ok(Self {
            machine,
            sink,
            thread_id,
            thread: Some(thread),
        })
    }
    pub fn set_hotkey(&self, key: Hotkey) {
        self.machine
            .lock()
            .expect("hook does not panic")
            .keys
            .set_hotkey(key);
    }
    pub fn force_idle(&self) {
        self.machine
            .lock()
            .expect("hook does not panic")
            .keys
            .force_idle();
    }
    pub fn begin_capture(&self) -> u64 {
        let (event, generation) = {
            let mut control = self.machine.lock().expect("hook does not panic");
            control.generation += 1;
            (control.keys.begin_capture(), control.generation)
        };
        if let Some(event) = event {
            (self.sink)(HookMessage::Dictation(event));
        }
        generation
    }
    pub fn cancel_capture(&self) {
        self.machine
            .lock()
            .expect("hook does not panic")
            .keys
            .cancel_capture();
    }
}
impl Drop for Hook {
    fn drop(&mut self) {
        // SAFETY: the thread id belongs to our live message loop.
        unsafe {
            let _ = PostThreadMessageW(self.thread_id, WM_QUIT, WPARAM(0), LPARAM(0));
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}
unsafe extern "system" fn callback(code: i32, w: WPARAM, l: LPARAM) -> LRESULT {
    if code >= 0
        && matches!(
            w.0 as u32,
            WM_KEYDOWN | WM_SYSKEYDOWN | WM_KEYUP | WM_SYSKEYUP
        )
    {
        // SAFETY: Windows supplies this struct for a low-level keyboard hook event.
        let key = unsafe { &*(l.0 as *const KBDLLHOOKSTRUCT) };
        let result = STATE.with(|s| {
            let state = s.borrow();
            let state = state
                .as_ref()
                .expect("hook state set before hook registration");
            let mut control = state.machine.lock().expect("hook does not panic");
            let outcome = control.keys.process(KeyEvent {
                key: key.vkCode as u8,
                down: matches!(w.0 as u32, WM_KEYDOWN | WM_SYSKEYDOWN),
                injected: key.flags.contains(LLKHF_INJECTED),
            });
            (outcome, control.generation, state.sink.clone())
        });
        let (out, generation, sink) = result;
        if out.mask_start_menu {
            // VK 0xE8 is unassigned; tapping it stops Win's release from opening Start.
            input::send_keys(&[(0xe8, false), (0xe8, true)]);
        }
        if let Some(event) = out.event {
            sink(HookMessage::Dictation(event));
        }
        if let Some(key) = out.capture {
            sink(HookMessage::Captured { generation, key });
        }
        if out.swallow {
            return LRESULT(1);
        }
    }
    // SAFETY: passes the original hook arguments on unchanged.
    unsafe { CallNextHookEx(None, code, w, l) }
}
