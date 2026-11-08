//! Waits for DWM presentation only while the pill needs another frame.
use std::{
    sync::{Arc, Condvar, Mutex},
    thread::{self, JoinHandle},
};
use windows::Win32::{
    Foundation::{HWND, LPARAM, WPARAM},
    Graphics::Dwm::DwmFlush,
    UI::WindowsAndMessaging::{PostMessageW, WM_APP},
};
pub const FRAME: u32 = WM_APP + 2;
#[derive(Default)]
struct State {
    active: bool,
    pending: bool,
    stopped: bool,
}
pub struct FrameClock {
    shared: Arc<(Mutex<State>, Condvar)>,
    worker: Option<JoinHandle<()>>,
}
impl FrameClock {
    pub fn new(hwnd: HWND) -> Self {
        let shared = Arc::new((Mutex::new(State::default()), Condvar::new()));
        let worker_state = shared.clone();
        let handle = hwnd.0 as usize;
        let worker = thread::spawn(move || {
            let (lock, signal) = &*worker_state;
            loop {
                let mut state = lock.lock().expect("frame clock does not panic");
                while !state.stopped && (!state.active || state.pending) {
                    state = signal.wait(state).expect("frame clock does not panic");
                }
                if state.stopped {
                    break;
                }
                drop(state);
                // SAFETY: DwmFlush synchronizes this worker with the compositor without accessing UI state.
                if let Err(error) = unsafe { DwmFlush() } {
                    eprintln!("Pill frame clock: {error}");
                    break;
                }
                let mut state = lock.lock().expect("frame clock does not panic");
                if state.stopped {
                    break;
                }
                if state.active {
                    state.pending = true;
                    // SAFETY: Overlay stops and joins this worker before destroying its window.
                    if unsafe {
                        PostMessageW(Some(HWND(handle as *mut _)), FRAME, WPARAM(0), LPARAM(0))
                    }
                    .is_err()
                    {
                        break;
                    }
                }
            }
        });
        Self {
            shared,
            worker: Some(worker),
        }
    }
    pub fn start(&self) {
        self.shared
            .0
            .lock()
            .expect("frame clock does not panic")
            .active = true;
        self.shared.1.notify_one();
    }
    pub fn stop(&self) {
        self.shared
            .0
            .lock()
            .expect("frame clock does not panic")
            .active = false;
    }
    pub fn presented(&self) {
        self.shared
            .0
            .lock()
            .expect("frame clock does not panic")
            .pending = false;
        self.shared.1.notify_one();
    }
}
impl Drop for FrameClock {
    fn drop(&mut self) {
        self.shared
            .0
            .lock()
            .expect("frame clock does not panic")
            .stopped = true;
        self.shared.1.notify_one();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}
